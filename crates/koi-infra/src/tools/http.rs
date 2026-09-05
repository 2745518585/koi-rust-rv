use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use koi_core::domain::{
    AuthorizedToolInvocation, PermissionLevel, ToolDefinition, ToolError, ToolErrorKind,
    ToolResult, ToolSideEffect,
};
use koi_core::ports::ToolExecutor;
use reqwest::{Client, Method, Url};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{ToolPolicy, definition, invalid, parse_args};

#[derive(Clone, Copy)]
enum HttpAction {
    Get,
    Request,
}

struct HttpTool {
    definition: ToolDefinition,
    action: HttpAction,
    policy: ToolPolicy,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    url: String,
    headers: Option<BTreeMap<String, String>>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestArgs {
    url: String,
    method: String,
    headers: Option<BTreeMap<String, String>>,
    body: Option<serde_json::Value>,
    timeout_ms: Option<u64>,
}

pub(crate) fn tools(policy: &ToolPolicy) -> Vec<Arc<dyn ToolExecutor>> {
    [
        (
            "http.get",
            "Send a read-only GET request to a policy-allowed HTTP host.",
            HttpAction::Get,
        ),
        (
            "curl.get",
            "Send a restricted read-only curl-style GET request with structured arguments.",
            HttpAction::Get,
        ),
        (
            "http.request",
            "Send a restricted mutating request to a policy-allowed HTTP host.",
            HttpAction::Request,
        ),
        (
            "curl.request",
            "Send a restricted mutating curl-style request with structured arguments.",
            HttpAction::Request,
        ),
    ]
    .into_iter()
    .map(|(name, description, action)| {
        let (permission, side_effect) = match action {
            HttpAction::Get => (PermissionLevel::User, ToolSideEffect::ReadOnly),
            HttpAction::Request => (PermissionLevel::Operator, ToolSideEffect::Stateful),
        };
        Arc::new(HttpTool {
            definition: definition(
                name,
                description,
                schema_for(action),
                permission,
                side_effect,
                30_000,
                true,
            ),
            action,
            policy: policy.clone(),
        }) as Arc<dyn ToolExecutor>
    })
    .collect()
}

fn schema_for(action: HttpAction) -> serde_json::Value {
    match action {
        HttpAction::Get => json!({
            "type":"object","required":["url"],"additionalProperties":false,
            "properties":{"url":{"type":"string","minLength":1},"headers":{"type":["object","null"],"additionalProperties":{"type":"string"}},"timeout_ms":{"type":["integer","null"],"minimum":1}}
        }),
        HttpAction::Request => json!({
            "type":"object","required":["url","method"],"additionalProperties":false,
            "properties":{"url":{"type":"string","minLength":1},"method":{"type":"string","enum":["POST","PUT","PATCH","DELETE"]},"headers":{"type":["object","null"],"additionalProperties":{"type":"string"}},"body":{},"timeout_ms":{"type":["integer","null"],"minimum":1}}
        }),
    }
}

#[async_trait]
impl ToolExecutor for HttpTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        match self.action {
            HttpAction::Get => {
                let args: GetArgs = parse_args(invocation.tool_call.arguments)?;
                let timeout_ms = self
                    .policy
                    .timeout(args.timeout_ms, self.definition.timeout_ms)?;
                self.send(
                    Method::GET,
                    args.url,
                    args.headers,
                    None,
                    timeout_ms,
                    cancel,
                )
                .await
            }
            HttpAction::Request => {
                let args: RequestArgs = parse_args(invocation.tool_call.arguments)?;
                let method = match args.method.to_ascii_uppercase().as_str() {
                    "POST" => Method::POST,
                    "PUT" => Method::PUT,
                    "PATCH" => Method::PATCH,
                    "DELETE" => Method::DELETE,
                    _ => {
                        return Err(invalid(
                            "http.request only supports POST, PUT, PATCH, or DELETE",
                        ));
                    }
                };
                self.policy.require_mutation()?;
                let timeout_ms = self
                    .policy
                    .timeout(args.timeout_ms, self.definition.timeout_ms)?;
                self.send(
                    method,
                    args.url,
                    args.headers,
                    args.body,
                    timeout_ms,
                    cancel,
                )
                .await
            }
        }
    }
}

impl HttpTool {
    #[allow(clippy::too_many_lines)]
    async fn send(
        &self,
        method: Method,
        raw_url: String,
        headers: Option<BTreeMap<String, String>>,
        body: Option<serde_json::Value>,
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let url = Url::parse(&raw_url).map_err(|error| invalid(format!("Invalid URL: {error}")))?;
        self.policy.require_http_host_for_url(&raw_url)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("URL must not include a user name or password"));
        }
        let resolved = tokio::select! {
            () = cancel.cancelled() => return Err(ToolError::new(ToolErrorKind::Cancelled, "HTTP host resolution was cancelled", true)),
            result = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                resolve_http_addresses(&url, self.policy.allow_private_http_addresses),
            ) => result
                .map_err(|_| ToolError::new(ToolErrorKind::Timeout, format!("HTTP host resolution exceeded {timeout_ms} milliseconds"), true))??,
        };
        let mut client_builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_millis(timeout_ms));
        if let Some(host) = url.host_str()
            && host.parse::<IpAddr>().is_err()
        {
            client_builder = client_builder.resolve_to_addrs(host, &resolved);
        }
        let client = client_builder
            .build()
            .map_err(|error| ToolError::new(ToolErrorKind::Internal, error.to_string(), false))?;
        let mut request = client.request(method.clone(), url.clone());
        let mut header_bytes = 0usize;
        let mut has_content_type = false;
        if let Some(headers) = headers {
            if headers.len() > 128 {
                return Err(invalid("HTTP header count must not exceed 128"));
            }
            for (name, value) in headers {
                let name = reqwest::header::HeaderName::try_from(name.as_str())
                    .map_err(|error| invalid(format!("Invalid HTTP header name: {error}")))?;
                let value = reqwest::header::HeaderValue::try_from(value.as_str())
                    .map_err(|error| invalid(format!("Invalid HTTP header value: {error}")))?;
                if matches!(
                    name.as_str(),
                    "connection"
                        | "content-length"
                        | "host"
                        | "proxy-authorization"
                        | "proxy-connection"
                        | "transfer-encoding"
                ) {
                    return Err(invalid(format!("Caller must not set HTTP header: {name}")));
                }
                header_bytes = header_bytes
                    .saturating_add(name.as_str().len())
                    .saturating_add(value.as_bytes().len());
                if name == reqwest::header::CONTENT_TYPE {
                    has_content_type = true;
                }
                request = request.header(name, value);
            }
        }
        if header_bytes > self.policy.max_http_header_bytes {
            return Err(invalid(format!(
                "Total HTTP header size exceeds the {} byte limit. The limit is controlled by [security].max_http_header_bytes in agent.toml",
                self.policy.max_http_header_bytes
            )));
        }
        if let Some(body) = body {
            let serialized = serde_json::to_vec(&body).map_err(|error| {
                invalid(format!("Unable to serialize HTTP request body: {error}"))
            })?;
            if serialized.len() > self.policy.max_input_bytes {
                return Err(invalid(format!(
                    "HTTP request body exceeds the {} byte limit. The limit is controlled by [security].max_input_bytes in agent.toml",
                    self.policy.max_input_bytes
                )));
            }
            if !has_content_type {
                request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
            }
            request = request.body(serialized);
        }

        let response = tokio::select! {
            () = cancel.cancelled() => return Err(ToolError::new(ToolErrorKind::Cancelled, "HTTP request was cancelled", true)),
            result = request.send() => result.map_err(|error| map_request_error(&error, timeout_ms))?,
        };
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = tokio::select! {
            () = cancel.cancelled() => return Err(ToolError::new(ToolErrorKind::Cancelled, "HTTP response read was cancelled", true)),
            item = stream.next() => item,
        } {
            let chunk = chunk.map_err(|error| {
                ToolError::new(ToolErrorKind::TargetUnavailable, error.to_string(), true)
            })?;
            let remaining = self.policy.max_output_bytes.saturating_sub(bytes.len());
            if chunk.len() > remaining {
                bytes.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let response_text = String::from_utf8_lossy(&bytes).into_owned();
        let summary_url = safe_url_for_summary(&url);
        Ok(ToolResult {
            summary: format!(
                "HTTP {} {} returned {}",
                method,
                summary_url,
                status.as_u16()
            ),
            data: json!({
                "status": status.as_u16(),
                "success": status.is_success(),
                "content_type": content_type,
                "body": response_text,
            }),
            truncated,
        })
    }
}

async fn resolve_http_addresses(
    url: &Url,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, ToolError> {
    let host = url
        .host_str()
        .ok_or_else(|| invalid("URL is missing a host name"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| invalid("URL is missing a recognized port"))?;
    if port == 0 {
        return Err(invalid("HTTP port must be between 1 and 65535"));
    }
    let literal_ip = host.parse::<IpAddr>().ok();
    let mut addresses = if let Some(ip) = literal_ip {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| {
                ToolError::new(
                    ToolErrorKind::TargetUnavailable,
                    format!("Failed to resolve HTTP host {host}: {error}"),
                    true,
                )
            })?
            .collect()
    };
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("HTTP host has no connectable address: {host}"),
            true,
        ));
    }
    if !allow_private
        && addresses
            .iter()
            .any(|address| is_restricted_ip(address.ip()))
    {
        return Err(ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!(
                "HTTP host resolved to a restricted address: {host}. To allow private, loopback, or link-local access, an administrator must set [security].allow_private_http_addresses = true in agent.toml."
            ),
            false,
        ));
    }
    Ok(addresses)
}

fn is_restricted_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && (18..=19).contains(&octets[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip
                    .to_ipv4()
                    .is_some_and(|mapped| is_restricted_ip(mapped.into()))
        }
    }
}

fn safe_url_for_summary(url: &Url) -> String {
    let mut safe = url.clone();
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string()
}

fn map_request_error(error: &reqwest::Error, timeout_ms: u64) -> ToolError {
    if error.is_timeout() {
        ToolError::new(
            ToolErrorKind::Timeout,
            format!("HTTP request exceeded {timeout_ms} milliseconds"),
            true,
        )
    } else if error.is_connect() {
        ToolError::new(ToolErrorKind::TargetUnavailable, error.to_string(), true)
    } else {
        ToolError::new(ToolErrorKind::ExecutionFailed, error.to_string(), false)
    }
}

impl ToolPolicy {
    fn require_http_host_for_url(&self, raw_url: &str) -> Result<(), ToolError> {
        let url = Url::parse(raw_url).map_err(|error| invalid(format!("Invalid URL: {error}")))?;
        match url.scheme() {
            "http" | "https" => {}
            _ => return Err(invalid("Only http and https URLs are allowed")),
        }
        self.require_http_host(url.host_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{is_restricted_ip, safe_url_for_summary};
    use reqwest::Url;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn restricted_addresses_are_blocked_by_default() {
        assert!(is_restricted_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_restricted_ip(IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_restricted_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_restricted_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(!is_restricted_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn request_query_is_not_included_in_summary_url() {
        let url = Url::parse("https://example.com/health?token=secret#fragment").unwrap();
        assert_eq!(safe_url_for_summary(&url), "https://example.com/health");
    }
}
