use std::sync::Arc;

use koi_core::agent::{InputInjector, RuntimeError, TaskManager, TaskManagerError};
use koi_core::domain::{AgentEvent, ControlEvent, TaskId, TaskOperation, TaskStatus};
use koi_core::ports::{EventStore, InMemoryEventStore};

#[tokio::test]
async fn main_task_eventizes_child_creation_cancellation_and_recovery() {
    let manager = TaskManager::new(Arc::new(InMemoryEventStore::default()));
    let mut main = manager.open_main().await.unwrap();
    let mut child = manager.create_child(&mut main, None).await.unwrap();
    let child_id = child.task_id();
    assert_ne!(child_id, TaskId::MAIN);

    child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    child
        .runtime_mut()
        .record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    manager
        .cancel_child(&mut main, child_id, "用户中止", None)
        .await
        .unwrap();
    assert!(child.cancellation_token().is_cancelled());
    assert_eq!(manager.active_tasks().unwrap().len(), 2);

    drop(child);
    let recovered = manager
        .resume_child(&mut main, child_id, None)
        .await
        .unwrap();
    assert_eq!(recovered.runtime().projection().status, TaskStatus::Queued);
}

#[tokio::test]
async fn child_cannot_create_management_control_events_or_target_main() {
    let manager = TaskManager::new(Arc::new(InMemoryEventStore::default()));
    let mut main = manager.open_main().await.unwrap();
    let mut child = manager.create_child(&mut main, None).await.unwrap();

    let error = child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskOperationRequested {
                operation: TaskOperation::CreateChild,
            }),
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::ChildTaskManagementForbidden(_)
    ));
    assert!(matches!(
        manager
            .cancel_child(&mut main, TaskId::MAIN, "禁止", None)
            .await,
        Err(TaskManagerError::OperationRejected(_))
    ));
}

#[tokio::test]
async fn completed_child_result_is_eventized_and_injected_as_untrusted_tool_context() {
    let store = Arc::new(InMemoryEventStore::default());
    let manager = TaskManager::new(Arc::clone(&store));
    let mut main = manager.open_main().await.unwrap();
    let mut child = manager.create_child(&mut main, None).await.unwrap();
    let child_id = child.task_id();
    child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    child
        .runtime_mut()
        .record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();
    child
        .runtime_mut()
        .record(AgentEvent::control(ControlEvent::TaskResumed), None)
        .await
        .unwrap();
    let completed = child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("自动部署服务未运行".into()),
            }),
            None,
        )
        .await
        .unwrap();

    let result_event_id = manager
        .forward_child_result(&mut main, child_id, completed.id, None)
        .await
        .unwrap();
    let result = store
        .load_event(TaskId::MAIN, result_event_id)
        .await
        .unwrap()
        .unwrap();
    let model_context = InputInjector::default()
        .inject(TaskId::MAIN, &result)
        .unwrap();

    assert_eq!(model_context.role, koi_core::domain::ModelInputRole::Tool);
    assert_eq!(
        model_context.permission,
        koi_core::domain::PermissionLevel::None
    );
    assert!(model_context.content.contains("自动部署服务未运行"));
}
