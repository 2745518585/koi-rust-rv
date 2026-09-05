use std::path::Path;
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

use super::policy::{existing_path, new_path};
use super::{
    CommandRunner, CommandSpec, ToolPolicy, command_result, definition, ensure_success, invalid,
    parse_args,
};

#[derive(Clone, Copy)]
enum ArchiveAction {
    List,
    Create,
    Extract,
}

struct ArchiveTool {
    definition: ToolDefinition,
    action: ArchiveAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveArgs {
    archive: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    archive: String,
    source: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractArgs {
    archive: String,
    destination: String,
    #[serde(default)]
    overwrite: bool,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "archive.list",
            "List tar.gz archive contents.",
            ArchiveAction::List,
            PermissionLevel::User,
            ToolSideEffect::ReadOnly,
            true,
        ),
        (
            "archive.create",
            "Create a tar.gz archive in an allowed directory.",
            ArchiveAction::Create,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
        (
            "archive.extract",
            "Extract a tar.gz archive to an allowed directory.",
            ArchiveAction::Extract,
            PermissionLevel::Operator,
            ToolSideEffect::Stateful,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(name, description, action, permission, side_effect, model_visible)| {
            Arc::new(ArchiveTool {
                definition: definition(
                    name,
                    description,
                    schema_for(action),
                    permission,
                    side_effect,
                    120_000,
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

fn schema_for(action: ArchiveAction) -> serde_json::Value {
    match action {
        ArchiveAction::List => {
            json!({"type":"object","required":["archive"],"properties":{"archive":{"type":"string","minLength":1}},"additionalProperties":false})
        }
        ArchiveAction::Create => {
            json!({"type":"object","required":["archive","source"],"properties":{"archive":{"type":"string","minLength":1},"source":{"type":"string","minLength":1},"overwrite":{"type":"boolean"}},"additionalProperties":false})
        }
        ArchiveAction::Extract => {
            json!({"type":"object","required":["archive","destination"],"properties":{"archive":{"type":"string","minLength":1},"destination":{"type":"string","minLength":1},"overwrite":{"type":"boolean"}},"additionalProperties":false})
        }
    }
}

#[async_trait]
impl ToolExecutor for ArchiveTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            ArchiveAction::List => {
                let args: ArchiveArgs = parse_args(invocation.tool_call.arguments)?;
                let archive = existing_path(&self.policy, &args.archive)?;
                if !archive.is_file() {
                    return Err(invalid("Archive must be a file"));
                }
                self.run(
                    vec!["-tzf".into(), archive.to_string_lossy().into_owned()],
                    "Archive listing",
                    cancel,
                    false,
                )
                .await
            }
            ArchiveAction::Create => {
                let args: CreateArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let source = existing_path(&self.policy, &args.source)?;
                let archive = new_path(&self.policy, &args.archive)?;
                if archive == source || (source.is_dir() && archive.starts_with(&source)) {
                    return Err(invalid(
                        "Archive target must not be inside the archive source directory",
                    ));
                }
                if std::fs::symlink_metadata(&archive)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(invalid("Cannot overwrite a symbolic link archive target"));
                }
                if archive.exists() && !args.overwrite {
                    return Err(invalid(
                        "Archive target already exists; explicitly set overwrite=true",
                    ));
                }
                let parent = archive
                    .parent()
                    .ok_or_else(|| invalid("Archive target has no parent directory"))?;
                let source_name = source
                    .file_name()
                    .ok_or_else(|| invalid("Archive source is missing a name"))?;
                self.run(
                    vec![
                        "-czf".into(),
                        archive.to_string_lossy().into_owned(),
                        "-C".into(),
                        parent.to_string_lossy().into_owned(),
                        "--".into(),
                        source_name.to_string_lossy().into_owned(),
                    ],
                    "Create archive",
                    cancel,
                    true,
                )
                .await
            }
            ArchiveAction::Extract => {
                let args: ExtractArgs = parse_args(invocation.tool_call.arguments)?;
                self.extract(args, cancel).await
            }
        }
    }
}

impl ArchiveTool {
    async fn extract(
        &self,
        args: ExtractArgs,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        let archive = existing_path(&self.policy, &args.archive)?;
        let destination = existing_path(&self.policy, &args.destination)?;
        if !archive.is_file() || !destination.is_dir() {
            return Err(invalid(
                "Archive must be a file and destination must be a directory",
            ));
        }
        let listing = self
            .runner
            .run(
                CommandSpec {
                    program: "tar".into(),
                    args: vec!["-tvzf".into(), archive.to_string_lossy().into_owned()],
                    cwd: None,
                    stdin: None,
                    requires_sudo: false,
                },
                self.definition.timeout_ms,
                cancel.clone(),
            )
            .await?;
        ensure_success("Validate archive", &listing)?;
        if listing.truncated {
            return Err(invalid(
                "Archive contents are too large to validate completely before extraction",
            ));
        }
        validate_archive_listing(&listing.stdout)?;
        validate_archive_types(&listing.stdout)?;
        let mut extract_args = vec![
            "-xzf".into(),
            archive.to_string_lossy().into_owned(),
            "-C".into(),
            destination.to_string_lossy().into_owned(),
            "--no-same-owner".into(),
            "--no-same-permissions".into(),
            "--no-overwrite-dir".into(),
        ];
        extract_args.push(if args.overwrite {
            "--overwrite".into()
        } else {
            "--keep-old-files".into()
        });
        extract_args.push("--".into());
        self.run(extract_args, "Extract archive", cancel, true)
            .await
    }

    async fn run(
        &self,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
        check: bool,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(
                CommandSpec {
                    program: "tar".into(),
                    args,
                    cwd: None,
                    stdin: None,
                    requires_sudo: false,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await?;
        if check {
            ensure_success(label, &output)?;
        }
        Ok(command_result(label, &output))
    }
}

fn validate_archive_listing(listing: &str) -> Result<(), ToolError> {
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let (_, entry) = parse_verbose_entry(line)?;
        let path = Path::new(entry);
        let windows_drive_path = entry.as_bytes().get(1).is_some_and(|separator| {
            *separator == b':'
                && entry
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphabetic)
        });
        let platform_neutral_absolute = entry.starts_with('/') || entry.starts_with('\\');
        let parent_component = entry.split(['/', '\\']).any(|component| component == "..");
        if path.is_absolute()
            || windows_drive_path
            || platform_neutral_absolute
            || path
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
            || parent_component
        {
            return Err(invalid(format!(
                "Archive contains an escaping path: {entry}"
            )));
        }
    }
    Ok(())
}

fn validate_archive_types(listing: &str) -> Result<(), ToolError> {
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let (kind, _) = parse_verbose_entry(line)?;
        if !matches!(kind, '-' | 'd') {
            return Err(invalid(format!(
                "Archive contains a disallowed special file type: {kind}"
            )));
        }
    }
    Ok(())
}

fn parse_verbose_entry(line: &str) -> Result<(char, &str), ToolError> {
    let mut rest = line.trim_start();
    let (mode, remainder) = take_listing_field(rest)?;
    rest = remainder;
    for _ in 0..4 {
        let (_, remainder) = take_listing_field(rest)?;
        rest = remainder;
    }
    let name = rest.trim();
    if name.is_empty() {
        return Err(invalid("Unable to parse archive entry name"));
    }
    let kind = mode
        .chars()
        .next()
        .ok_or_else(|| invalid("Unable to parse archive entry type"))?;
    Ok((kind, name))
}

fn take_listing_field(input: &str) -> Result<(&str, &str), ToolError> {
    let input = input.trim_start();
    let boundary = input
        .find(char::is_whitespace)
        .ok_or_else(|| invalid("Unable to parse detailed archive listing"))?;
    Ok((&input[..boundary], &input[boundary..]))
}

#[cfg(test)]
mod tests {
    use super::{validate_archive_listing, validate_archive_types};

    #[test]
    fn archive_paths_and_types_are_restricted() {
        assert!(
            validate_archive_listing(
                "-rw-r--r-- user/group 12 2026-01-01 00:00 service/config.toml\n"
            )
            .is_ok()
        );
        assert!(
            validate_archive_listing("-rw-r--r-- user/group 12 2026-01-01 00:00 ../outside\n")
                .is_err()
        );
        assert!(
            validate_archive_listing("-rw-r--r-- user/group 12 2026-01-01 00:00 /etc/passwd\n")
                .is_err()
        );
        assert!(
            validate_archive_types(
                "-rw-r--r-- user/group 12 2026-01-01 00:00 service/config.toml\n\
             drwxr-xr-x user/group 0 2026-01-01 00:00 service"
            )
            .is_ok()
        );
        assert!(
            validate_archive_types(
                "lrwxrwxrwx user/group 0 2026-01-01 00:00 secret -> /etc/passwd"
            )
            .is_err()
        );
    }
}
