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
enum ProcessAction {
    Signal,
    Renice,
}

struct ProcessTool {
    definition: ToolDefinition,
    action: ProcessAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalArgs {
    pid: u32,
    signal: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReniceArgs {
    pid: u32,
    priority: i32,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    vec![
        Arc::new(ProcessTool {
            definition: definition(
                "process.signal",
                "向指定进程发送受限信号。",
                json!({"type":"object","required":["pid"],"properties":{"pid":{"type":"integer","minimum":2},"signal":{"type":["string","null"],"enum":["TERM","INT","HUP","KILL","STOP","CONT",null]}},"additionalProperties":false}),
                PermissionLevel::Operator,
                ToolSideEffect::Destructive,
                30_000,
                true,
            ),
            action: ProcessAction::Signal,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>,
        Arc::new(ProcessTool {
            definition: definition(
                "process.renice",
                "调整指定进程的调度优先级。",
                json!({"type":"object","required":["pid","priority"],"properties":{"pid":{"type":"integer","minimum":2},"priority":{"type":"integer","minimum":-20,"maximum":19}},"additionalProperties":false}),
                PermissionLevel::Operator,
                ToolSideEffect::Stateful,
                30_000,
                true,
            ),
            action: ProcessAction::Renice,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>,
    ]
}

#[async_trait]
impl ToolExecutor for ProcessTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.policy.require_mutation()?;
        match self.action {
            ProcessAction::Signal => {
                let args: SignalArgs = parse_args(invocation.tool_call.arguments)?;
                if args.pid <= 1 {
                    return Err(invalid("不能操作 PID 1 或更低的进程"));
                }
                let signal = args.signal.unwrap_or_else(|| "TERM".into());
                if !matches!(
                    signal.as_str(),
                    "TERM" | "INT" | "HUP" | "KILL" | "STOP" | "CONT"
                ) {
                    return Err(invalid("不支持的进程信号"));
                }
                self.run(
                    vec![format!("-{signal}"), args.pid.to_string()],
                    "进程信号",
                    cancel,
                )
                .await
            }
            ProcessAction::Renice => {
                let args: ReniceArgs = parse_args(invocation.tool_call.arguments)?;
                if args.pid <= 1 {
                    return Err(invalid("不能操作 PID 1 或更低的进程"));
                }
                if !(-20..=19).contains(&args.priority) {
                    return Err(invalid("进程优先级必须位于 -20 到 19 之间"));
                }
                self.run(
                    vec![
                        "-n".into(),
                        args.priority.to_string(),
                        "-p".into(),
                        args.pid.to_string(),
                    ],
                    "进程优先级",
                    cancel,
                )
                .await
            }
        }
    }
}

impl ProcessTool {
    async fn run(
        &self,
        args: Vec<String>,
        label: &str,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let program = if matches!(self.action, ProcessAction::Signal) {
            "kill"
        } else {
            "renice"
        };
        let output = self
            .runner
            .run(
                CommandSpec {
                    program: program.into(),
                    args,
                    cwd: None,
                    stdin: None,
                    requires_sudo: true,
                },
                self.definition.timeout_ms,
                cancel,
            )
            .await?;
        ensure_success(label, &output)?;
        Ok(command_result(label, &output))
    }
}
