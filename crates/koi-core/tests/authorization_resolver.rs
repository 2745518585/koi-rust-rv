use chrono::Utc;
use koi_core::agent::PersistedAuthorizationEvidenceResolver;
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, EventEnvelope,
    EventProvenance, EventSource, IngressEvent, PermissionAssessment, PermissionLevel, Principal,
    Scope, SourceName, TaskId,
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
