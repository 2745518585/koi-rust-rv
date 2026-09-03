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
        } => {
            let is_safe_internal_result = matches!(creator, EventSource::System)
                && context.kind == crate::domain::ContextKind::ToolResult
                && assessment.effective_permission == PermissionLevel::None;
            if !matches!(creator, EventSource::External(_)) && !is_safe_internal_result {
                return Err(AuthorizationError::new("输入事件必须由外部来源创建"));
            }
            Ok((
                AuthorizationEvidenceEventKind::Ingress,
                context.actor.clone(),
                assessment.source_maximum_permission,
                assessment.effective_permission,
                None,
            ))
        }
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
