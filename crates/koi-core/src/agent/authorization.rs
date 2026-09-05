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

    async fn resolve_any(
        &self,
        event_id: crate::domain::EventId,
    ) -> Result<AuthorizationEvidence, AuthorizationError> {
        let event = self
            .store
            .load_event_any(event_id)
            .await
            .map_err(|error| AuthorizationError::new(error.to_string()))?
            .ok_or_else(|| AuthorizationError::new(format!("权限事件不存在：{event_id}")))?;
        evidence_from_event(event)
    }
}

fn evidence_from_event(event: EventEnvelope) -> Result<AuthorizationEvidence, AuthorizationError> {
    let authority_parent_event_id = event.provenance.authority_parent_event_id;
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
            AgentEvent::Ingress(ingress) => ingress_evidence(
                ingress.as_ref(),
                &event.provenance.creator,
                authority_parent_event_id,
            )?,
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
        authority_parent_event_id,
        expires_at: event.provenance.expires_at,
        approval_request_event_id,
    })
}

fn ingress_evidence(
    ingress: &IngressEvent,
    creator: &EventSource,
    authority_parent_event_id: Option<crate::domain::EventId>,
) -> Result<EvidenceParts, AuthorizationError> {
    match ingress {
        IngressEvent::ContextReceived {
            context,
            assessment: _,
        } if context.kind == crate::domain::ContextKind::ToolResult => {
            // 工具结果是数据回传通道，不论其由哪个适配器提交、持久化内容是否异常，都
            // 绝不能成为工具授权证据。这里再次归零，防御历史脏数据或绕过登记器写入。
            Ok((
                AuthorizationEvidenceEventKind::Ingress,
                None,
                PermissionLevel::None,
                PermissionLevel::None,
                None,
            ))
        }
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
            EventSource::Model | EventSource::Tool
                if matches!(
                    context.kind,
                    crate::domain::ContextKind::UserMessage | crate::domain::ContextKind::Alert
                ) && authority_parent_event_id.is_some() =>
            {
                // 主会话委托给子任务的输入由模型发起，但不拥有直接权限；权限只能从
                // 它的 authority_parent_event_id 继续递归追溯。这里忽略输入事件中保存
                // 的权限快照，避免将模型生成的字段当作直接授权。
                Ok((
                    AuthorizationEvidenceEventKind::Ingress,
                    None,
                    PermissionLevel::None,
                    PermissionLevel::None,
                    None,
                ))
            }
            EventSource::Model | EventSource::Tool => {
                Err(AuthorizationError::new("模型或工具输入必须带有权限父事件"))
            }
        },
        IngressEvent::ApprovalSubmitted {
            approval_request_event_id,
            principal,
            assessment,
            approved,
            ..
        } if matches!(creator, EventSource::External(_)) => Ok((
            AuthorizationEvidenceEventKind::Ingress,
            Some(principal.clone()),
            assessment.source_maximum_permission,
            if *approved {
                assessment.effective_permission
            } else {
                // 拒绝决定可以被模型看见，但绝不能成为后续工具调用的授权证据。
                PermissionLevel::None
            },
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
            // 取消请求的语义是撤回或停止当前流程，不是对任何工具调用的授权。
            PermissionLevel::None,
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
