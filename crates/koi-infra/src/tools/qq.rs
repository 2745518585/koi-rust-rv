use std::sync::Arc;

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizedToolInvocation, PermissionLevel, ToolDefinition, ToolError, ToolErrorKind,
    ToolResult, ToolSideEffect,
};
use koi_core::ports::ToolExecutor;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::qq_source::{
    MAX_CONTENT_CHARS, MAX_GROUP_OPENID_CHARS, QqError, QqReplyDelivery, QqSource,
};

use super::{definition, internal, invalid, parse_args};

const TOOL_NAME: &str = "qq.group_send";
const REPLY_TOOL_NAME: &str = "qq.reply";
const REPORT_TOOL_NAME: &str = "qq.report";
const TOOL_TIMEOUT_MS: u64 = 30_000;

struct QqGroupMessageTool {
    definition: ToolDefinition,
    source: Arc<QqSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupMessageArgs {
    group_openid: String,
    content: String,
    reply_to_message_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageContentArgs {
    content: String,
}

pub(crate) fn tools(source: Arc<QqSource>) -> Vec<Arc<dyn ToolExecutor>> {
    let group_source = Arc::clone(&source);
    let reply_source = Arc::clone(&source);
    let mut tools: Vec<Arc<dyn ToolExecutor>> = vec![
        Arc::new(QqGroupMessageTool {
            definition: group_definition(),
            source: group_source,
        }),
        Arc::new(QqReplyTool {
            definition: reply_definition(),
            source: reply_source,
        }),
    ];
    if source.report_group_openid().is_some() {
        tools.push(Arc::new(QqReportTool {
            definition: report_definition(),
            source,
        }));
    }
    tools
}

fn group_definition() -> ToolDefinition {
    definition(
        TOOL_NAME,
        "Send a text notification to a specified QQ group through the configured QQ bot; optionally reply to an existing group message.",
        json!({
            "type": "object",
            "required": ["group_openid", "content"],
            "additionalProperties": false,
            "properties": {
                "group_openid": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_GROUP_OPENID_CHARS
                },
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_CONTENT_CHARS
                },
                "reply_to_message_id": {
                    "type": ["string", "null"],
                    "maxLength": MAX_GROUP_OPENID_CHARS
                }
            }
        }),
        PermissionLevel::User,
        ToolSideEffect::Notification,
        TOOL_TIMEOUT_MS,
        true,
    )
}

fn reply_definition() -> ToolDefinition {
    definition(
        REPLY_TOOL_NAME,
        "Reply to the QQ message selected by the authority-parent event. The destination is recovered from that persisted QQ context; only the reply content is supplied.",
        json!({
            "type": "object",
            "required": ["content"],
            "additionalProperties": false,
            "properties": {
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_CONTENT_CHARS
                }
            }
        }),
        PermissionLevel::User,
        ToolSideEffect::Notification,
        TOOL_TIMEOUT_MS,
        true,
    )
}

fn report_definition() -> ToolDefinition {
    definition(
        REPORT_TOOL_NAME,
        "Send an operational report to the configured primary QQ report group. The destination is fixed by server configuration and cannot be changed by the call.",
        json!({
            "type": "object",
            "required": ["content"],
            "additionalProperties": false,
            "properties": {
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_CONTENT_CHARS
                }
            }
        }),
        PermissionLevel::User,
        ToolSideEffect::Notification,
        TOOL_TIMEOUT_MS,
        true,
    )
}

#[async_trait]
impl ToolExecutor for QqGroupMessageTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args: GroupMessageArgs = parse_args(invocation.tool_call.arguments)?;
        let group_openid = args.group_openid.trim().to_owned();
        if group_openid.is_empty() {
            return Err(invalid("QQ group_openid must not be empty"));
        }
        if group_openid.chars().count() > MAX_GROUP_OPENID_CHARS {
            return Err(invalid(format!(
                "QQ group_openid exceeds {MAX_GROUP_OPENID_CHARS} characters"
            )));
        }
        let content = args.content.trim().to_owned();
        if content.is_empty() {
            return Err(invalid("QQ group message content must not be empty"));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(invalid(format!(
                "QQ group message content exceeds {MAX_CONTENT_CHARS} characters"
            )));
        }
        let reply_to_message_id = args
            .reply_to_message_id
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if reply_to_message_id
            .as_deref()
            .is_some_and(|value| value.chars().count() > MAX_GROUP_OPENID_CHARS)
        {
            return Err(invalid(format!(
                "QQ reply_to_message_id exceeds {MAX_GROUP_OPENID_CHARS} characters"
            )));
        }

        if cancel.is_cancelled() {
            return Err(ToolError::new(
                ToolErrorKind::Cancelled,
                "QQ group delivery was cancelled",
                true,
            ));
        }
        let delivery = tokio::select! {
            () = cancel.cancelled() => {
                return Err(ToolError::new(
                    ToolErrorKind::Cancelled,
                    "QQ group delivery was cancelled",
                    true,
                ));
            }
            result = self.source.send_group_message(
                &group_openid,
                &content,
                reply_to_message_id.as_deref(),
            ) => result.map_err(map_qq_error)?,
        };
        Ok(ToolResult {
            summary: format!(
                "QQ group message accepted for {} ({} chunk{})",
                delivery.group_openid,
                delivery.chunks,
                if delivery.chunks == 1 { "" } else { "s" },
            ),
            data: json!({
                "source": "qq",
                "group_openid": delivery.group_openid,
                "reply_to_message_id": delivery.reply_to_message_id,
                "chunks": delivery.chunks,
                "message_ids": delivery.message_ids,
            }),
            truncated: false,
        })
    }
}

struct QqReplyTool {
    definition: ToolDefinition,
    source: Arc<QqSource>,
}

#[async_trait]
impl ToolExecutor for QqReplyTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args: MessageContentArgs = parse_args(invocation.tool_call.arguments)?;
        let content = validate_content(args.content, "QQ reply")?;
        let context_event_id = invocation
            .tool_call
            .authority_parent_event_id
            .ok_or_else(|| invalid("qq.reply requires a QQ context authority-parent event"))?;

        if cancel.is_cancelled() {
            return Err(cancelled("QQ reply"));
        }
        let delivery = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled("QQ reply")),
            result = self.source.reply_to_context(
                invocation.task_id,
                context_event_id,
                &content,
            ) => result.map_err(map_qq_error)?,
        };
        Ok(reply_result(delivery))
    }
}

struct QqReportTool {
    definition: ToolDefinition,
    source: Arc<QqSource>,
}

#[async_trait]
impl ToolExecutor for QqReportTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let args: MessageContentArgs = parse_args(invocation.tool_call.arguments)?;
        let content = validate_content(args.content, "QQ report")?;

        if cancel.is_cancelled() {
            return Err(cancelled("QQ report"));
        }
        let delivery = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled("QQ report")),
            result = self.source.report_message(&content) => result.map_err(map_qq_error)?,
        };
        Ok(ToolResult {
            summary: format!(
                "QQ report accepted for {} ({} chunk{})",
                delivery.group_openid,
                delivery.chunks,
                if delivery.chunks == 1 { "" } else { "s" },
            ),
            data: json!({
                "source": "qq",
                "group_openid": delivery.group_openid,
                "chunks": delivery.chunks,
                "message_ids": delivery.message_ids,
            }),
            truncated: false,
        })
    }
}

fn validate_content(content: String, label: &str) -> Result<String, ToolError> {
    let content = content.trim().to_owned();
    if content.is_empty() {
        return Err(invalid(format!("{label} content must not be empty")));
    }
    if content.chars().count() > MAX_CONTENT_CHARS {
        return Err(invalid(format!(
            "{label} content exceeds {MAX_CONTENT_CHARS} characters"
        )));
    }
    Ok(content)
}

fn cancelled(label: &str) -> ToolError {
    ToolError::new(
        ToolErrorKind::Cancelled,
        format!("{label} was cancelled"),
        true,
    )
}

fn reply_result(delivery: QqReplyDelivery) -> ToolResult {
    ToolResult {
        summary: format!(
            "QQ reply accepted for {}:{} ({} chunk{})",
            delivery.scope.kind,
            delivery.scope.id,
            delivery.chunks,
            if delivery.chunks == 1 { "" } else { "s" },
        ),
        data: json!({
            "source": "qq",
            "scope": {
                "kind": delivery.scope.kind,
                "id": delivery.scope.id,
            },
            "reply_to_message_id": delivery.reply_to_message_id,
            "chunks": delivery.chunks,
            "message_ids": delivery.message_ids,
        }),
        truncated: false,
    }
}

fn map_qq_error(error: QqError) -> ToolError {
    match error {
        QqError::Configuration(message) | QqError::Event(message) => invalid(message),
        QqError::Core(message) => internal(message),
        QqError::Network(message) | QqError::Gateway(message) => {
            ToolError::new(ToolErrorKind::TargetUnavailable, message, true)
        }
        QqError::Api {
            status,
            code,
            message,
        } => ToolError::new(
            ToolErrorKind::TargetUnavailable,
            format!("QQ API group delivery failed (HTTP {status}, code {code:?}): {message}"),
            status == 429 || status >= 500,
        ),
    }
}
