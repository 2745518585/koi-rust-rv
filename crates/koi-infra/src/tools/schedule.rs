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
enum ScheduleAction {
    List,
    Timers,
    Install,
    Clear,
}

struct ScheduleTool {
    definition: ToolDefinition,
    action: ScheduleAction,
    policy: ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentArgs {
    content: String,
}

pub(crate) fn tools(policy: &ToolPolicy, runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        ("schedule.list", "读取当前用户的 crontab。", ScheduleAction::List, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("schedule.timers", "读取 systemd 定时器。", ScheduleAction::Timers, PermissionLevel::User, ToolSideEffect::ReadOnly, true),
        ("schedule.install", "替换当前用户的 crontab；原内容不会自动合并。", ScheduleAction::Install, PermissionLevel::Operator, ToolSideEffect::Destructive, true),
        ("schedule.clear", "删除当前用户的 crontab。", ScheduleAction::Clear, PermissionLevel::Operator, ToolSideEffect::Destructive, true),
    ]
    .into_iter()
    .map(|(name, description, action, permission, side_effect, model_visible)| {
        let schema = if matches!(action, ScheduleAction::Install) {
            json!({"type":"object","required":["content"],"properties":{"content":{"type":"string","minLength":1}},"additionalProperties":false})
        } else {
            json!({"type":"object","additionalProperties":false})
        };
        Arc::new(ScheduleTool {
            definition: definition(name, description, schema, permission, side_effect, 60_000, model_visible),
            action,
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[async_trait]
impl ToolExecutor for ScheduleTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            ScheduleAction::List => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    CommandSpec {
                        program: "crontab".into(),
                        args: vec!["-l".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    "读取 crontab",
                    cancel,
                    false,
                )
                .await
            }
            ScheduleAction::Timers => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    CommandSpec {
                        program: "systemctl".into(),
                        args: vec!["list-timers".into(), "--all".into(), "--no-pager".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    "读取 systemd 定时器",
                    cancel,
                    false,
                )
                .await
            }
            ScheduleAction::Install => {
                let args: ContentArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                if args.content.trim().is_empty() {
                    return Err(invalid(
                        "crontab 内容不能为空；清空任务请使用 schedule.clear",
                    ));
                }
                validate_content(&args.content, self.policy.max_file_bytes)?;
                self.run(
                    CommandSpec {
                        program: "crontab".into(),
                        args: vec!["-".into()],
                        cwd: None,
                        stdin: Some(args.content.into_bytes()),
                        requires_sudo: false,
                    },
                    "安装 crontab",
                    cancel,
                    true,
                )
                .await
            }
            ScheduleAction::Clear => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.policy.require_mutation()?;
                self.run(
                    CommandSpec {
                        program: "crontab".into(),
                        args: vec!["-r".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    "清理 crontab",
                    cancel,
                    true,
                )
                .await
            }
        }
    }
}

impl ScheduleTool {
    async fn run(
        &self,
        spec: CommandSpec,
        label: &str,
        cancel: CancellationToken,
        check: bool,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(spec, self.definition.timeout_ms, cancel)
            .await?;
        if check {
            ensure_success(label, &output)?;
        }
        Ok(command_result(label, &output))
    }
}

fn validate_content(content: &str, max_bytes: usize) -> Result<(), ToolError> {
    if content.len() > max_bytes {
        return Err(invalid(format!("crontab 内容超过 {max_bytes} 字节限制")));
    }
    if content
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(invalid("crontab 内容包含不允许的控制字符"));
    }
    Ok(())
}
