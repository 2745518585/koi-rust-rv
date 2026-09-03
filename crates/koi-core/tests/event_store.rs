use koi_core::agent::TaskRuntime;
use koi_core::domain::{AgentEvent, ControlEvent, TaskId, TaskStatus};
use koi_core::ports::{EventStore, InMemoryEventStore};

#[tokio::test]
async fn memory_store_reads_events_and_recovers_projection() {
    let task_id = TaskId::new();
    let mut runtime = TaskRuntime::new(InMemoryEventStore::default(), task_id);
    let created = runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskQueued),
            Some(created.id),
        )
        .await
        .unwrap();

    let store = runtime.into_store();
    assert_eq!(store.event_count(task_id).unwrap(), 2);
    assert_eq!(
        store
            .load_event(task_id, created.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        created.id
    );

    let recovered = TaskRuntime::recover(store, task_id).await.unwrap();
    assert_eq!(recovered.projection().status, TaskStatus::Queued);
    assert_eq!(recovered.projection().last_sequence, 2);
}
