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
        ("firewall.status", "读取防火墙状态或规则。", FirewallAction::Status, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("firewall.port", "通过受限端口规则修改防火墙。", FirewallAction::Port, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("firewall.reload", "重新加载防火墙配置。", FirewallAction::Reload, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
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
                self.run(program, command, "防火墙状态", cancel, false)
                    .await
            }
            FirewallAction::Port => {
                let args: PortArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                if args.port == 0 {
                    return Err(invalid("防火墙端口必须位于 1 到 65535 之间"));
                }
                let protocol = args.protocol.unwrap_or_else(|| "tcp".into());
                if !matches!(protocol.as_str(), "tcp" | "udp") {
                    return Err(invalid("防火墙协议只能是 tcp 或 udp"));
                }
                let allow = match args.action.as_str() {
                    "allow" => true,
                    "deny" => false,
                    _ => return Err(invalid("防火墙动作只能是 allow 或 deny")),
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
                            return Err(invalid("firewalld 端口工具暂不支持 source 条件"));
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
                            "firewall.port 暂不直接修改 nft 规则，请使用受控配置工具",
                        ));
                    }
                };
                let first = self.run_raw(program, command, cancel.clone(), true).await?;
                if matches!(backend(&args.backend)?, Backend::Firewalld) {
                    let second = self
                        .run_raw("firewall-cmd", vec!["--reload".into()], cancel, true)
                        .await?;
                    ensure_success("防火墙规则", &first)?;
                    ensure_success("防火墙重载", &second)?;
                    Ok(ToolResult {
                        summary: "防火墙端口规则已更新".into(),
                        data: json!({
                            "rule": command_result("防火墙规则", &first).data,
                            "reload": command_result("防火墙重载", &second).data,
                        }),
                        truncated: first.truncated || second.truncated,
                    })
                } else {
                    ensure_success("防火墙规则", &first)?;
                    Ok(command_result("防火墙规则", &first))
                }
            }
            FirewallAction::Reload => {
                let args: BackendArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                let (program, command) = match backend(&args.backend)? {
                    Backend::Ufw => ("ufw", vec!["reload".into()]),
                    Backend::Firewalld => ("firewall-cmd", vec!["--reload".into()]),
                    Backend::Nft => return Err(invalid("nft 没有通用 reload 操作")),
                };
                self.run(program, command, "防火墙重载", cancel, true).await
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
        _ => Err(invalid("防火墙 backend 只能是 ufw、firewalld 或 nft")),
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
        return Err(invalid("防火墙 source 无效"));
    }
    Ok(())
}
