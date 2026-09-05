use std::sync::Arc;

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizedToolInvocation, PermissionLevel, ToolDefinition, ToolError, ToolResult,
    ToolSideEffect,
};
use koi_core::ports::ToolExecutor;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{
    CommandRunner, CommandSpec, ToolPolicy, command_result, definition, ensure_success, invalid,
    parse_args,
};

#[derive(Clone, Copy)]
enum PackageAction {
    List,
    Search,
    Install,
    Upgrade,
    Remove,
}

#[derive(Clone, Copy)]
enum PackageManager {
    Apt,
    Dnf,
    Apk,
}

struct PackageTool {
    definition: ToolDefinition,
    action: PackageAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalNameArgs {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    query: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagesArgs {
    packages: Vec<String>,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "package.list",
            "Read installed package information.",
            PackageAction::List,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "package.search",
            "Search packages.",
            PackageAction::Search,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "package.install",
            "Install packages.",
            PackageAction::Install,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "package.upgrade",
            "Upgrade packages.",
            PackageAction::Upgrade,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "package.remove",
            "Remove packages.",
            PackageAction::Remove,
            PermissionLevel::Operator,
            ToolSideEffect::Destructive,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(name, description, action, permission, side_effect, model_visible)| {
            Arc::new(PackageTool {
                definition: definition(
                    name,
                    description,
                    schema_for(action),
                    permission,
                    side_effect,
                    300_000,
                    model_visible,
                ),
                action,
                policy: policy.clone(),
                runner: runner.clone(),
            }) as Arc<dyn ToolExecutor>
        },
    )
    .collect()
}

fn schema_for(action: PackageAction) -> serde_json::Value {
    match action {
        PackageAction::List => {
            json!({"type":"object","properties":{"name":{"type":["string","null"],"maxLength":512}},"additionalProperties":false})
        }
        PackageAction::Search => {
            json!({"type":"object","required":["query"],"properties":{"query":{"type":"string","minLength":1,"maxLength":512}},"additionalProperties":false})
        }
        PackageAction::Install | PackageAction::Upgrade | PackageAction::Remove => {
            json!({"type":"object","required":["packages"],"properties":{"packages":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"string","minLength":1,"maxLength":256}}},"additionalProperties":false})
        }
    }
}

#[async_trait]
impl ToolExecutor for PackageTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let manager = detect_manager()?;
        let (program, args, requires_sudo) = match self.action {
            PackageAction::List => {
                let args: OptionalNameArgs = parse_args(invocation.tool_call.arguments)?;
                let name = args.name.unwrap_or_default();
                if !name.is_empty() {
                    if name.len() > 512 {
                        return Err(invalid("Package name must not exceed 512 bytes"));
                    }
                    validate_package(&name)?;
                }
                package_command(manager, PackageAction::List, vec![name])
            }
            PackageAction::Search => {
                let args: QueryArgs = parse_args(invocation.tool_call.arguments)?;
                if args.query.len() > 512 {
                    return Err(invalid("Package search query must not exceed 512 bytes"));
                }
                validate_package(&args.query)?;
                package_command(manager, PackageAction::Search, vec![args.query])
            }
            PackageAction::Install | PackageAction::Upgrade | PackageAction::Remove => {
                let args: PackagesArgs = parse_args(invocation.tool_call.arguments)?;
                if args.packages.is_empty() {
                    return Err(invalid("At least one package name is required"));
                }
                if args.packages.len() > 100 {
                    return Err(invalid("At most 100 packages may be processed at once"));
                }
                for package in &args.packages {
                    validate_package(package)?;
                }
                self.policy.require_mutation()?;
                package_command(manager, self.action, args.packages)
            }
        };
        let output = self
            .runner
            .run(
                CommandSpec {
                    program,
                    args,
                    cwd: None,
                    stdin: None,
                    requires_sudo,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await?;
        if requires_sudo {
            ensure_success("Package operation", &output)?;
        }
        Ok(command_result("Package operation", &output))
    }
}

fn detect_manager() -> Result<PackageManager, ToolError> {
    if program_on_path("apt-get") {
        return Ok(PackageManager::Apt);
    }
    if program_on_path("dnf") {
        return Ok(PackageManager::Dnf);
    }
    if program_on_path("apk") {
        return Ok(PackageManager::Apk);
    }
    Err(ToolError::new(
        koi_core::domain::ToolErrorKind::TargetUnavailable,
        "No supported package manager was detected (apt, dnf, or apk)",
        false,
    ))
}

fn program_on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(program);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

#[allow(clippy::too_many_lines)]
fn package_command(
    manager: PackageManager,
    action: PackageAction,
    values: Vec<String>,
) -> (String, Vec<String>, bool) {
    match (manager, action) {
        (PackageManager::Apt, PackageAction::List) => {
            if values[0].is_empty() {
                (
                    "apt".into(),
                    vec!["list".into(), "--installed".into()],
                    false,
                )
            } else {
                (
                    "apt-cache".into(),
                    vec!["policy".into(), values[0].clone()],
                    false,
                )
            }
        }
        (PackageManager::Dnf, PackageAction::List) => (
            "dnf".into(),
            std::iter::once("list".into())
                .chain(std::iter::once("installed".into()))
                .chain(values.into_iter().filter(|value| !value.is_empty()))
                .collect(),
            false,
        ),
        (PackageManager::Apk, PackageAction::List) => (
            "apk".into(),
            std::iter::once("info".into())
                .chain(values.into_iter().filter(|value| !value.is_empty()))
                .collect(),
            false,
        ),
        (PackageManager::Apt, PackageAction::Search) => (
            "apt-cache".into(),
            vec!["search".into(), values[0].clone()],
            false,
        ),
        (PackageManager::Dnf, PackageAction::Search) => (
            "dnf".into(),
            vec!["search".into(), values[0].clone()],
            false,
        ),
        (PackageManager::Apk, PackageAction::Search) => (
            "apk".into(),
            vec!["search".into(), values[0].clone()],
            false,
        ),
        (PackageManager::Apt, PackageAction::Install) => (
            "apt-get".into(),
            std::iter::once("install".into())
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Apt, PackageAction::Upgrade) => (
            "apt-get".into(),
            std::iter::once("install".into())
                .chain(std::iter::once("--only-upgrade".into()))
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Apt, PackageAction::Remove) => (
            "apt-get".into(),
            std::iter::once("remove".into())
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Dnf, PackageAction::Install) => (
            "dnf".into(),
            std::iter::once("install".into())
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Dnf, PackageAction::Upgrade) => (
            "dnf".into(),
            std::iter::once("upgrade".into())
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Dnf, PackageAction::Remove) => (
            "dnf".into(),
            std::iter::once("remove".into())
                .chain(std::iter::once("-y".into()))
                .chain(values)
                .collect(),
            true,
        ),
        (PackageManager::Apk, PackageAction::Install) => (
            "apk".into(),
            std::iter::once("add".into()).chain(values).collect(),
            true,
        ),
        (PackageManager::Apk, PackageAction::Upgrade) => (
            "apk".into(),
            std::iter::once("upgrade".into()).chain(values).collect(),
            true,
        ),
        (PackageManager::Apk, PackageAction::Remove) => (
            "apk".into(),
            std::iter::once("del".into()).chain(values).collect(),
            true,
        ),
    }
}

fn validate_package(value: &str) -> Result<(), ToolError> {
    if value.trim().is_empty()
        || value.starts_with('-')
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid(format!("Invalid package name: {value}")));
    }
    Ok(())
}
