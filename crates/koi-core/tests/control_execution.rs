use std::sync::Arc;

use async_trait::async_trait;
use koi_core::agent::{
    ControlExecutionError, ControlExecutionRequest, ControlExecutor, DirectControlAuthority,
    TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, ControlEvent, EventEnvelope, PermissionLevel, Principal, SourceName, TaskId,
};
use koi_core::ports::{EventStore, EventStoreError};
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

fn authority(permission: PermissionLevel) -> DirectControlAuthority {
    DirectControlAuthority::external(
        SourceName::new("qq").unwrap(),
        Principal::new("qq", "10001"),
        permission,
        None,
    )
    .unwrap()
}

async fn running_runtime() -> TaskRuntime<MemoryEventStore> {
    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), TaskId::new());
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
    runtime
        .record(AgentEvent::control(ControlEvent::TaskResumed), None)
        .await
        .unwrap();
    runtime
}

#[tokio::test]
async fn controls_enforce_minimum_permission_and_persist_direct_source() {
    let mut runtime = running_runtime().await;

    let paused = ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::TaskPaused {
                reason: "人工检查".into(),
            },
            authority: authority(PermissionLevel::User),
            causation_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(paused.provenance.creator.as_str(), "qq");

    ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::MinimumControlPermissionChanged {
                minimum_permission: PermissionLevel::Operator,
            },
            authority: authority(PermissionLevel::Operator),
            causation_id: Some(paused.id),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        runtime.projection().minimum_control_permission,
        PermissionLevel::Operator
    );

    let error = ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::TaskResumed,
            authority: authority(PermissionLevel::User),
            causation_id: None,
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        ControlExecutionError::InsufficientPermission {
            required: PermissionLevel::Operator,
            actual: PermissionLevel::User,
        }
    ));

    ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::TaskResumed,
            authority: authority(PermissionLevel::Operator),
            causation_id: None,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn external_source_cannot_execute_internal_lifecycle_control() {
    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), TaskId::new());

    let error = ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::TaskCreated {
                trigger_event_id: None,
            },
            authority: authority(PermissionLevel::Admin),
            causation_id: None,
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ControlExecutionError::InternalControlEvent));
}

#[tokio::test]
async fn model_selection_is_a_direct_control_event_and_updates_projection() {
    let mut runtime = running_runtime().await;

    let selected = ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::ModelSelected {
                provider: "deepseek".into(),
                model_id: "deepseek-chat".into(),
            },
            authority: authority(PermissionLevel::User),
            causation_id: None,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        runtime.projection().selected_model,
        Some(koi_core::domain::ModelSelection::new("deepseek", "deepseek-chat").unwrap())
    );
    assert_eq!(selected.provenance.creator.as_str(), "qq");
}

#[tokio::test]
async fn model_selection_rejects_invalid_ids_before_persistence() {
    let mut runtime = running_runtime().await;

    let error = ControlExecutor::execute(
        &mut runtime,
        ControlExecutionRequest {
            event: ControlEvent::ModelSelected {
                provider: "deepseek".into(),
                model_id: "not a model".into(),
            },
            authority: authority(PermissionLevel::User),
            causation_id: None,
        },
    )
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        ControlExecutionError::InvalidModelSelection(_)
    ));
    assert_eq!(runtime.projection().selected_model, None);
}
