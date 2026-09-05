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

use super::{CommandRunner, CommandSpec, command_result, definition, ensure_success, parse_args};

#[derive(Clone, Copy)]
enum SystemAction {
    Info,
    Resources,
    Processes,
    Filesystems,
    Logs,
    KernelMessages,
}

struct SystemTool {
    definition: ToolDefinition,
    action: SystemAction,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitArgs {
    limit: Option<u32>,
}

pub(crate) fn tools(runner: &CommandRunner) -> Vec<Arc<dyn ToolExecutor>> {
    [
        ("system.info", "Read host, kernel, and uptime information.", SystemAction::Info, "{}"),
        ("system.resources", "Read CPU, memory, and base resource information.", SystemAction::Resources, "{}"),
        ("system.processes", "Read the current process list.", SystemAction::Processes, "limit"),
        ("system.filesystems", "Read filesystem and mount usage information.", SystemAction::Filesystems, "{}"),
        ("system.logs", "Read system logs.", SystemAction::Logs, "limit"),
        ("system.kernel_messages", "Read warnings and errors from kernel messages.", SystemAction::KernelMessages, "{}"),
    ]
    .into_iter()
    .map(|(name, description, action, schema_kind)| {
        let schema = if schema_kind == "limit" {
            json!({"type":"object","properties":{"limit":{"type":["integer","null"],"minimum":1,"maximum":1000}},"additionalProperties":false})
        } else {
            json!({"type":"object","additionalProperties":false})
        };
        Arc::new(SystemTool {
            definition: definition(
                name,
                description,
                schema,
                PermissionLevel::User,
                ToolSideEffect::ReadOnly,
                30_000,
                true,
            ),
            action,
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[async_trait]
impl ToolExecutor for SystemTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            SystemAction::Info => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    "System information",
                    CommandSpec {
                        program: "uname".into(),
                        args: vec!["-a".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    cancel,
                )
                .await
            }
            SystemAction::Resources => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    "System resources",
                    CommandSpec {
                        program: "free".into(),
                        args: vec!["-h".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    cancel,
                )
                .await
            }
            SystemAction::Processes => {
                let args: LimitArgs = parse_args(invocation.tool_call.arguments)?;
                let limit = limit_value(args.limit, 100)?;
                let result = self
                    .run(
                        "Process list",
                        CommandSpec {
                            program: "ps".into(),
                            args: vec![
                                "-eo".into(),
                                "pid,ppid,user,stat,%cpu,%mem,comm,args".into(),
                                "--sort=-%cpu".into(),
                            ],
                            cwd: None,
                            stdin: None,
                            requires_sudo: false,
                        },
                        cancel,
                    )
                    .await?;
                Ok(limit_stdout(result, limit))
            }
            SystemAction::Filesystems => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    "Filesystem information",
                    CommandSpec {
                        program: "df".into(),
                        args: vec!["-hT".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    cancel,
                )
                .await
            }
            SystemAction::Logs => {
                let args: LimitArgs = parse_args(invocation.tool_call.arguments)?;
                let limit = limit_value(args.limit, 200)?;
                self.run(
                    "System logs",
                    CommandSpec {
                        program: "journalctl".into(),
                        args: vec!["--no-pager".into(), "-n".into(), limit.to_string()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    cancel,
                )
                .await
            }
            SystemAction::KernelMessages => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    "Kernel messages",
                    CommandSpec {
                        program: "dmesg".into(),
                        args: vec!["--ctime".into(), "--level=err,warn".into()],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    cancel,
                )
                .await
            }
        }
    }
}

fn limit_value(value: Option<u32>, default: u32) -> Result<usize, ToolError> {
    let limit = value.unwrap_or(default);
    if limit == 0 {
        return Err(super::invalid("limit must be greater than zero"));
    }
    Ok(limit.min(1_000) as usize)
}

fn limit_stdout(mut result: ToolResult, limit: usize) -> ToolResult {
    let Some(stdout) = result.data.get("stdout").and_then(|value| value.as_str()) else {
        return result;
    };
    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() <= limit.saturating_add(1) {
        return result;
    }
    let kept = lines[..limit.saturating_add(1)].join("\n");
    result.data["stdout"] = json!(kept);
    result.truncated = true;
    result
}

impl SystemTool {
    async fn run(
        &self,
        label: &str,
        spec: CommandSpec,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let output = self
            .runner
            .run(spec, self.definition.timeout_ms, cancel)
            .await?;
        ensure_success(label, &output)?;
        Ok(command_result(label, &output))
    }
}

#[cfg(test)]
mod tests {
    use super::{limit_stdout, limit_value};
    use koi_core::domain::ToolResult;
    use serde_json::json;

    #[test]
    fn process_limit_is_applied_to_command_output() {
        let result = limit_stdout(
            ToolResult {
                summary: "Process list".into(),
                data: json!({"stdout":"header\nfirst\nsecond\nthird\n"}),
                truncated: false,
            },
            2,
        );
        assert_eq!(result.data["stdout"], "header\nfirst\nsecond");
        assert!(result.truncated);
    }

    #[test]
    fn zero_process_limit_is_rejected() {
        assert!(limit_value(Some(0), 100).is_err());
    }
}
