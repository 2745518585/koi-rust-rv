use chrono::{Duration, Utc};
use koi_core::agent::{InputInjectionError, InputInjector};
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, EventEnvelope,
    EventProvenance, EventSource, IngressEvent, ModelInputRole, PermissionAssessment,
    PermissionLevel, Principal, Scope, SourceName, TaskId,
};

fn user_input_event(task_id: TaskId) -> EventEnvelope {
    let now = Utc::now();
    let permission = PermissionLevel::User;
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
                    native_event_id: "message-100".into(),
                },
                actor: Some(Principal::new("qq", "10001")),
                scope: Scope::new("qq_group", "42"),
                occurred_at: now,
                received_at: now,
                position: None,
                permission,
                payload: ContextPayload::Text {
                    text: "自动部署似乎挂了，帮我检查一下".into(),
                    mentions: vec!["bot".into()],
                },
                causation_id: None,
                content_hash: "test".into(),
            }),
            assessment: PermissionAssessment::new(permission, permission, permission),
        }),
    );
    event.provenance = EventProvenance {
        creator: EventSource::External(SourceName::new("qq").unwrap()),
        direct_permission: Some(permission),
        authority_parent_event_id: None,
        expires_at: None,
    };
    event
}

#[test]
fn injects_persisted_external_input_after_validation() {
    let task_id = TaskId::new();
    let event = user_input_event(task_id);

    let item = InputInjector::default().inject(task_id, &event).unwrap();

    assert_eq!(item.event_id, event.id);
    assert_eq!(item.role, ModelInputRole::User);
    assert_eq!(item.permission, PermissionLevel::User);
    assert_eq!(item.content, "自动部署似乎挂了，帮我检查一下");
}

#[test]
fn rejects_expired_input_before_model_injection() {
    let task_id = TaskId::new();
    let mut event = user_input_event(task_id);
    event.provenance.expires_at = Some(Utc::now() - Duration::seconds(1));

    let error = InputInjector::default()
        .inject(task_id, &event)
        .unwrap_err();

    assert!(matches!(error, InputInjectionError::Expired(event_id) if event_id == event.id));
}
