use chrono::Utc;
use koi_core::agent::PersistedAuthorizationEvidenceResolver;
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, EventEnvelope,
    EventId, EventProvenance, EventSource, IngressEvent, PermissionAssessment, PermissionLevel,
    Principal, Scope, SourceName, TaskId,
};
use koi_core::ports::{AuthorizationEvidenceResolver, EventStore, InMemoryEventStore};

#[tokio::test]
async fn resolver_returns_persisted_source_limit_and_effective_permission() {
    let store = InMemoryEventStore::default();
    let task_id = TaskId::new();
    let now = Utc::now();
    let assessment = PermissionAssessment::new(
        PermissionLevel::Admin,
        PermissionLevel::Operator,
        PermissionLevel::Admin,
    );
    let mut event = EventEnvelope::new(
        task_id,
        1,
        None,
        AgentEvent::ingress(IngressEvent::ContextReceived {
            context: Box::new(ContextEnvelope {
                schema_version: 1,
                kind: ContextKind::UserMessage,
                origin: ContextOrigin {
                    source: "qq".into(),
                    source_instance: "group-42".into(),
                    native_event_id: "message-1".into(),
                },
                actor: Some(Principal::new("qq", "10001")),
                scope: Scope::new("qq_group", "42"),
                occurred_at: now,
                received_at: now,
                position: None,
                permission: assessment.effective_permission,
                payload: ContextPayload::Text {
                    text: "重启服务".into(),
                    mentions: vec!["bot".into()],
                },
                causation_id: None,
                content_hash: "test".into(),
            }),
            assessment,
        }),
    );
    event.provenance = EventProvenance {
        creator: EventSource::External(SourceName::new("qq").unwrap()),
        direct_permission: Some(PermissionLevel::Operator),
        authority_parent_event_id: None,
        expires_at: None,
    };
    store.append(&event).await.unwrap();

    let resolver = PersistedAuthorizationEvidenceResolver::new(&store);
    let evidence = resolver.resolve(task_id, event.id).await.unwrap();

    assert_eq!(evidence.source.as_str(), "qq");
    assert_eq!(
        evidence.source_maximum_permission,
        PermissionLevel::Operator
    );
    assert_eq!(evidence.permission, PermissionLevel::Operator);
    assert_eq!(evidence.principal, Some(Principal::new("qq", "10001")));
}

fn system_internal_ingress_event(
    task_id: TaskId,
    kind: ContextKind,
    sequence: u64,
) -> EventEnvelope {
    let now = Utc::now();
    let mut event = EventEnvelope::new(
        task_id,
        sequence,
        None,
        AgentEvent::ingress(IngressEvent::ContextReceived {
            context: Box::new(ContextEnvelope {
                schema_version: 1,
                kind,
                origin: ContextOrigin {
                    source: "internal-task".into(),
                    source_instance: "core".into(),
                    native_event_id: format!("internal-{sequence}"),
                },
                actor: None,
                scope: Scope::new("task", task_id.to_string()),
                occurred_at: now,
                received_at: now,
                position: None,
                permission: PermissionLevel::None,
                payload: ContextPayload::Text {
                    text: "核心内部事件".into(),
                    mentions: vec![],
                },
                causation_id: None,
                content_hash: "test".into(),
            }),
            assessment: PermissionAssessment::new(
                PermissionLevel::None,
                PermissionLevel::None,
                PermissionLevel::None,
            ),
        }),
    );
    event.provenance = EventProvenance {
        creator: EventSource::System,
        direct_permission: Some(PermissionLevel::System),
        authority_parent_event_id: None,
        expires_at: None,
    };
    event
}

#[tokio::test]
async fn system_internal_events_hold_system_permission_but_cannot_be_authority_parents() {
    let store = InMemoryEventStore::default();
    let task_id = TaskId::new();
    let goal = system_internal_ingress_event(task_id, ContextKind::SystemEvent, 1);
    let tool_result = system_internal_ingress_event(task_id, ContextKind::ToolResult, 2);
    store.append(&goal).await.unwrap();
    store.append(&tool_result).await.unwrap();
    let resolver = PersistedAuthorizationEvidenceResolver::new(&store);

    // 核心内部事件显式拥有 System 权限。
    let goal_evidence = resolver.resolve(task_id, goal.id).await.unwrap();
    assert_eq!(goal_evidence.permission, PermissionLevel::System);
    assert!(!goal_evidence.can_be_authority_parent());

    // 工具结果回传保持无权限，仅供分析。
    let result_evidence = resolver.resolve(task_id, tool_result.id).await.unwrap();
    assert_eq!(result_evidence.permission, PermissionLevel::None);
    assert!(!result_evidence.can_be_authority_parent());
}

#[tokio::test]
async fn tool_results_never_provide_authorization_even_if_persisted_incorrectly() {
    let store = InMemoryEventStore::default();
    let task_id = TaskId::new();
    let mut event = system_internal_ingress_event(task_id, ContextKind::ToolResult, 1);
    event.provenance.creator = EventSource::External(SourceName::new("qq").unwrap());
    if let AgentEvent::Ingress(ingress) = &mut event.payload {
        if let IngressEvent::ContextReceived {
            context,
            assessment,
        } = ingress.as_mut()
        {
            context.permission = PermissionLevel::Admin;
            assessment.suggested_permission = PermissionLevel::Admin;
            assessment.source_maximum_permission = PermissionLevel::Admin;
            assessment.identity_maximum_permission = PermissionLevel::Admin;
            assessment.effective_permission = PermissionLevel::Admin;
        }
    }
    store.append(&event).await.unwrap();

    let resolver = PersistedAuthorizationEvidenceResolver::new(&store);
    let evidence = resolver.resolve(task_id, event.id).await.unwrap();
    assert_eq!(evidence.permission, PermissionLevel::None);
    assert!(!evidence.is_usable());
}

#[tokio::test]
async fn denied_approval_never_provides_authorization() {
    let store = InMemoryEventStore::default();
    let task_id = TaskId::new();
    let assessment = PermissionAssessment::new(
        PermissionLevel::Admin,
        PermissionLevel::Admin,
        PermissionLevel::Admin,
    );
    let mut event = EventEnvelope::new(
        task_id,
        1,
        None,
        AgentEvent::ingress(IngressEvent::ApprovalSubmitted {
            approval_request_event_id: EventId::new(),
            principal: Principal::new("web", "alice"),
            scope: Scope::new("service", "order-api"),
            assessment,
            approved: false,
        }),
    );
    event.provenance = EventProvenance {
        creator: EventSource::External(SourceName::new("web").unwrap()),
        direct_permission: Some(PermissionLevel::Admin),
        authority_parent_event_id: None,
        expires_at: None,
    };
    store.append(&event).await.unwrap();

    let resolver = PersistedAuthorizationEvidenceResolver::new(&store);
    let evidence = resolver.resolve(task_id, event.id).await.unwrap();
    assert_eq!(evidence.permission, PermissionLevel::None);
    assert!(!evidence.is_usable());
}

#[tokio::test]
async fn cancellation_request_never_provides_authorization() {
    let store = InMemoryEventStore::default();
    let task_id = TaskId::new();
    let mut event = EventEnvelope::new(
        task_id,
        1,
        None,
        AgentEvent::ingress(IngressEvent::CancellationRequested {
            principal: Principal::new("web", "alice"),
            scope: Scope::new("service", "order-api"),
            assessment: PermissionAssessment::new(
                PermissionLevel::Admin,
                PermissionLevel::Admin,
                PermissionLevel::Admin,
            ),
            reason: "停止当前任务".into(),
        }),
    );
    event.provenance = EventProvenance {
        creator: EventSource::External(SourceName::new("web").unwrap()),
        direct_permission: Some(PermissionLevel::Admin),
        authority_parent_event_id: None,
        expires_at: None,
    };
    store.append(&event).await.unwrap();

    let resolver = PersistedAuthorizationEvidenceResolver::new(&store);
    let evidence = resolver.resolve(task_id, event.id).await.unwrap();
    assert_eq!(evidence.permission, PermissionLevel::None);
    assert!(!evidence.is_usable());
}
