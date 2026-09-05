use std::sync::Arc;

use koi_core::agent::{RuntimeError, TaskManager, TaskManagerError, TaskRuntime};
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, ControlEvent,
    EventProvenance, EventSource, IngressEvent, PermissionAssessment, PermissionLevel, Principal,
    Scope, TaskId, TaskOperation, TaskStatus,
};
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
#[allow(clippy::too_many_lines)]
async fn main_can_delegate_input_to_child_with_external_authority_parent() {
    let shared = Arc::new(InMemoryEventStore::default());
    let manager = TaskManager::new(Arc::new(Arc::clone(&shared)));
    let mut main = TaskRuntime::new(Arc::clone(&shared), TaskId::MAIN);
    main.record(
        AgentEvent::control(ControlEvent::TaskCreated {
            trigger_event_id: None,
        }),
        None,
    )
    .await
    .unwrap();
    main.record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    let mut created = manager.request_create_child(&mut main, None).await.unwrap();
    created
        .runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: Some(created.accepted_event_id),
            }),
            Some(created.accepted_event_id),
        )
        .await
        .unwrap();
    created
        .runtime
        .record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    let now = chrono::Utc::now();
    let assessment = PermissionAssessment::new(
        PermissionLevel::User,
        PermissionLevel::User,
        PermissionLevel::User,
    );
    let parent = main
        .record_with_provenance(
            AgentEvent::ingress(IngressEvent::ContextReceived {
                context: Box::new(ContextEnvelope {
                    schema_version: 1,
                    kind: ContextKind::UserMessage,
                    origin: ContextOrigin {
                        source: "qq".into(),
                        source_instance: "group-1".into(),
                        native_event_id: "message-1".into(),
                    },
                    actor: Some(Principal::new("qq", "10001")),
                    scope: Scope::new("qq_group", "1"),
                    occurred_at: now,
                    received_at: now,
                    position: None,
                    permission: PermissionLevel::User,
                    payload: ContextPayload::Text {
                        text: "请调查部署失败".into(),
                        mentions: vec!["bot".into()],
                    },
                    causation_id: None,
                    content_hash: "parent-input".into(),
                }),
                assessment,
            }),
            None,
            EventProvenance {
                creator: EventSource::External(koi_core::domain::SourceName::new("qq").unwrap()),
                direct_permission: Some(PermissionLevel::User),
                authority_parent_event_id: None,
                expires_at: None,
            },
        )
        .await
        .unwrap();

    let input_id = manager
        .request_input_child(
            &mut main,
            created.task_id,
            "请根据上面的请求调查部署失败并报告证据".into(),
            parent.id,
            PermissionLevel::User,
            None,
        )
        .await
        .unwrap();

    let child_events = shared.load_task(created.task_id).await.unwrap();
    let input = child_events
        .iter()
        .find(|event| event.id == input_id)
        .expect("子任务输入事件应已持久化");
    assert_eq!(input.provenance.creator, EventSource::Model);
    assert_eq!(input.provenance.authority_parent_event_id, Some(parent.id));
    let AgentEvent::Ingress(ingress) = &input.payload else {
        panic!("子任务投递事件必须是输入事件");
    };
    let IngressEvent::ContextReceived {
        context,
        assessment,
    } = ingress.as_ref()
    else {
        panic!("子任务投递事件必须是上下文输入");
    };
    assert_eq!(context.kind, ContextKind::UserMessage);
    assert_eq!(context.permission, PermissionLevel::User);
    assert_eq!(assessment.effective_permission, PermissionLevel::User);

    let injected = koi_core::agent::InputInjector::default()
        .inject(created.task_id, input, PermissionLevel::User)
        .unwrap();
    assert_eq!(injected.event_id, input_id);
    assert_eq!(injected.role, koi_core::domain::ModelInputRole::User);
    assert_eq!(injected.permission, PermissionLevel::User);

    // 当前执行周期结束后，向同一个子任务再次投递输入应开启新的周期，而不是创建一个
    // 新会话；旧周期的完成事件仍保留在事件流中。
    let mut finished_child = TaskRuntime::recover(Arc::clone(&shared), created.task_id)
        .await
        .unwrap();
    finished_child
        .record(AgentEvent::control(ControlEvent::TaskResumed), None)
        .await
        .unwrap();
    finished_child
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("第一轮完成".into()),
            }),
            None,
        )
        .await
        .unwrap();
    let second_input_id = manager
        .request_input_child(
            &mut main,
            created.task_id,
            "请继续检查刚才的服务".into(),
            parent.id,
            PermissionLevel::User,
            None,
        )
        .await
        .unwrap();
    let child_events = shared.load_task(created.task_id).await.unwrap();
    assert!(child_events.iter().any(|event| {
        event.sequence > 5
            && matches!(
                &event.payload,
                AgentEvent::Control(control)
                    if matches!(control.as_ref(), ControlEvent::TaskQueued)
            )
    }));
    assert_eq!(
        child_events
            .iter()
            .filter(|event| event.id == second_input_id)
            .count(),
        1
    );
    assert_eq!(
        TaskRuntime::recover(Arc::clone(&shared), created.task_id)
            .await
            .unwrap()
            .projection()
            .status,
        TaskStatus::Queued
    );
}

#[tokio::test]
async fn completed_child_result_is_delivered_as_tool_event_to_main() {
    let store = Arc::new(InMemoryEventStore::default());
    let manager = TaskManager::new(Arc::new(Arc::clone(&store)));
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
    child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("自动部署服务未运行".into()),
            }),
            None,
        )
        .await
        .unwrap();
    drop(child);
    drop(main);

    // 未通过 task.start 启动的子任务没有绑定主会话工具调用，回传应返回 None。
    let mut main_runtime = TaskRuntime::new(Arc::clone(&store), TaskId::MAIN);
    let delivered = manager
        .deliver_child_result(&mut main_runtime, child_id)
        .await
        .unwrap();
    assert!(delivered.is_none());
    // 主会话事件流中不应出现任何工具结果。
    assert!(
        store
            .load_task(TaskId::MAIN)
            .await
            .unwrap()
            .iter()
            .all(|event| !matches!(event.payload, AgentEvent::Tool(_)))
    );
}

#[tokio::test]
async fn persisted_children_can_be_listed_and_inspected_without_active_leases() {
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
    child
        .runtime_mut()
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("已确认服务恢复".into()),
            }),
            None,
        )
        .await
        .unwrap();
    drop(child);

    let summaries = manager.list_child_tasks().await.unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].task_id, child_id);
    assert_eq!(summaries[0].status, TaskStatus::Completed);
    assert!(summaries[0].has_result);

    let details = manager.inspect_child_task(child_id).await.unwrap();
    assert_eq!(details.summary, summaries[0]);
    assert!(details.latest_result_event_id.is_some());
    assert_eq!(details.latest_result.as_deref(), Some("已确认服务恢复"));
}

#[tokio::test]
async fn tool_started_child_delivers_final_output_as_tool_event() {
    use koi_core::agent::CreatedChild;
    use koi_core::domain::{EventProvenance, EventSource, ToolEvent};

    // 共享存储模式：管理器与主会话运行时持有同一存储句柄的不同包装，
    // 使 lease 型 API 与 request 型 API 可以在类型上互通。
    let shared = Arc::new(InMemoryEventStore::default());
    let manager = TaskManager::new(Arc::new(Arc::clone(&shared)));
    let mut main = TaskRuntime::new(Arc::clone(&shared), TaskId::MAIN);
    main.record(
        AgentEvent::control(ControlEvent::TaskCreated {
            trigger_event_id: None,
        }),
        None,
    )
    .await
    .unwrap();
    main.record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    // 模拟主循环通过统一入口创建子任务。
    let CreatedChild {
        task_id,
        accepted_event_id,
        mut runtime,
        ..
    } = manager.request_create_child(&mut main, None).await.unwrap();
    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: Some(accepted_event_id),
            }),
            Some(accepted_event_id),
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
    // 主会话在 task.start 的工具调用链中记录 Started，causation 指向 Accepted 事件。
    let started = main
        .record_with_provenance(
            AgentEvent::tool(ToolEvent::Started {
                proposal_event_id: accepted_event_id,
            }),
            Some(accepted_event_id),
            EventProvenance::tool(),
        )
        .await
        .unwrap();
    runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCompleted {
                response: Some("子任务最终结论".into()),
            }),
            None,
        )
        .await
        .unwrap();

    let delivered = manager
        .deliver_child_result(&mut main, task_id)
        .await
        .unwrap()
        .expect("子任务结果应回传为主会话工具事件");
    assert_eq!(delivered.started_event_id, started.id);
    assert!(delivered.result.summary.contains("子任务最终结论"));

    let finished = shared
        .load_event(TaskId::MAIN, delivered.finished_event_id)
        .await
        .unwrap()
        .unwrap();
    match finished.payload {
        AgentEvent::Tool(ref tool) => {
            assert!(matches!(
                tool.as_ref(),
                ToolEvent::Finished { execution_started_event_id, .. }
                    if *execution_started_event_id == started.id
            ));
        }
        _ => panic!("回传事件必须是工具事件"),
    }
    assert_eq!(finished.provenance.creator, EventSource::Tool);
    // causation 指向子任务的终止事件，形成完整证据链。
    assert!(finished.causation_id.is_some());
    assert_eq!(finished.task_id, TaskId::MAIN);

    // 幂等：重复回传不会产生第二条工具结果。
    let again = manager
        .deliver_child_result(&mut main, task_id)
        .await
        .unwrap();
    assert!(again.is_none());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn name_control_and_delete_children_through_main_stream() {
    use koi_core::domain::PermissionLevel;

    let shared = Arc::new(InMemoryEventStore::default());
    let manager = TaskManager::new(Arc::new(Arc::clone(&shared)));
    let mut main = TaskRuntime::new(Arc::clone(&shared), TaskId::MAIN);
    main.record(
        AgentEvent::control(ControlEvent::TaskCreated {
            trigger_event_id: None,
        }),
        None,
    )
    .await
    .unwrap();
    main.record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();

    let mut created = manager.request_create_child(&mut main, None).await.unwrap();
    let child_id = created.task_id;
    created
        .runtime
        .record(
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: Some(created.accepted_event_id),
            }),
            Some(created.accepted_event_id),
        )
        .await
        .unwrap();
    created
        .runtime
        .record(AgentEvent::control(ControlEvent::TaskQueued), None)
        .await
        .unwrap();
    created
        .runtime
        .record(AgentEvent::control(ControlEvent::TaskResumed), None)
        .await
        .unwrap();

    // 命名：投影标题更新，主会话留有请求与接受审计。
    manager
        .request_name_child(&mut main, child_id, " nightly 部署巡检 ", None)
        .await
        .unwrap();
    let child_runtime = TaskRuntime::recover(Arc::new(Arc::clone(&shared)), child_id)
        .await
        .unwrap();
    assert_eq!(
        child_runtime.projection().title.as_deref(),
        Some("nightly 部署巡检")
    );

    // 命名主会话必须被拒绝。
    assert!(matches!(
        manager
            .request_name_child(&mut main, TaskId::MAIN, "主会话", None)
            .await,
        Err(TaskManagerError::OperationRejected(_))
    ));

    // 控制：暂停子任务（System 权限，经 ControlExecutor 写入子任务流）。
    manager
        .request_control_child(
            &mut main,
            child_id,
            ControlEvent::TaskPaused {
                reason: "等待维护窗口".into(),
            },
            None,
        )
        .await
        .unwrap();
    let child_runtime = TaskRuntime::recover(Arc::new(Arc::clone(&shared)), child_id)
        .await
        .unwrap();
    assert_eq!(child_runtime.projection().status, TaskStatus::Paused);

    // 删除未终止的子任务必须先取消。
    assert!(matches!(
        manager
            .request_delete_child(&mut main, child_id, "测试", None)
            .await,
        Err(TaskManagerError::OperationRejected(_))
    ));

    manager
        .request_control_child(
            &mut main,
            child_id,
            ControlEvent::TaskCancelled {
                reason: "测试结束".into(),
            },
            None,
        )
        .await
        .unwrap();
    let child_runtime = TaskRuntime::recover(Arc::new(Arc::clone(&shared)), child_id)
        .await
        .unwrap();
    assert_eq!(child_runtime.projection().status, TaskStatus::Cancelled);

    manager
        .request_delete_child(&mut main, child_id, "测试结束删除", None)
        .await
        .unwrap();
    assert!(shared.load_task(child_id).await.unwrap().is_empty());
    // 删除主会话必须被拒绝。
    assert!(matches!(
        manager
            .request_delete_child(&mut main, TaskId::MAIN, "禁止", None)
            .await,
        Err(TaskManagerError::OperationRejected(_))
    ));

    // 最低控制权限变更仍受 User 下限约束。
    manager
        .request_control_child(
            &mut main,
            TaskId::new(),
            ControlEvent::MinimumControlPermissionChanged {
                minimum_permission: PermissionLevel::Operator,
            },
            None,
        )
        .await
        .unwrap_err();
}
