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
    CommandRunner, CommandSpec, ToolPolicy, command_result, definition, invalid, parse_args,
};

pub(crate) struct CommandTool {
    definition: ToolDefinition,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    cwd: Option<String>,
    stdin: Option<String>,
    timeout_ms: Option<u64>,
}

impl CommandTool {
    pub(crate) fn new(policy: ToolPolicy) -> Self {
        let runner = CommandRunner::new(policy.clone());
        Self {
            definition: definition(
                "system.command",
                "以 Admin 权限执行一个任意程序及其参数；默认不对模型可见。",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["program"],
                    "properties": {
                        "program": {"type": "string", "minLength": 1},
                        "args": {"type": "array", "items": {"type": "string"}},
                        "cwd": {"type": ["string", "null"]},
                        "stdin": {"type": ["string", "null"]},
                        "timeout_ms": {"type": ["integer", "null"], "minimum": 1}
                    }
                }),
                PermissionLevel::Admin,
                ToolSideEffect::Destructive,
                120_000,
                false,
            ),
            policy,
            runner,
        }
    }
}

#[async_trait]
impl ToolExecutor for CommandTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.policy.require_admin_commands()?;
        let args: CommandArgs = parse_args(invocation.tool_call.arguments)?;
        if args.program.trim().is_empty() || args.program.chars().any(char::is_control) {
            return Err(invalid("程序名不能为空且不能包含控制字符"));
        }
        if args
            .args
            .iter()
            .chain(args.cwd.iter())
            .chain(args.stdin.iter())
            .any(|value| value.chars().any(char::is_control))
        {
            return Err(invalid("命令参数不能包含控制字符"));
        }
        if let Some(cwd) = &args.cwd {
            let metadata = std::fs::metadata(cwd)
                .map_err(|error| invalid(format!("工作目录不可用：{cwd}：{error}")))?;
            if !metadata.is_dir() {
                return Err(invalid("工作目录必须是目录"));
            }
        }
        if let Some(stdin) = &args.stdin {
            if stdin.len() > self.policy.max_file_bytes {
                return Err(invalid(format!(
                    "标准输入超过 {} 字节限制",
                    self.policy.max_file_bytes
                )));
            }
        }
        let timeout_ms = self
            .policy
            .timeout(args.timeout_ms, self.definition.timeout_ms)?;
        let output = self
            .runner
            .run(
                CommandSpec {
                    program: args.program,
                    args: args.args,
                    cwd: args.cwd.map(Into::into),
                    stdin: args.stdin.map(String::into_bytes),
                    requires_sudo: false,
                },
                timeout_ms,
                cancel,
            )
            .await?;
        Ok(command_result("任意命令", &output))
    }
}
