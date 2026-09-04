use async_trait::async_trait;
use chrono::Utc;

use crate::domain::{
    AgentEvent, AuthorizationEvidence, AuthorizationEvidenceEventKind, AuthorizationEvidenceStatus,
    EventEnvelope, EventSource, IngressEvent, PermissionLevel, TaskId,
};
use crate::ports::{AuthorizationError, AuthorizationEvidenceResolver, EventStore};

type EvidenceParts = (
    AuthorizationEvidenceEventKind,
    Option<crate::domain::Principal>,
    PermissionLevel,
    PermissionLevel,
    Option<crate::domain::EventId>,
);

/// 从事件存储中重建权限证据的解析器。
///
/// 权限结论只读取事件持久化时的审查结果，不会在工具调用时重新查询用户当前角色。
pub struct PersistedAuthorizationEvidenceResolver<'a, S> {
    store: &'a S,
}

impl<'a, S> PersistedAuthorizationEvidenceResolver<'a, S> {
    #[must_use]
    pub const fn new(store: &'a S) -> Self {
        Self { store }
    }
}

#[async_trait]
impl<S> AuthorizationEvidenceResolver for PersistedAuthorizationEvidenceResolver<'_, S>
where
    S: EventStore,
{
    async fn resolve(
        &self,
        task_id: TaskId,
        event_id: crate::domain::EventId,
    ) -> Result<AuthorizationEvidence, AuthorizationError> {
        let event = self
            .store
            .load_event(task_id, event_id)
            .await
            .map_err(|error| AuthorizationError::new(error.to_string()))?
            .ok_or_else(|| AuthorizationError::new(format!("权限事件不存在：{event_id}")))?;
        evidence_from_event(event)
    }
}

fn evidence_from_event(event: EventEnvelope) -> Result<AuthorizationEvidence, AuthorizationError> {
    let status = if event
        .provenance
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now())
    {
        AuthorizationEvidenceStatus::Expired
    } else {
        AuthorizationEvidenceStatus::Active
    };

    let (event_kind, principal, source_maximum_permission, permission, approval_request_event_id) =
        match event.payload {
            AgentEvent::Ingress(ingress) => {
                ingress_evidence(ingress.as_ref(), &event.provenance.creator)?
            }
            AgentEvent::Control(_) => direct_evidence(
                AuthorizationEvidenceEventKind::Control,
                &event.provenance.creator,
                event.provenance.direct_permission,
            ),
            AgentEvent::Model(_) => (
                AuthorizationEvidenceEventKind::Model,
                None,
                PermissionLevel::None,
                PermissionLevel::None,
                None,
            ),
            AgentEvent::Tool(_) => (
                AuthorizationEvidenceEventKind::Tool,
                None,
                PermissionLevel::None,
                PermissionLevel::None,
                None,
            ),
        };

    Ok(AuthorizationEvidence {
        event_id: event.id,
        source: event.provenance.creator,
        event_kind,
        principal,
        source_maximum_permission,
        permission,
        status,
        authority_parent_event_id: event.provenance.authority_parent_event_id,
        expires_at: event.provenance.expires_at,
        approval_request_event_id,
    })
}

fn ingress_evidence(
    ingress: &IngressEvent,
    creator: &EventSource,
) -> Result<EvidenceParts, AuthorizationError> {
    match ingress {
        IngressEvent::ContextReceived {
            context,
            assessment,
        } => match creator {
            EventSource::External(_) => Ok((
                AuthorizationEvidenceEventKind::Ingress,
                context.actor.clone(),
                assessment.source_maximum_permission,
                assessment.effective_permission,
                None,
            )),
            // 工具结果回传必须保持无权限，仅供模型分析；即使评估结论被伪造为高权限，
            // 证据仍按 None 计算。
            EventSource::System
                if context.kind == crate::domain::ContextKind::ToolResult
                    && assessment.effective_permission == PermissionLevel::None =>
            {
                Ok((
                    AuthorizationEvidenceEventKind::Ingress,
                    None,
                    PermissionLevel::None,
                    PermissionLevel::None,
                    None,
                ))
            }
            // 其余核心内部事件（例如子任务引导输入）拥有 System 直接权限：该权限只
            // 代表事件由核心创建并可用于核心自身的运转判定。System 来源事件永远不能
            // 作为权限父节点参与模型的提权审查（见 `can_be_authority_parent`），因此
            // 不会形成权限提升通道。
            EventSource::System => Ok((
                AuthorizationEvidenceEventKind::Ingress,
                None,
                PermissionLevel::System,
                PermissionLevel::System,
                None,
            )),
            EventSource::Model | EventSource::Tool => Err(AuthorizationError::new(
                "输入事件必须由外部来源或核心创建",
            )),
        },
        IngressEvent::ApprovalSubmitted {
            approval_request_event_id,
            principal,
            assessment,
            ..
        } if matches!(creator, EventSource::External(_)) => Ok((
            AuthorizationEvidenceEventKind::Ingress,
            Some(principal.clone()),
            assessment.source_maximum_permission,
            assessment.effective_permission,
            Some(*approval_request_event_id),
        )),
        IngressEvent::ApprovalSubmitted { .. } => {
            Err(AuthorizationError::new("审批输入必须由外部来源创建"))
        }
        IngressEvent::CancellationRequested {
            principal,
            assessment,
            ..
        } if matches!(creator, EventSource::External(_)) => Ok((
            AuthorizationEvidenceEventKind::Ingress,
            Some(principal.clone()),
            assessment.source_maximum_permission,
            assessment.effective_permission,
            None,
        )),
        IngressEvent::CancellationRequested { .. } => {
            Err(AuthorizationError::new("取消输入必须由外部来源创建"))
        }
    }
}

fn direct_evidence(
    event_kind: AuthorizationEvidenceEventKind,
    creator: &EventSource,
    direct_permission: Option<PermissionLevel>,
) -> EvidenceParts {
    let permission = match creator {
        EventSource::System => PermissionLevel::System,
        EventSource::External(_) => direct_permission.unwrap_or(PermissionLevel::None),
        EventSource::Model | EventSource::Tool => PermissionLevel::None,
    };
    (event_kind, None, permission, permission, None)
}
