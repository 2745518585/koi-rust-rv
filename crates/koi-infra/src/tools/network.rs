use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizedToolInvocation, PermissionLevel, ToolDefinition, ToolError, ToolErrorKind,
    ToolResult, ToolSideEffect,
};
use koi_core::ports::ToolExecutor;
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::{
    CommandRunner, CommandSpec, command_result, definition, ensure_success, invalid, parse_args,
};

#[derive(Clone, Copy)]
enum NetworkAction {
    Interfaces,
    Routes,
    Connections,
    DnsLookup,
    PortCheck,
    TlsCheck,
}

struct NetworkTool {
    definition: ToolDefinition,
    action: NetworkAction,
    policy: super::ToolPolicy,
    runner: CommandRunner,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostArgs {
    host: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPortArgs {
    host: String,
    port: u16,
    timeout_ms: Option<u64>,
}

pub(crate) fn tools(
    policy: &super::ToolPolicy,
    runner: &CommandRunner,
) -> Vec<Arc<dyn ToolExecutor>> {
    [
        ("network.interfaces", "Read network interface information.", NetworkAction::Interfaces),
        ("network.routes", "Read network routing information.", NetworkAction::Routes),
        ("network.connections", "Read active network connections and listeners.", NetworkAction::Connections),
        ("network.dns_lookup", "Query DNS resolution results.", NetworkAction::DnsLookup),
        ("network.port_check", "Check whether a target TCP port is reachable.", NetworkAction::PortCheck),
        ("network.tls_check", "Check TLS handshake information for a target service.", NetworkAction::TlsCheck),
    ]
    .into_iter()
    .map(|(name, description, action)| {
        let schema = match action {
            NetworkAction::Interfaces | NetworkAction::Routes | NetworkAction::Connections => {
                json!({"type":"object","additionalProperties":false})
            }
            NetworkAction::DnsLookup => json!({"type":"object","required":["host"],"properties":{"host":{"type":"string","minLength":1}},"additionalProperties":false}),
            NetworkAction::PortCheck | NetworkAction::TlsCheck => json!({"type":"object","required":["host","port"],"properties":{"host":{"type":"string","minLength":1},"port":{"type":"integer","minimum":1,"maximum":65535},"timeout_ms":{"type":["integer","null"],"minimum":1}},"additionalProperties":false}),
        };
        Arc::new(NetworkTool {
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
            policy: policy.clone(),
            runner: runner.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

#[async_trait]
impl ToolExecutor for NetworkTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            NetworkAction::Interfaces => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run(
                    "Network interfaces",
                    "ip",
                    vec!["-brief".into(), "address".into()],
                    cancel,
                )
                .await
            }
            NetworkAction::Routes => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run("Network routes", "ip", vec!["route".into()], cancel)
                    .await
            }
            NetworkAction::Connections => {
                let _: EmptyArgs = parse_args(invocation.tool_call.arguments)?;
                self.run("Network connections", "ss", vec!["-tunap".into()], cancel)
                    .await
            }
            NetworkAction::DnsLookup => {
                let args: HostArgs = parse_args(invocation.tool_call.arguments)?;
                validate_host(&args.host)?;
                self.policy.require_network_host(&args.host)?;
                self.run(
                    "DNS lookup",
                    "dig",
                    vec!["+short".into(), args.host],
                    cancel,
                )
                .await
            }
            NetworkAction::PortCheck => {
                let args: HostPortArgs = parse_args(invocation.tool_call.arguments)?;
                validate_port(args.port)?;
                validate_host(&args.host)?;
                self.policy.require_network_host(&args.host)?;
                self.port_check(args, cancel).await
            }
            NetworkAction::TlsCheck => {
                let args: HostPortArgs = parse_args(invocation.tool_call.arguments)?;
                validate_host(&args.host)?;
                validate_port(args.port)?;
                self.policy.require_network_host(&args.host)?;
                let timeout_ms = self
                    .policy
                    .timeout(args.timeout_ms, self.definition.timeout_ms)?;
                self.run_with_timeout(
                    "TLS check",
                    CommandSpec {
                        program: "openssl".into(),
                        args: vec![
                            "s_client".into(),
                            "-connect".into(),
                            format_endpoint(&args.host, args.port),
                            "-servername".into(),
                            args.host,
                            "-brief".into(),
                        ],
                        cwd: None,
                        stdin: None,
                        requires_sudo: false,
                    },
                    timeout_ms,
                    cancel,
                )
                .await
            }
        }
    }
}

impl NetworkTool {
    async fn run(
        &self,
        label: &str,
        program: &str,
        args: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.run_with_timeout(
            label,
            CommandSpec {
                program: program.into(),
                args,
                cwd: None,
                stdin: None,
                requires_sudo: false,
            },
            self.definition.timeout_ms,
            cancel,
        )
        .await
    }

    async fn run_with_timeout(
        &self,
        label: &str,
        spec: CommandSpec,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let output = self.runner.run(spec, timeout_ms, cancel).await?;
        ensure_success(label, &output)?;
        Ok(command_result(label, &output))
    }

    async fn port_check(
        &self,
        args: HostPortArgs,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        validate_host(&args.host)?;
        validate_port(args.port)?;
        let timeout_ms = self
            .policy
            .timeout(args.timeout_ms, self.definition.timeout_ms)?;
        let host = args.host;
        let port = args.port;
        let connect = TcpStream::connect((host.as_str(), port));
        tokio::select! {
            () = cancel.cancelled() => Err(ToolError::new(ToolErrorKind::Cancelled, "Port check was cancelled", true)),
            result = tokio::time::timeout(Duration::from_millis(timeout_ms), connect) => {
                match result {
                    Ok(Ok(_stream)) => Ok(ToolResult {
                        summary: format!("{host}:{port} is reachable"),
                        data: json!({"host": host, "port": port, "reachable": true}),
                        truncated: false,
                    }),
                    Ok(Err(error)) => Err(ToolError::new(ToolErrorKind::TargetUnavailable, format!("{host}:{port} is unreachable: {error}"), true)),
                    Err(_) => Err(ToolError::new(ToolErrorKind::Timeout, format!("Connection to {host}:{port} exceeded {timeout_ms} milliseconds"), true)),
                }
            }
        }
    }
}

fn validate_host(host: &str) -> Result<(), ToolError> {
    if host.trim().is_empty()
        || host.starts_with('-')
        || host.starts_with('@')
        || host.len() > 253
        || host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid(
            "Host name must not be empty or contain whitespace or control characters",
        ));
    }
    Ok(())
}

fn validate_port(port: u16) -> Result<(), ToolError> {
    if port == 0 {
        return Err(invalid("Port must be between 1 and 65535"));
    }
    Ok(())
}

fn format_endpoint(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::{format_endpoint, validate_host};
    use crate::tools::ToolPolicy;

    #[test]
    fn active_network_probes_are_fail_closed_and_support_ipv6() {
        assert!(validate_host("@8.8.8.8").is_err());
        assert!(format_endpoint("2001:db8::1", 443).starts_with('['));

        let policy = ToolPolicy::default();
        assert!(policy.require_network_host("example.com").is_err());
        let policy = policy.with_allowed_network_host("Example.com");
        assert!(policy.require_network_host("example.com").is_ok());
    }
}
