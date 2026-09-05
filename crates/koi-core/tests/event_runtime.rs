use std::sync::Arc;

use async_trait::async_trait;
use koi_core::agent::TaskRuntime;
use koi_core::domain::{
    AgentEvent, ControlEvent, EventEnvelope, EventId, ModelEvent, ModelOutput, PermissionLevel,
    PolicyDecision, TaskId, TaskStatus, ToolCall, ToolEvent, Usage,
};
use koi_core::ports::{EventStore, EventStoreError};
use serde_json::json;
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

#[tokio::test]
async fn task_runtime_records_a_replayable_approval_flow() {
    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let task_id = TaskId::new();
    let mut runtime = TaskRuntime::new(store, task_id);

    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();

    let model_started = runtime
        .record(
            AgentEvent::model(ModelEvent::CallStarted {
                context_event_ids: vec![],
                context_hash: "test-context".into(),
                provider: "responses".into(),
                model_id: "test-model".into(),
            }),
            None,
        )
        .await
        .unwrap();

    let tool_call = ToolCall {
        name: "service_restart".into(),
        arguments: json!({"service": "koi-demo.service"}),
        provider_call_id: None,
        authority_parent_event_id: None,
    };
    let proposed = runtime
        .record(
            AgentEvent::tool(ToolEvent::Proposed {
                tool_call: tool_call.clone(),
            }),
            Some(model_started.id),
        )
        .await
        .unwrap();

    runtime
        .record(
            AgentEvent::tool(ToolEvent::AuthorizationChecked {
                proposal_event_id: proposed.id,
                decision: PolicyDecision::RequireApproval,
                effective_permission: PermissionLevel::User,
                evidence_event_ids: vec![],
            }),
            Some(proposed.id),
        )
        .await
        .unwrap();

    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskPaused {
                reason: "waiting for an approval".into(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(runtime.projection().status, TaskStatus::Paused);
    assert_eq!(runtime.projection().last_sequence, 5);
    assert_eq!(events.lock().await.len(), 5);
}

#[tokio::test]
async fn model_usage_is_accumulated_in_the_projection() {
    let task_id = TaskId::new();
    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), task_id);

    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();

    let model_started = runtime
        .record(
            AgentEvent::model(ModelEvent::CallStarted {
                context_event_ids: vec![],
                context_hash: "test-context".into(),
                provider: "responses".into(),
                model_id: "test-model".into(),
            }),
            None,
        )
        .await
        .unwrap();

    runtime
        .record(
            AgentEvent::model(ModelEvent::Completed {
                call_started_event_id: model_started.id,
                outputs: vec![ModelOutput::Text {
                    text: "done".into(),
                }],
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 7,
                    cached_input_tokens: Some(3),
                    reasoning_tokens: Some(2),
                },
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(runtime.projection().usage.input_tokens, 12);
    assert_eq!(runtime.projection().usage.output_tokens, 7);
    assert_eq!(runtime.projection().usage.cached_input_tokens, 3);
    assert_eq!(runtime.projection().usage.reasoning_tokens, 2);
}

#[test]
fn projection_rejects_out_of_order_events() {
    use koi_core::domain::TaskProjection;

    let task_id = TaskId::new();
    let mut projection = TaskProjection::new(task_id);
    let event = EventEnvelope::new(
        task_id,
        2,
        Some(EventId::new()),
        AgentEvent::control(ControlEvent::TaskCreated {
            trigger_event_id: None,
        }),
    );

    assert!(projection.apply(&event).is_err());
}

#[tokio::test]
async fn rejected_event_is_not_left_in_the_append_only_store() {
    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let task_id = TaskId::new();
    let mut runtime = TaskRuntime::new(store, task_id);
    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    runtime
        .record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    let error = runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("不应从 Queued 直接完成".into()),
            }),
            None,
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        koi_core::agent::RuntimeError::Projection(_)
    ));
    assert_eq!(events.lock().await.len(), 2);
    assert_eq!(runtime.projection().last_sequence, 2);
}

#[test]
fn queued_task_can_be_cancelled_before_a_worker_starts() {
    use koi_core::domain::{IngressEvent, PermissionAssessment, Principal, Scope, TaskProjection};

    let task_id = TaskId::new();
    let mut projection = TaskProjection::new(task_id);
    let events = [
        EventEnvelope::new(
            task_id,
            1,
            None,
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
        ),
        EventEnvelope::new(
            task_id,
            2,
            None,
            AgentEvent::control(ControlEvent::TaskQueued),
        ),
        EventEnvelope::new(
            task_id,
            3,
            None,
            AgentEvent::ingress(IngressEvent::CancellationRequested {
                principal: Principal::new("web", "operator"),
                scope: Scope::new("service", "order-api"),
                assessment: PermissionAssessment::new(
                    PermissionLevel::User,
                    PermissionLevel::User,
                    PermissionLevel::User,
                ),
                reason: "不再需要诊断".into(),
            }),
        ),
    ];

    for event in &events {
        projection.apply(event).unwrap();
    }

    assert_eq!(projection.status, TaskStatus::Cancelling);
}

#[test]
fn any_persistent_session_can_start_a_new_cycle_after_termination() {
    use koi_core::domain::TaskProjection;

    for task_id in [TaskId::MAIN, TaskId::new()] {
        let mut projection = TaskProjection::new(task_id);
        let events = [
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            AgentEvent::control(ControlEvent::TaskQueued),
            AgentEvent::control(ControlEvent::TaskCancelled {
                reason: "测试终止".into(),
            }),
        ];
        for (index, payload) in events.into_iter().enumerate() {
            projection
                .apply(&EventEnvelope::new(
                    task_id,
                    (index + 1) as u64,
                    None,
                    payload,
                ))
                .unwrap();
        }
        let restart = EventEnvelope::new(
            task_id,
            4,
            None,
            AgentEvent::control(ControlEvent::TaskQueued),
        );
        projection.apply(&restart).unwrap();
        assert_eq!(projection.status, TaskStatus::Queued);
    }
}
