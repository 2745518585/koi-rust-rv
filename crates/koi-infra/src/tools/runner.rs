use std::process::Stdio;
use std::time::Duration;

use koi_core::domain::{ToolError, ToolErrorKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::policy::ToolPolicy;

#[derive(Clone, Debug)]
pub(crate) struct CommandRunner {
    policy: ToolPolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub requires_sudo: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CommandOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

impl CommandRunner {
    #[must_use]
    pub(crate) fn new(policy: ToolPolicy) -> Self {
        Self { policy }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) async fn run(
        &self,
        spec: CommandSpec,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<CommandOutput, ToolError> {
        if spec.program.trim().is_empty() {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "程序名不能为空",
                false,
            ));
        }
        if spec.program.chars().any(char::is_control)
            || spec
                .args
                .iter()
                .any(|argument| argument.chars().any(char::is_control))
        {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "程序或参数不能包含控制字符",
                false,
            ));
        }
        if spec.args.len() > 256 {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                "命令参数数量不能超过 256 个",
                false,
            ));
        }
        let command_bytes = spec.program.len().saturating_add(
            spec.args
                .iter()
                .map(String::len)
                .fold(0usize, usize::saturating_add),
        );
        if command_bytes > self.policy.max_input_bytes {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!(
                    "命令参数总大小超过 {} 字节限制",
                    self.policy.max_input_bytes
                ),
                false,
            ));
        }
        if spec
            .stdin
            .as_ref()
            .is_some_and(|input| input.len() > self.policy.max_input_bytes)
        {
            return Err(ToolError::new(
                ToolErrorKind::InvalidArguments,
                format!("命令标准输入超过 {} 字节限制", self.policy.max_input_bytes),
                false,
            ));
        }
        let timeout_ms = self.policy.timeout(Some(timeout_ms), timeout_ms)?;
        let mut program = spec.program;
        let mut args = spec.args;
        if spec.requires_sudo && self.policy.use_sudo && cfg!(unix) {
            args.insert(0, program);
            args.insert(0, "-n".into());
            program = "sudo".into();
        }

        let mut command = Command::new(&program);
        command
            .args(&args)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.env_clear();
        for name in &self.policy.environment_allowlist {
            if valid_environment_name(name)
                && let Some(value) = std::env::var_os(name)
            {
                command.env(name, value);
            }
        }
        if let Some(cwd) = spec.cwd {
            command.current_dir(cwd);
        }
        if spec.stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }

        #[cfg(unix)]
        {
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| {
            let kind = if error.kind() == std::io::ErrorKind::NotFound {
                ToolErrorKind::TargetUnavailable
            } else {
                ToolErrorKind::ExecutionFailed
            };
            ToolError::new(kind, format!("无法启动 {program}：{error}"), false)
        })?;
        let child_pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolError::new(ToolErrorKind::Internal, "无法取得命令标准输出", false)
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            ToolError::new(ToolErrorKind::Internal, "无法取得命令标准错误", false)
        })?;
        let stdin = child.stdin.take();
        let input = spec.stdin;
        let max_output = self.policy.max_output_bytes;

        let run = async move {
            let write = async move {
                if let (Some(mut stdin), Some(input)) = (stdin, input) {
                    stdin.write_all(&input).await.map_err(|error| {
                        ToolError::new(
                            ToolErrorKind::ExecutionFailed,
                            format!("写入命令标准输入失败：{error}"),
                            false,
                        )
                    })?;
                    stdin.shutdown().await.map_err(|error| {
                        ToolError::new(
                            ToolErrorKind::ExecutionFailed,
                            format!("关闭命令标准输入失败：{error}"),
                            false,
                        )
                    })?;
                }
                Ok::<(), ToolError>(())
            };
            let read_stdout = read_limited(stdout, max_output);
            let read_stderr = read_limited(stderr, max_output);
            let wait = async {
                child.wait().await.map_err(|error| {
                    ToolError::new(
                        ToolErrorKind::ExecutionFailed,
                        format!("等待命令结束失败：{error}"),
                        false,
                    )
                })
            };
            let (write_result, stdout_result, stderr_result, status_result) =
                tokio::join!(write, read_stdout, read_stderr, wait);
            write_result?;
            let (stdout, stdout_truncated) = stdout_result?;
            let (stderr, stderr_truncated) = stderr_result?;
            let status = status_result?;
            Ok::<CommandOutput, ToolError>(CommandOutput {
                success: status.success(),
                exit_code: status.code(),
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                truncated: stdout_truncated || stderr_truncated,
            })
        };

        tokio::select! {
            () = cancel.cancelled() => {
                terminate_process_tree(child_pid).await;
                Err(ToolError::new(ToolErrorKind::Cancelled, "命令执行已取消", true))
            },
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), run) => {
                if let Ok(result) = result {
                    result
                } else {
                    terminate_process_tree(child_pid).await;
                    Err(ToolError::new(ToolErrorKind::Timeout, format!("命令执行超过 {timeout_ms} 毫秒"), true))
                }
            }
        }
    }
}

fn valid_environment_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('=') && !name.chars().any(char::is_control)
}

async fn terminate_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    #[cfg(unix)]
    {
        let process_group = format!("-{pid}");
        let _ = run_cleanup_command("kill", &["-TERM", &process_group]).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = run_cleanup_command("kill", &["-KILL", &process_group]).await;
    }

    #[cfg(windows)]
    {
        let pid = pid.to_string();
        let _ = run_cleanup_command("taskkill", &["/PID", &pid, "/T", "/F"]).await;
    }
}

async fn run_cleanup_command(program: &str, args: &[&str]) -> bool {
    let command = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    tokio::time::timeout(Duration::from_secs(1), command)
        .await
        .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

async fn read_limited<R>(reader: R, limit: usize) -> Result<(Vec<u8>, bool), ToolError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut reader = reader.take(limit.saturating_add(1) as u64);
    reader.read_to_end(&mut buffer).await.map_err(|error| {
        ToolError::new(
            ToolErrorKind::ExecutionFailed,
            format!("读取命令输出失败：{error}"),
            false,
        )
    })?;
    let truncated = buffer.len() > limit;
    if truncated {
        buffer.truncate(limit);
    }
    Ok((buffer, truncated))
}
