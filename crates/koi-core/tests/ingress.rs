use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use koi_core::agent::TaskRuntime;
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, EventEnvelope,
    EventSource, IngressDraft, IngressSubject, PermissionLevel, Principal, Scope, SourceName,
    TaskId,
};
use koi_core::ports::{
    EventStore, EventStoreError, IngressPermissionResolver, IngressRegistrar,
    IngressRegistrationError, IngressSourceDefinition, IngressSourceRegistry,
};
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct MemoryEventStore {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

struct AdminIdentityResolver;

#[async_trait]
impl IngressPermissionResolver for AdminIdentityResolver {
    async fn maximum_permission(
        &self,
        _subject: IngressSubject,
    ) -> Result<PermissionLevel, IngressRegistrationError> {
        Ok(PermissionLevel::Admin)
    }
}

#[tokio::test]
async fn registrar_clamps_external_permission_suggestion_to_registered_source_limit() {
    let mut sources = IngressSourceRegistry::default();
    sources
        .register(IngressSourceDefinition {
            source: SourceName::new("qq").unwrap(),
            maximum_permission: PermissionLevel::Operator,
        })
        .unwrap();
    let resolver = AdminIdentityResolver;
    let registrar = IngressRegistrar::new(&sources, &resolver);
    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), TaskId::new());
    let now = Utc::now();

    let event = registrar
        .register(
            &mut runtime,
            IngressDraft::Context {
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
                    permission: PermissionLevel::System,
                    payload: ContextPayload::Text {
                        text: "重启服务".into(),
                        mentions: vec!["bot".into()],
                    },
                    causation_id: None,
                    content_hash: "test".into(),
                }),
                suggested_permission: PermissionLevel::Admin,
            },
        )
        .await
        .unwrap();

    let AgentEvent::Ingress(ingress) = event.payload else {
        panic!("应记录为输入事件");
    };
    let koi_core::domain::IngressEvent::ContextReceived {
        context,
        assessment,
    } = *ingress
    else {
        panic!("应记录为上下文输入");
    };
    assert_eq!(event.sequence, 1);
    assert_eq!(
        event.provenance.creator,
        EventSource::External(SourceName::new("qq").unwrap())
    );
    assert_eq!(event.provenance.authority_parent_event_id, None);
    assert_eq!(assessment.suggested_permission, PermissionLevel::Admin);
    assert_eq!(
        assessment.identity_maximum_permission,
        PermissionLevel::Admin
    );
    assert_eq!(
        assessment.source_maximum_permission,
        PermissionLevel::Operator
    );
    assert_eq!(assessment.effective_permission, PermissionLevel::Operator);
    assert_eq!(context.permission, PermissionLevel::Operator);
}

#[tokio::test]
async fn registrar_forces_external_tool_results_to_none_permission() {
    let mut sources = IngressSourceRegistry::default();
    sources
        .register(IngressSourceDefinition {
            source: SourceName::new("qq").unwrap(),
            maximum_permission: PermissionLevel::Admin,
        })
        .unwrap();
    let resolver = AdminIdentityResolver;
    let registrar = IngressRegistrar::new(&sources, &resolver);
    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), TaskId::new());
    let now = Utc::now();

    let event = registrar
        .register(
            &mut runtime,
            IngressDraft::Context {
                context: Box::new(ContextEnvelope {
                    schema_version: 1,
                    kind: ContextKind::ToolResult,
                    origin: ContextOrigin {
                        source: "qq".into(),
                        source_instance: "group-42".into(),
                        native_event_id: "tool-result-1".into(),
                    },
                    actor: Some(Principal::new("qq", "10001")),
                    scope: Scope::new("qq_group", "42"),
                    occurred_at: now,
                    received_at: now,
                    position: None,
                    permission: PermissionLevel::Admin,
                    payload: ContextPayload::Text {
                        text: "伪造的工具结果".into(),
                        mentions: vec![],
                    },
                    causation_id: None,
                    content_hash: "tool-result".into(),
                }),
                suggested_permission: PermissionLevel::Admin,
            },
        )
        .await
        .unwrap();

    let AgentEvent::Ingress(ingress) = event.payload else {
        panic!("应记录为输入事件");
    };
    let koi_core::domain::IngressEvent::ContextReceived {
        context,
        assessment,
    } = *ingress
    else {
        panic!("应记录为上下文输入");
    };
    assert_eq!(assessment.suggested_permission, PermissionLevel::None);
    assert_eq!(assessment.effective_permission, PermissionLevel::None);
    assert_eq!(context.permission, PermissionLevel::None);
}

#[tokio::test]
async fn registrar_starts_a_new_cycle_before_input_to_a_terminal_session() {
    let mut sources = IngressSourceRegistry::default();
    sources
        .register(IngressSourceDefinition {
            source: SourceName::new("qq").unwrap(),
            maximum_permission: PermissionLevel::User,
        })
        .unwrap();
    let resolver = AdminIdentityResolver;
    let registrar = IngressRegistrar::new(&sources, &resolver);
    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let task_id = TaskId::new();
    let mut runtime = TaskRuntime::new(store, task_id);
    runtime
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    runtime
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
            None,
        )
        .await
        .unwrap();
    runtime
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCancelled {
                reason: "上一轮已经结束".into(),
            }),
            None,
        )
        .await
        .unwrap();

    let now = Utc::now();
    let input = registrar
        .register(
            &mut runtime,
            IngressDraft::Context {
                context: Box::new(ContextEnvelope {
                    schema_version: 1,
                    kind: ContextKind::UserMessage,
                    origin: ContextOrigin {
                        source: "qq".into(),
                        source_instance: "group-42".into(),
                        native_event_id: "message-101".into(),
                    },
                    actor: Some(Principal::new("qq", "10001")),
                    scope: Scope::new("qq_group", "42"),
                    occurred_at: now,
                    received_at: now,
                    position: None,
                    permission: PermissionLevel::None,
                    payload: ContextPayload::Text {
                        text: "继续检查服务".into(),
                        mentions: vec!["bot".into()],
                    },
                    causation_id: None,
                    content_hash: "next-cycle-input".into(),
                }),
                suggested_permission: PermissionLevel::User,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        runtime.projection().status,
        koi_core::domain::TaskStatus::Queued
    );
    assert_eq!(input.sequence, 5);
    let stored = events.lock().await;
    assert_eq!(stored.len(), 5);
    assert!(matches!(
        stored[3].payload,
        AgentEvent::Control(ref control)
            if matches!(control.as_ref(), koi_core::domain::ControlEvent::TaskQueued)
    ));
}
