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
enum FirewallAction {
    Status,
    Port,
    Reload,
}

struct FirewallTool {
    definition: ToolDefinition,
    action: FirewallAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackendArgs {
    backend: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortArgs {
    backend: String,
    action: String,
    port: u16,
    protocol: Option<String>,
    source: Option<String>,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        ("firewall.status", "Read firewall status or rules.", FirewallAction::Status, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("firewall.port", "Modify the firewall through restricted port rules.", FirewallAction::Port, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("firewall.reload", "Reload the firewall configuration.", FirewallAction::Reload, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
    ]
    .into_iter()
    .map(|(name, description, action, permission, side_effect, model_visible)| {
        let schema = match action {
            FirewallAction::Status | FirewallAction::Reload => json!({"type":"object","required":["backend"],"properties":{"backend":{"type":"string","enum":["ufw","firewalld","nft"]}},"additionalProperties":false}),
            FirewallAction::Port => json!({"type":"object","required":["backend","action","port"],"properties":{"backend":{"type":"string","enum":["ufw","firewalld"]},"action":{"type":"string","enum":["allow","deny"]},"port":{"type":"integer","minimum":1,"maximum":65535},"protocol":{"type":["string","null"],"enum":["tcp","udp",null]},"source":{"type":["string","null"]}},"additionalProperties":false}),
        };
        Arc::new(FirewallTool {
            definition: definition(name, description, schema, permission, side_effect, 60_000, model_visible),
            action,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[allow(clippy::too_many_lines)]
#[async_trait]
impl ToolExecutor for FirewallTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            FirewallAction::Status => {
                let args: BackendArgs = parse_args(invocation.tool_call.arguments)?;
                let (program, command) = match backend(&args.backend)? {
                    Backend::Ufw => ("ufw", vec!["status".into(), "verbose".into()]),
                    Backend::Firewalld => ("firewall-cmd", vec!["--list-all".into()]),
                    Backend::Nft => ("nft", vec!["list".into(), "ruleset".into()]),
                };
                self.run(program, command, "Firewall status", cancel, false)
                    .await
            }
            FirewallAction::Port => {
                let args: PortArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                if args.port == 0 {
                    return Err(invalid("Firewall port must be between 1 and 65535"));
                }
                let protocol = args.protocol.unwrap_or_else(|| "tcp".into());
                if !matches!(protocol.as_str(), "tcp" | "udp") {
                    return Err(invalid("Firewall protocol must be tcp or udp"));
                }
                let allow = match args.action.as_str() {
                    "allow" => true,
                    "deny" => false,
                    _ => return Err(invalid("Firewall action must be allow or deny")),
                };
                let (program, command) = match backend(&args.backend)? {
                    Backend::Ufw => {
                        if let Some(source) = args.source.as_deref() {
                            validate_source(source)?;
                        }
                        let mut command = vec![if allow { "allow" } else { "deny" }.into()];
                        if let Some(source) = args.source {
                            command.extend([
                                "from".into(),
                                source,
                                "to".into(),
                                "any".into(),
                                "port".into(),
                                args.port.to_string(),
                                "proto".into(),
                                protocol,
                            ]);
                        } else {
                            command.push(format!("{}/{}", args.port, protocol));
                        }
                        ("ufw", command)
                    }
                    Backend::Firewalld => {
                        if args.source.is_some() {
                            return Err(invalid(
                                "The firewalld port tool does not support source conditions",
                            ));
                        }
                        (
                            "firewall-cmd",
                            vec![
                                "--permanent".into(),
                                format!(
                                    "--{}-port={}/{}",
                                    if allow { "add" } else { "remove" },
                                    args.port,
                                    protocol
                                ),
                            ],
                        )
                    }
                    Backend::Nft => {
                        return Err(invalid(
                            "firewall.port does not modify nft rules directly; use a controlled configuration tool instead",
                        ));
                    }
                };
                let first = self.run_raw(program, command, cancel.clone(), true).await?;
                if matches!(backend(&args.backend)?, Backend::Firewalld) {
                    let second = self
                        .run_raw("firewall-cmd", vec!["--reload".into()], cancel, true)
                        .await?;
                    ensure_success("Firewall rule", &first)?;
                    ensure_success("Firewall reload", &second)?;
                    Ok(ToolResult {
                        summary: "Firewall port rule updated".into(),
                        data: json!({
                            "rule": command_result("Firewall rule", &first).data,
                            "reload": command_result("Firewall reload", &second).data,
                        }),
                        truncated: first.truncated || second.truncated,
                    })
                } else {
                    ensure_success("Firewall rule", &first)?;
                    Ok(command_result("Firewall rule", &first))
                }
            }
            FirewallAction::Reload => {
                let args: BackendArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let (program, command) = match backend(&args.backend)? {
                    Backend::Ufw => ("ufw", vec!["reload".into()]),
                    Backend::Firewalld => ("firewall-cmd", vec!["--reload".into()]),
                    Backend::Nft => return Err(invalid("nft has no generic reload operation")),
                };
                self.run(program, command, "Firewall reload", cancel, true)
                    .await
            }
        }
    }
}

impl FirewallTool {
    async fn run(
        &self,
        program: &str,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
        check: bool,
    ) -> Result<ToolResult, ToolError> {
        let output = self.run_raw(program, args, cancel, check).await?;
        if check {
            ensure_success(label, &output)?;
        }
        Ok(command_result(label, &output))
    }

    async fn run_raw(
        &self,
        program: &str,
        args: Vec<String>,
        cancel: CancellationToken,
        requires_sudo: bool,
    ) -> Result<super::CommandOutput, ToolError> {
        self.runner
            .run(
                CommandSpec {
                    program: program.into(),
                    args,
                    cwd: None,
                    stdin: None,
                    requires_sudo,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Ufw,
    Firewalld,
    Nft,
}

fn backend(value: &str) -> Result<Backend, ToolError> {
    match value {
        "ufw" => Ok(Backend::Ufw),
        "firewalld" => Ok(Backend::Firewalld),
        "nft" => Ok(Backend::Nft),
        _ => Err(invalid("Firewall backend must be ufw, firewalld, or nft")),
    }
}

fn validate_source(source: &str) -> Result<(), ToolError> {
    if source.trim().is_empty()
        || source.starts_with('-')
        || source.len() > 128
        || source
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid("Invalid firewall source"));
    }
    Ok(())
}
