use std::collections::HashSet;

use chrono::Utc;
use thiserror::Error;

use crate::domain::{
    AgentEvent, ContextKind, ContextPayload, EventEnvelope, EventSource, ModelContextItem,
    ModelInputRole, SourceName, TaskId,
};

/// 输入事件注入模型前的确定性限制。
#[derive(Clone, Debug)]
pub struct InputInjectionPolicy {
    pub max_content_chars: usize,
}

impl Default for InputInjectionPolicy {
    fn default() -> Self {
        Self {
            max_content_chars: 12_000,
        }
    }
}

/// 将已持久化输入事件转换为模型上下文的核心服务。
pub struct InputInjector {
    policy: InputInjectionPolicy,
}

impl InputInjector {
    #[must_use]
    pub const fn new(policy: InputInjectionPolicy) -> Self {
        Self { policy }
    }

    /// 注入一条属于当前任务的上下文输入。
    ///
    /// # Errors
    ///
    /// 当事件不属于任务、已过期、不是可注入输入、来源或权限结论不一致，或者内容超限时
    /// 返回错误。
    pub fn inject(
        &self,
        task_id: TaskId,
        event: &EventEnvelope,
    ) -> Result<ModelContextItem, InputInjectionError> {
        if event.task_id != task_id {
            return Err(InputInjectionError::WrongTask {
                expected: task_id,
                actual: event.task_id,
            });
        }
        if event
            .provenance
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            return Err(InputInjectionError::Expired(event.id));
        }

        let AgentEvent::Ingress(ingress) = &event.payload else {
            return Err(InputInjectionError::NotIngress(event.id));
        };
        let crate::domain::IngressEvent::ContextReceived {
            context,
            assessment,
        } = ingress.as_ref()
        else {
            return Err(InputInjectionError::NonContextualIngress(event.id));
        };
        if context.permission != assessment.effective_permission {
            return Err(InputInjectionError::PermissionMismatch(event.id));
        }
        validate_ingress_creator(
            event,
            &context.origin.source,
            context.kind,
            context.permission,
        )?;

        let role = match context.kind {
            ContextKind::UserMessage | ContextKind::Alert => ModelInputRole::User,
            ContextKind::AssistantMessage => ModelInputRole::Assistant,
            ContextKind::ToolResult => ModelInputRole::Tool,
            ContextKind::SystemEvent => ModelInputRole::System,
            ContextKind::Approval | ContextKind::Cancellation => {
                return Err(InputInjectionError::NonInjectableContextKind(event.id));
            }
        };
        let content = render_payload(&context.payload);
        if content.trim().is_empty() {
            return Err(InputInjectionError::EmptyContent(event.id));
        }
        if content.chars().count() > self.policy.max_content_chars {
            return Err(InputInjectionError::ContentTooLong {
                event_id: event.id,
                limit: self.policy.max_content_chars,
            });
        }

        Ok(ModelContextItem {
            event_id: event.id,
            role,
            content,
            permission: assessment.effective_permission,
        })
    }

    /// 按给定顺序注入多条事件，并拒绝重复事件。
    ///
    /// # Errors
    ///
    /// 当任意事件无法注入或事件 ID 重复时返回错误。
    pub fn inject_many(
        &self,
        task_id: TaskId,
        events: &[EventEnvelope],
    ) -> Result<Vec<ModelContextItem>, InputInjectionError> {
        let mut event_ids = HashSet::with_capacity(events.len());
        let mut context = Vec::with_capacity(events.len());
        for event in events {
            if !event_ids.insert(event.id) {
                return Err(InputInjectionError::DuplicateEvent(event.id));
            }
            context.push(self.inject(task_id, event)?);
        }
        Ok(context)
    }
}

impl Default for InputInjector {
    fn default() -> Self {
        Self::new(InputInjectionPolicy::default())
    }
}

fn validate_ingress_creator(
    event: &EventEnvelope,
    origin_source: &str,
    kind: ContextKind,
    permission: crate::domain::PermissionLevel,
) -> Result<(), InputInjectionError> {
    match &event.provenance.creator {
        EventSource::External(source_name) => {
            let origin_name = SourceName::new(origin_source)
                .map_err(|_| InputInjectionError::OriginSourceMismatch(event.id))?;
            if source_name != &origin_name {
                return Err(InputInjectionError::OriginSourceMismatch(event.id));
            }
        }
        // 系统提示词与核心生成的子任务回传由核心创建；回传必须始终是无权限工具结果。
        EventSource::System if kind == ContextKind::SystemEvent => {}
        EventSource::System
            if kind == ContextKind::ToolResult
                && permission == crate::domain::PermissionLevel::None => {}
        EventSource::System | EventSource::Model | EventSource::Tool => {
            return Err(InputInjectionError::InvalidIngressCreator(event.id));
        }
    }
    Ok(())
}

fn render_payload(payload: &ContextPayload) -> String {
    match payload {
        ContextPayload::Text { text, .. } => text.clone(),
        ContextPayload::Alert {
            name,
            severity,
            summary,
            labels,
        } => format!(
            "告警：{name}\n严重级别：{severity}\n摘要：{summary}\n标签：{}",
            labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ContextPayload::Structured(value) => value.to_string(),
    }
}

#[derive(Debug, Error)]
pub enum InputInjectionError {
    #[error("输入事件属于任务 {actual}，而非当前任务 {expected}")]
    WrongTask { expected: TaskId, actual: TaskId },
    #[error("输入事件已过期：{0}")]
    Expired(crate::domain::EventId),
    #[error("事件不是输入事件：{0}")]
    NotIngress(crate::domain::EventId),
    #[error("输入事件不包含可注入上下文：{0}")]
    NonContextualIngress(crate::domain::EventId),
    #[error("输入事件权限结论与上下文不一致：{0}")]
    PermissionMismatch(crate::domain::EventId),
    #[error("输入事件必须由已注册外部来源创建：{0}")]
    InvalidIngressCreator(crate::domain::EventId),
    #[error("输入事件来源与上下文来源不一致：{0}")]
    OriginSourceMismatch(crate::domain::EventId),
    #[error("该上下文类型不能注入模型：{0}")]
    NonInjectableContextKind(crate::domain::EventId),
    #[error("输入内容为空：{0}")]
    EmptyContent(crate::domain::EventId),
    #[error("输入事件 {event_id} 超过 {limit} 个字符")]
    ContentTooLong {
        event_id: crate::domain::EventId,
        limit: usize,
    },
    #[error("输入事件重复：{0}")]
    DuplicateEvent(crate::domain::EventId),
}
