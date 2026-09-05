use std::collections::HashSet;

use chrono::Utc;
use thiserror::Error;

use crate::domain::{
    AgentEvent, ContextKind, ContextPayload, EventEnvelope, EventId, EventSource, ModelContextItem,
    ModelInputRole, PermissionLevel, SourceName, TaskId,
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
    /// 权限检查按顺序执行：事件归属、有效期、持久化权限结论的自洽性、上下文与结论
    /// 的一致性、来源与创建者一致性，最后是权限审查。用户消息、告警与系统事件都是
    /// 输入事件，受会话最低控制权限审查——系统事件携带的 `System` 权限是最高等级，
    /// 因此核心写入的系统事件一定能注入；工具结果回传不受该审查（工具回传通道）。
    ///
    /// # Errors
    ///
    /// 当事件不属于任务、已过期、不是可注入输入、权限结论自洽性被破坏、来源或权限
    /// 结论不一致、权限低于会话最低控制权限，或者内容超限时返回错误。
    pub fn inject(
        &self,
        task_id: TaskId,
        event: &EventEnvelope,
        minimum_control_permission: PermissionLevel,
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
        if assessment.effective_permission > assessment.suggested_permission
            || assessment.effective_permission > assessment.source_maximum_permission
            || assessment.effective_permission > assessment.identity_maximum_permission
        {
            return Err(InputInjectionError::PermissionAssessmentInconsistent(
                event.id,
            ));
        }
        if context.permission != assessment.effective_permission {
            return Err(InputInjectionError::PermissionMismatch(event.id));
        }
        if context.kind == ContextKind::ToolResult
            && assessment.effective_permission != PermissionLevel::None
        {
            return Err(InputInjectionError::ToolResultHasPermission(event.id));
        }
        validate_ingress_creator(
            event,
            &context.origin.source,
            context.kind,
            context.permission,
        )?;
        if is_permission_gated_input(context.kind)
            && !assessment
                .effective_permission
                .allows(minimum_control_permission)
        {
            return Err(InputInjectionError::InsufficientControlPermission {
                event_id: event.id,
                effective_permission: assessment.effective_permission,
                minimum_control_permission,
            });
        }

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

    /// 注入前校验输入事件确实存在于事件存储且与持久化内容一致。
    ///
    /// 模型只能引用已持久化的事件作为授权证据；未持久化或被篡改的输入一旦注入，
    /// 其事件 ID 就可能被模型当作伪造的授权证据引用，造成权限提升。调用方传入的
    /// 事件必须来自对同一事件存储的真实读取。
    ///
    /// # Errors
    ///
    /// 当事件未持久化或与持久化内容不一致时返回错误。
    pub fn verify_persisted_events(
        expected: &[EventEnvelope],
        stored: &[EventEnvelope],
    ) -> Result<(), InputInjectionError> {
        for event in expected {
            let Some(persisted) = stored.iter().find(|candidate| candidate.id == event.id) else {
                return Err(InputInjectionError::NotPersisted(event.id));
            };
            if persisted != event {
                return Err(InputInjectionError::PersistedEventMismatch(event.id));
            }
        }
        Ok(())
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
        minimum_control_permission: PermissionLevel,
    ) -> Result<Vec<ModelContextItem>, InputInjectionError> {
        let mut event_ids = HashSet::with_capacity(events.len());
        let mut context = Vec::with_capacity(events.len());
        for event in events {
            if !event_ids.insert(event.id) {
                return Err(InputInjectionError::DuplicateEvent(event.id));
            }
            context.push(self.inject(task_id, event, minimum_control_permission)?);
        }
        Ok(context)
    }
}

impl Default for InputInjector {
    fn default() -> Self {
        Self::new(InputInjectionPolicy::default())
    }
}

/// 需要经过会话最低控制权限审查的输入事件。
///
/// 用户消息与告警是外部指令，必须满足会话最低控制权限。系统事件也是一种输入事件，
/// 走同一条审查：但它携带的 `System` 权限是最高等级，`System.allows` 对任何最低
/// 权限都成立，因此核心写入的系统事件一定能注入上下文——保证来自权限本身，而不是
/// 类型豁免。工具结果回传（`ToolResult`）是唯一的审查豁免：工具事件进入会话不受
/// 权限限制（工具回传通道的显式设计，见 `deliver_child_result`）。
fn is_permission_gated_input(kind: ContextKind) -> bool {
    matches!(
        kind,
        ContextKind::UserMessage | ContextKind::Alert | ContextKind::SystemEvent
    )
}

fn validate_ingress_creator(
    event: &EventEnvelope,
    origin_source: &str,
    kind: ContextKind,
    permission: crate::domain::PermissionLevel,
) -> Result<(), InputInjectionError> {
    match &event.provenance.creator {
        EventSource::External(source_name) => {
            if kind == ContextKind::SystemEvent {
                // 系统事件是核心专属的输入事件，外部来源不得伪造。
                return Err(InputInjectionError::InvalidIngressCreator(event.id));
            }
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
        // 主会话通过 task.input 委托给子任务的输入由模型触发，但权限快照只能由核心
        // 根据 authority_parent_event_id 计算；模型来源的输入必须带父事件且具备可授权
        // 权限，不能凭空伪造普通用户输入。
        EventSource::Model
            if matches!(kind, ContextKind::UserMessage | ContextKind::Alert)
                && event.provenance.authority_parent_event_id.is_some()
                && permission.can_authorize() => {}
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
    #[error("输入事件权限结论内部不一致：{0}")]
    PermissionAssessmentInconsistent(crate::domain::EventId),
    #[error("输入事件权限结论与上下文不一致：{0}")]
    PermissionMismatch(crate::domain::EventId),
    #[error("工具结果必须是不具备授权能力的 None 权限：{0}")]
    ToolResultHasPermission(crate::domain::EventId),
    #[error("输入事件的创建者无权使用该上下文类型：{0}")]
    InvalidIngressCreator(crate::domain::EventId),
    #[error("输入事件来源与上下文来源不一致：{0}")]
    OriginSourceMismatch(crate::domain::EventId),
    #[error("该上下文类型不能注入模型：{0}")]
    NonInjectableContextKind(crate::domain::EventId),
    #[error(
        "输入事件 {event_id} 权限 {:?} 低于会话最低控制权限 {:?}",
        effective_permission,
        minimum_control_permission
    )]
    InsufficientControlPermission {
        event_id: EventId,
        effective_permission: PermissionLevel,
        minimum_control_permission: PermissionLevel,
    },
    #[error("输入事件未持久化，拒绝注入：{0}")]
    NotPersisted(crate::domain::EventId),
    #[error("输入事件与持久化内容不一致：{0}")]
    PersistedEventMismatch(crate::domain::EventId),
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
