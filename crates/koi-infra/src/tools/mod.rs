//! Built-in operational tools.
//!
//! The tools in this module deliberately use structured arguments and fixed
//! command templates. The only general-purpose process entry point is
//! `system.command`, which is Admin-only and disabled by the default policy.

mod archive;
mod command;
mod database;
mod docker;
mod filesystem;
mod firewall;
mod git;
mod http;
mod network;
mod package;
mod policy;
mod process;
mod runner;
mod schedule;
mod service;
mod system;

use std::sync::Arc;

use koi_core::domain::{PermissionLevel, ToolDefinition, ToolError, ToolErrorKind, ToolSideEffect};
use koi_core::ports::{ToolExecutor, ToolRegistrationError, ToolRegistry};

pub use policy::ToolPolicy;
pub(crate) use runner::{CommandOutput, CommandRunner, CommandSpec};

/// Register all built-in operational tools into a core registry.
///
/// # Errors
///
/// Returns an error if a built-in definition is invalid or a name is
/// registered twice.
pub fn register_builtin_tools(
    registry: &mut ToolRegistry,
    policy: ToolPolicy,
) -> Result<usize, ToolRegistrationError> {
    let runner = CommandRunner::new(policy.clone());
    let mut tools: Vec<Arc<dyn ToolExecutor>> = Vec::new();
    tools.extend(filesystem::tools(&policy));
    tools.extend(system::tools(&runner));
    tools.extend(network::tools(&policy, &runner));
    tools.extend(http::tools(&policy));
    tools.extend(database::tools(&policy, &runner));
    tools.extend(service::tools(&policy, &runner));
    tools.extend(git::tools(&policy, &runner));
    tools.extend(docker::tools(&policy, &runner));
    tools.extend(archive::tools(&policy, &runner));
    tools.extend(package::tools(&policy, &runner));
    tools.extend(process::tools(&policy, &runner));
    tools.extend(schedule::tools(&policy, &runner));
    tools.extend(firewall::tools(&policy, &runner));
    tools.push(Arc::new(command::CommandTool::new(policy)));

    let count = tools.len();
    for tool in tools {
        registry.register(tool)?;
    }
    Ok(count)
}

pub(crate) fn definition(
    name: &str,
    description: &str,
    input_schema: serde_json::Value,
    required_permission: PermissionLevel,
    side_effect: ToolSideEffect,
    timeout_ms: u64,
    model_visible: bool,
) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema,
        required_permission,
        side_effect,
        timeout_ms,
        model_visible,
        main_session_only: false,
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::InvalidArguments, message, false)
}

pub(crate) fn internal(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorKind::Internal, message, false)
}

pub(crate) fn parse_args<T>(value: serde_json::Value) -> Result<T, ToolError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(value).map_err(|error| invalid(format!("工具参数无效：{error}")))
}

pub(crate) async fn blocking<T, F>(operation: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| internal(format!("阻塞操作异常结束：{error}")))?
}

pub(crate) fn command_result(label: &str, output: &CommandOutput) -> koi_core::domain::ToolResult {
    let exit_code = output.exit_code;
    let suffix = exit_code.map_or_else(
        || "未取得退出码".to_owned(),
        |code| format!("退出码 {code}"),
    );
    koi_core::domain::ToolResult {
        summary: format!("{label}完成（{suffix}）"),
        data: serde_json::json!({
            "exit_code": exit_code,
            "stdout": redact_text(&output.stdout),
            "stderr": redact_text(&output.stderr),
        }),
        truncated: output.truncated,
    }
}

pub(crate) fn ensure_success(label: &str, output: &CommandOutput) -> Result<(), ToolError> {
    if output.success {
        return Ok(());
    }
    let detail = if output.stderr.trim().is_empty() {
        redact_text(output.stdout.trim())
    } else {
        redact_text(output.stderr.trim())
    };
    Err(ToolError::new(
        ToolErrorKind::ExecutionFailed,
        format!("{label}失败（退出码 {:?}）：{detail}", output.exit_code),
        false,
    ))
}

fn redact_text(text: &str) -> String {
    const SENSITIVE_MARKERS: [&str; 9] = [
        "password",
        "passwd",
        "token",
        "secret",
        "api_key",
        "apikey",
        "authorization",
        "private_key",
        "access_key",
    ];

    text.lines()
        .map(|line| {
            let line = redact_url_userinfo(line);
            let lower = line.to_ascii_lowercase();
            if !SENSITIVE_MARKERS
                .iter()
                .any(|marker| lower.contains(marker))
            {
                return line;
            }
            let (marker_start, marker) = SENSITIVE_MARKERS
                .iter()
                .filter_map(|marker| lower.find(marker).map(|index| (index, *marker)))
                .min_by_key(|(index, _)| *index)
                .unwrap_or((0, ""));
            let marker_end = marker_start + marker.len();
            let delimiter = line[marker_end..]
                .find(['=', ':'])
                .map(|index| marker_end + index);
            delimiter.map_or_else(
                || format!("{}<redacted>", &line[..marker_end]),
                |index| format!("{}<redacted>", &line[..=index]),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_url_userinfo(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        let Some(scheme_end) = rest.find("://") else {
            output.push_str(rest);
            break;
        };
        let authority_start = scheme_end + 3;
        let authority_end = rest[authority_start..]
            .find(|character: char| {
                character.is_whitespace() || matches!(character, '/' | '?' | '#')
            })
            .map_or(rest.len(), |offset| authority_start + offset);
        let authority = &rest[authority_start..authority_end];
        let Some(at) = authority.rfind('@') else {
            output.push_str(&rest[..authority_end]);
            rest = &rest[authority_end..];
            continue;
        };
        let userinfo = &authority[..at];
        if !userinfo.contains(':') {
            output.push_str(&rest[..authority_end]);
            rest = &rest[authority_end..];
            continue;
        }
        output.push_str(&rest[..authority_start]);
        output.push_str("<redacted>@");
        rest = &rest[authority_start + at + 1..];
    }
    output
}

#[cfg(test)]
mod tests {
    use super::redact_text;

    #[test]
    fn command_output_redacts_url_userinfo() {
        assert_eq!(
            redact_text("origin https://user:secret@example.com/repo (fetch)"),
            "origin https://<redacted>@example.com/repo (fetch)"
        );
        assert_eq!(
            redact_text("https://user:p@example.com/repo"),
            "https://<redacted>@example.com/repo"
        );
        assert_eq!(
            redact_text("password=secret\nAuthorization: Bearer token"),
            "password=<redacted>\nAuthorization:<redacted>"
        );
    }
}
