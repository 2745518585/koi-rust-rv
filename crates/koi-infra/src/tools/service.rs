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
enum ServiceAction {
    Status,
    Logs,
    Start,
    Stop,
    Restart,
    Reload,
    Enable,
    Disable,
    Mask,
    Unmask,
    DaemonReload,
}

struct ServiceTool {
    definition: ToolDefinition,
    action: ServiceAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceArgs {
    service: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    service: String,
    lines: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        ("service.status", "读取 allowlist 服务的当前状态。", ServiceAction::Status, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("service.logs", "读取 allowlist 服务的日志。", ServiceAction::Logs, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("service.start", "启动 allowlist 服务。", ServiceAction::Start, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.stop", "停止 allowlist 服务。", ServiceAction::Stop, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.restart", "重启 allowlist 服务。", ServiceAction::Restart, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.reload", "重新加载 allowlist 服务。", ServiceAction::Reload, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.enable", "设置 allowlist 服务开机启动。", ServiceAction::Enable, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.disable", "取消 allowlist 服务开机启动。", ServiceAction::Disable, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.mask", "屏蔽 allowlist 服务。", ServiceAction::Mask, PermissionLevel::Operator, ToolSideEffect::Destructive, true),
        ("service.unmask", "解除 allowlist 服务屏蔽。", ServiceAction::Unmask, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
        ("service.daemon_reload", "重新加载 systemd 配置。", ServiceAction::DaemonReload, PermissionLevel::Operator, ToolSideEffect::Stateful, true),
    ]
    .into_iter()
    .map(|(name, description, action, permission, side_effect, model_visible)| {
        let schema = if matches!(action, ServiceAction::Logs) {
            json!({"type":"object","required":["service"],"properties":{"service":{"type":"string","minLength":1},"lines":{"type":["integer","null"],"minimum":1,"maximum":2000}},"additionalProperties":false})
        } else if matches!(action, ServiceAction::DaemonReload) {
            json!({"type":"object","additionalProperties":false})
        } else {
            json!({"type":"object","required":["service"],"properties":{"service":{"type":"string","minLength":1}},"additionalProperties":false})
        };
        Arc::new(ServiceTool {
            definition: definition(name, description, schema, permission, side_effect, 60_000, model_visible),
            action,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[async_trait]
impl ToolExecutor for ServiceTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let (service, lines) = if matches!(self.action, ServiceAction::Logs) {
            let args: LogsArgs = super::parse_args(invocation.tool_call.arguments)?;
            let lines = args.lines.unwrap_or(200);
            if lines == 0 {
                return Err(invalid("日志行数必须大于 0"));
            }
            (Some(args.service), Some(lines.min(2_000)))
        } else if matches!(self.action, ServiceAction::DaemonReload) {
            let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
            (None, None)
        } else {
            let args: ServiceArgs = parse_args(invocation.tool_call.arguments)?;
            (Some(args.service), None)
        };
        if let Some(service) = &service {
            self.policy.require_service(service)?;
        }
        let service_name = service.unwrap_or_default();

        let (action, mut args, requires_sudo) = match self.action {
            ServiceAction::Status => ("is-active", vec![service_name.clone()], false),
            ServiceAction::Logs => (
                "",
                vec![
                    "-u".into(),
                    service_name.clone(),
                    "--no-pager".into(),
                    "-n".into(),
                    lines.unwrap_or(200).to_string(),
                ],
                false,
            ),
            ServiceAction::Start => ("start", vec![service_name.clone()], true),
            ServiceAction::Stop => ("stop", vec![service_name.clone()], true),
            ServiceAction::Restart => ("restart", vec![service_name.clone()], true),
            ServiceAction::Reload => ("reload", vec![service_name.clone()], true),
            ServiceAction::Enable => ("enable", vec![service_name.clone()], true),
            ServiceAction::Disable => ("disable", vec![service_name.clone()], true),
            ServiceAction::Mask => ("mask", vec![service_name.clone()], true),
            ServiceAction::Unmask => ("unmask", vec![service_name.clone()], true),
            ServiceAction::DaemonReload => ("daemon-reload", Vec::new(), true),
        };
        let program = if matches!(self.action, ServiceAction::Logs) {
            "journalctl"
        } else {
            "systemctl"
        };
        if requires_sudo {
            self.policy.require_mutation()?;
        }
        if !action.is_empty() {
            args.insert(0, action.into());
        }
        let output = self
            .runner
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
            .await?;
        if requires_sudo {
            ensure_success("服务操作", &output)?;
        }
        Ok(command_result("服务操作", &output))
    }
}
