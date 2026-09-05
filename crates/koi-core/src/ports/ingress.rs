use std::collections::BTreeMap;

use async_trait::async_trait;
use thiserror::Error;

use crate::agent::{RuntimeError, TaskRuntime};
use crate::domain::{
    AgentEvent, ContextKind, EventEnvelope, EventId, EventProvenance, EventSource, IngressDraft,
    IngressEvent, IngressSubject, PermissionAssessment, PermissionLevel, SourceName,
    SourceNameError, TaskId,
};
use crate::ports::EventStore;

/// 来源在核心中的权限注册信息。
///
/// 例如 QQ 可建议至 `Admin`，监控告警最多为 `User`；只有核心内部来源可登记为
/// `System`。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IngressSourceDefinition {
    pub source: SourceName,
    pub maximum_permission: PermissionLevel,
}

/// 已配置来源的权限上限注册表。
#[derive(Default)]
pub struct IngressSourceRegistry {
    definitions: BTreeMap<SourceName, IngressSourceDefinition>,
}

impl IngressSourceRegistry {
    /// # Errors
    ///
    /// 当来源名称为空或重名时返回错误。
    pub fn register(
        &mut self,
        definition: IngressSourceDefinition,
    ) -> Result<(), IngressSourceRegistrationError> {
        if self.definitions.contains_key(&definition.source) {
            return Err(IngressSourceRegistrationError::DuplicateSource(
                definition.source.as_str().into(),
            ));
        }
        self.definitions
            .insert(definition.source.clone(), definition);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, source: &str) -> Option<&IngressSourceDefinition> {
        let source = SourceName::new(source).ok()?;
        self.definitions.get(&source)
    }
}

#[derive(Debug, Error)]
pub enum IngressSourceRegistrationError {
    #[error("输入来源已注册：{0}")]
    DuplicateSource(String),
}

/// 核心身份与角色系统的查询端口。
///
/// 实现可以查询本地角色表、Web 会话或 QQ 群管理员映射，但返回值只是该身份的最高
/// 权限；外部来源的建议仍会再经过注册表上限截断。
#[async_trait]
pub trait IngressPermissionResolver: Send + Sync {
    /// # Errors
    ///
    /// 当身份不存在、来源认证状态无法确认或角色查询失败时返回错误。
    async fn maximum_permission(
        &self,
        subject: IngressSubject,
    ) -> Result<PermissionLevel, IngressRegistrationError>;
}

/// 将外部草稿转换为唯一、可审计的 Ingress 事件的核心服务。
pub struct IngressRegistrar<'a> {
    sources: &'a IngressSourceRegistry,
    permissions: &'a dyn IngressPermissionResolver,
}

impl<'a> IngressRegistrar<'a> {
    #[must_use]
    pub const fn new(
        sources: &'a IngressSourceRegistry,
        permissions: &'a dyn IngressPermissionResolver,
    ) -> Self {
        Self {
            sources,
            permissions,
        }
    }

    /// 注册一个外部输入，并由 `TaskRuntime` 分配其 `EventId` 与任务内顺序。
    ///
    /// # Errors
    ///
    /// 当来源未登记、身份权限无法解析或事件无法持久化时返回错误。
    pub async fn register<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        draft: IngressDraft,
    ) -> Result<EventEnvelope, IngressRegistrationError>
    where
        S: EventStore,
    {
        let source = self
            .sources
            .get(draft.source())
            .ok_or_else(|| IngressRegistrationError::UnregisteredSource(draft.source().into()))?;
        let assessment = self.assess(&draft, source).await?;
        let (event, causation_id) = into_ingress_event(draft, assessment);
        if is_cycle_input(&event) {
            // 输入代表同一会话的新一轮对话。终态会话先追加一个核心控制事件作为周期
            // 边界，再追加外部输入；旧周期的结果仍然保留，且输入权限不会从旧结果继承。
            runtime
                .start_new_cycle_if_terminal(causation_id)
                .await
                .map_err(IngressRegistrationError::Runtime)?;
        }
        runtime
            .record_with_provenance(
                AgentEvent::ingress(event),
                causation_id,
                EventProvenance {
                    creator: EventSource::External(source.source.clone()),
                    direct_permission: Some(assessment.effective_permission),
                    authority_parent_event_id: None,
                    expires_at: None,
                },
            )
            .await
            .map_err(Into::into)
    }

    async fn assess(
        &self,
        draft: &IngressDraft,
        source: &IngressSourceDefinition,
    ) -> Result<PermissionAssessment, IngressRegistrationError> {
        let identity_maximum_permission =
            self.permissions.maximum_permission(draft.subject()).await?;
        // 工具回传只用于向模型提供执行结果，不能表达用户意图或作为工具授权证据。
        // 即使外部适配器错误地建议高权限，也必须在事件落库前归零。
        let suggested_permission = match draft {
            IngressDraft::Context { context, .. } if context.kind == ContextKind::ToolResult => {
                PermissionLevel::None
            }
            _ => draft.suggested_permission(),
        };
        Ok(PermissionAssessment::new(
            suggested_permission,
            source.maximum_permission,
            identity_maximum_permission,
        ))
    }
}

fn is_cycle_input(event: &IngressEvent) -> bool {
    matches!(
        event,
        IngressEvent::ContextReceived { context, .. }
            if matches!(
                context.kind,
                ContextKind::UserMessage
                    | ContextKind::Alert
                    | ContextKind::SystemEvent
            )
    )
}

fn into_ingress_event(
    draft: IngressDraft,
    assessment: PermissionAssessment,
) -> (IngressEvent, Option<EventId>) {
    match draft {
        IngressDraft::Context { mut context, .. } => {
            let causation_id = context.causation_id;
            context.permission = assessment.effective_permission;
            (
                IngressEvent::ContextReceived {
                    context,
                    assessment,
                },
                causation_id,
            )
        }
        IngressDraft::Approval {
            approval_request_event_id,
            principal,
            scope,
            approved,
            ..
        } => (
            IngressEvent::ApprovalSubmitted {
                approval_request_event_id,
                principal,
                scope,
                assessment,
                approved,
            },
            Some(approval_request_event_id),
        ),
        IngressDraft::Cancellation {
            principal,
            scope,
            reason,
            ..
        } => (
            IngressEvent::CancellationRequested {
                principal,
                scope,
                assessment,
                reason,
            },
            None,
        ),
    }
}

#[derive(Debug, Error)]
pub enum IngressRegistrationError {
    #[error("输入来源未登记：{0}")]
    UnregisteredSource(String),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error("身份权限解析失败：{message}")]
    PermissionResolution { message: String },
    #[error(transparent)]
    InvalidSourceName(#[from] SourceNameError),
}

impl IngressRegistrationError {
    #[must_use]
    pub fn permission_resolution(message: impl Into<String>) -> Self {
        Self::PermissionResolution {
            message: message.into(),
        }
    }
}

/// 创建新任务后由调用方返回给来源适配器的关键事件标识。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisteredTaskInput {
    pub task_id: TaskId,
    pub ingress_event_id: EventId,
}
