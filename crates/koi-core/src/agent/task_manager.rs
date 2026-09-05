use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    ControlExecutionRequest, ControlExecutor, DirectControlAuthority, RuntimeError,
    RuntimeRecoveryError, TaskRuntime,
};
use crate::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, ControlEvent,
    EventEnvelope, EventId, EventProvenance, IngressEvent, PermissionAssessment, PermissionLevel,
    Scope, TaskId, TaskOperation, ToolEvent, ToolResult,
};
use crate::ports::EventStore;

/// 进程内任务管理器。跨任务写操作仅接受 `MainTaskLease`。
pub struct TaskManager<S: EventStore> {
    store: Arc<S>,
    tasks: Arc<Mutex<HashMap<TaskId, ManagedTask>>>,
}

struct ManagedTask {
    cancel: CancellationToken,
}

impl<S: EventStore> TaskManager<S> {
    #[must_use]
    pub fn new(store: Arc<S>) -> Self {
        Self {
            store,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 打开全局唯一的主会话任务。首次打开时由后续主循环写入生命周期事件。
    ///
    /// # Errors
    ///
    /// 当主会话已运行、读取事件失败或重放失败时返回错误。
    pub async fn open_main(&self) -> Result<MainTaskLease<S>, TaskManagerError> {
        let events = self.store.load_task(TaskId::MAIN).await?;
        let runtime = if events.is_empty() {
            TaskRuntime::new(Arc::clone(&self.store), TaskId::MAIN)
        } else {
            TaskRuntime::recover(Arc::clone(&self.store), TaskId::MAIN).await?
        };
        let cancel = CancellationToken::new();
        self.register(TaskId::MAIN, cancel.clone())?;
        Ok(MainTaskLease(TaskLease::new(
            TaskId::MAIN,
            runtime,
            cancel,
            Arc::clone(&self.tasks),
        )))
    }

    /// 创建子任务，并在主会话事件流中追加请求和接受结果。
    ///
    /// # Errors
    ///
    /// 当主会话事件无法写入或任务注册失败时返回错误。
    pub async fn create_child(
        &self,
        main: &mut MainTaskLease<S>,
        causation_id: Option<EventId>,
    ) -> Result<ChildTaskLease<S>, TaskManagerError> {
        let request = self
            .record_request(
                &mut main.0.runtime,
                TaskOperation::CreateChild,
                causation_id,
            )
            .await?;
        let task_id = TaskId::new();
        let cancel = CancellationToken::new();
        self.register(task_id, cancel.clone())?;
        if let Err(error) = self.accept(&mut main.0.runtime, request.id, task_id).await {
            self.unregister(task_id);
            return Err(error);
        }
        Ok(ChildTaskLease(TaskLease::new(
            task_id,
            TaskRuntime::new(Arc::clone(&self.store), task_id),
            cancel,
            Arc::clone(&self.tasks),
        )))
    }

    /// 恢复子任务，并在主会话事件流中追加请求和结果。
    ///
    /// # Errors
    ///
    /// 当子任务不存在、已终止、正在运行或事件流无法恢复时返回错误。
    pub async fn resume_child(
        &self,
        main: &mut MainTaskLease<S>,
        task_id: TaskId,
        causation_id: Option<EventId>,
    ) -> Result<ChildTaskLease<S>, TaskManagerError> {
        let request = self
            .record_request(
                &mut main.0.runtime,
                TaskOperation::ResumeChild { task_id },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(
                    &mut main.0.runtime,
                    request.id,
                    "任务管理接口不能恢复主会话",
                )
                .await;
        }
        let runtime = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(runtime) if !runtime.projection().status.is_terminal() => runtime,
            Ok(runtime) => {
                return self
                    .reject(
                        &mut main.0.runtime,
                        request.id,
                        format!("目标子任务已终止：{:?}", runtime.projection().status),
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .reject(&mut main.0.runtime, request.id, error.to_string())
                    .await;
            }
        };
        let cancel = CancellationToken::new();
        if let Err(error) = self.register(task_id, cancel.clone()) {
            return self
                .reject(&mut main.0.runtime, request.id, error.to_string())
                .await;
        }
        if let Err(error) = self.accept(&mut main.0.runtime, request.id, task_id).await {
            self.unregister(task_id);
            return Err(error);
        }
        Ok(ChildTaskLease(TaskLease::new(
            task_id,
            runtime,
            cancel,
            Arc::clone(&self.tasks),
        )))
    }

    /// 将取消请求事件化后投递给正在运行的子任务。
    ///
    /// # Errors
    ///
    /// 当目标是主会话、目标未运行或主会话事件无法写入时返回错误。
    pub async fn cancel_child(
        &self,
        main: &mut MainTaskLease<S>,
        task_id: TaskId,
        reason: impl Into<String>,
        causation_id: Option<EventId>,
    ) -> Result<(), TaskManagerError> {
        let request = self
            .record_request(
                &mut main.0.runtime,
                TaskOperation::CancelChild {
                    task_id,
                    reason: reason.into(),
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(
                    &mut main.0.runtime,
                    request.id,
                    "任务管理接口不能控制主会话",
                )
                .await;
        }
        let cancel = {
            let tasks = self.lock_tasks()?;
            tasks.get(&task_id).map(|task| task.cancel.clone())
        };
        let Some(cancel) = cancel else {
            return self
                .reject(
                    &mut main.0.runtime,
                    request.id,
                    "目标子任务未在当前进程运行",
                )
                .await;
        };
        cancel.cancel();
        self.accept(&mut main.0.runtime, request.id, task_id)
            .await?;
        Ok(())
    }

    /// 通过主会话事件流创建子任务（统一入口：主会话特殊工具与外部适配器共用）。
    ///
    /// 在主会话事件流中记录 `TaskOperationRequested -> TaskOperationAccepted`，然后返回
    /// 新子任务的运行时；调用方负责写入子任务的生命周期与首条输入事件。
    ///
    /// # Errors
    ///
    /// 当主会话事件无法写入或任务注册失败时返回错误。
    pub async fn request_create_child(
        &self,
        main: &mut TaskRuntime<S>,
        causation_id: Option<EventId>,
    ) -> Result<CreatedChild<S>, TaskManagerError> {
        let request = self
            .record_request(main, TaskOperation::CreateChild, causation_id)
            .await?;
        let task_id = TaskId::new();
        let cancel = CancellationToken::new();
        self.register(task_id, cancel.clone())?;
        let accepted = match self.accept(main, request.id, task_id).await {
            Ok(accepted) => accepted,
            Err(error) => {
                self.unregister(task_id);
                return Err(error);
            }
        };
        Ok(CreatedChild {
            task_id,
            requested_event_id: request.id,
            accepted_event_id: accepted.id,
            runtime: TaskRuntime::new(Arc::clone(&self.store), task_id),
        })
    }

    /// 通过主会话事件流向子任务投递一条输入事件。
    ///
    /// 输入事件由模型发起，但不获得模型自身的权限；它必须引用主会话中的直接外部
    /// 输入作为授权父节点。核心重新读取并核对该父节点的已持久化权限，然后将核定的
    /// 权限快照写入子任务输入，事件本身仍以 `Model` 来源持久化。子任务后续审查工具
    /// 调用时，会从这条输入继续沿 `authority_parent_event_id` 跨任务追溯到主会话输入。
    ///
    /// # Errors
    ///
    /// 当目标为主会话、父事件不存在或不具备直接外部输入权限、权限快照不一致、目标
    /// 子任务不存在或事件流无法写入时返回错误。已结束的子任务会在追加输入前开启
    /// 一个新的排队周期，因此仍可作为持久化会话继续使用。
    pub async fn request_input_child(
        &self,
        main: &mut TaskRuntime<S>,
        task_id: TaskId,
        message: String,
        authority_parent_event_id: EventId,
        effective_permission: PermissionLevel,
        causation_id: Option<EventId>,
    ) -> Result<EventId, TaskManagerError> {
        let request = self
            .record_request(
                main,
                TaskOperation::InputChild {
                    task_id,
                    message: message.clone(),
                    authority_parent_event_id,
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能向主会话投递输入")
                .await;
        }
        if !effective_permission.can_authorize() {
            return self
                .reject(main, request.id, "子任务输入必须具有有效的授权父事件")
                .await;
        }

        let main_events = main.load_events().await?;
        let Some(parent_permission) = main_events
            .iter()
            .find(|event| event.id == authority_parent_event_id)
            .and_then(direct_external_input_permission)
        else {
            return self
                .reject(
                    main,
                    request.id,
                    "授权父事件必须是主会话中未过期的直接外部用户输入或告警",
                )
                .await;
        };
        if parent_permission != effective_permission {
            return self
                .reject(
                    main,
                    request.id,
                    format!(
                        "子任务输入的权限快照与授权父事件不一致：{effective_permission:?} != {parent_permission:?}"
                    ),
                )
                .await;
        }

        let mut child = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(child) => child,
            Err(error) => return self.reject(main, request.id, error.to_string()).await,
        };
        if let Err(error) = child.start_new_cycle_if_terminal(Some(request.id)).await {
            return self.reject(main, request.id, error.to_string()).await;
        }

        let now = Utc::now();
        let input = child
            .record_with_provenance(
                AgentEvent::ingress(IngressEvent::ContextReceived {
                    context: Box::new(ContextEnvelope {
                        schema_version: 1,
                        kind: ContextKind::UserMessage,
                        origin: ContextOrigin {
                            source: "internal-task".into(),
                            source_instance: "main-session".into(),
                            native_event_id: request.id.to_string(),
                        },
                        actor: None,
                        scope: Scope::new("task", task_id.to_string()),
                        occurred_at: now,
                        received_at: now,
                        position: None,
                        permission: effective_permission,
                        payload: ContextPayload::Text {
                            text: message.clone(),
                            mentions: Vec::new(),
                        },
                        causation_id: Some(request.id),
                        content_hash: format!("task-input:{request}", request = request.id),
                    }),
                    assessment: PermissionAssessment::new(
                        effective_permission,
                        effective_permission,
                        effective_permission,
                    ),
                }),
                Some(request.id),
                EventProvenance::model(Some(authority_parent_event_id)),
            )
            .await?;
        self.accept(main, request.id, task_id).await?;
        Ok(input.id)
    }

    /// 通过主会话事件流为子任务设置显示名称。
    ///
    /// # Errors
    ///
    /// 当目标为主会话、不存在、已终止、名称非法或主会话事件无法写入时返回错误。
    pub async fn request_name_child(
        &self,
        main: &mut TaskRuntime<S>,
        task_id: TaskId,
        name: &str,
        causation_id: Option<EventId>,
    ) -> Result<EventId, TaskManagerError> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.chars().count() > 128 {
            return Err(TaskManagerError::OperationRejected(
                "任务名称必须为 1 到 128 个字符".into(),
            ));
        }
        let request = self
            .record_request(
                main,
                TaskOperation::NameChild {
                    task_id,
                    name: trimmed.to_owned(),
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能命名主会话")
                .await;
        }
        let mut child = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(child) if !child.projection().status.is_terminal() => child,
            Ok(child) => {
                return self
                    .reject(
                        main,
                        request.id,
                        format!("目标子任务已终止：{:?}", child.projection().status),
                    )
                    .await;
            }
            Err(error) => return self.reject(main, request.id, error.to_string()).await,
        };
        let named = child
            .record(
                AgentEvent::control(ControlEvent::TaskNamed {
                    name: trimmed.to_owned(),
                }),
                Some(request.id),
            )
            .await?;
        self.accept(main, request.id, task_id).await?;
        Ok(named.id)
    }

    /// 通过主会话事件流删除一个已终止的子任务事件流。
    ///
    /// # Errors
    ///
    /// 当目标为主会话、不存在、尚未终止、存储不支持删除或主会话事件无法写入时返回错误。
    pub async fn request_delete_child(
        &self,
        main: &mut TaskRuntime<S>,
        task_id: TaskId,
        reason: impl Into<String>,
        causation_id: Option<EventId>,
    ) -> Result<(), TaskManagerError> {
        let request = self
            .record_request(
                main,
                TaskOperation::DeleteChild {
                    task_id,
                    reason: reason.into(),
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能删除主会话")
                .await;
        }
        let status = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(child) => child.projection().status,
            Err(error) => return self.reject(main, request.id, error.to_string()).await,
        };
        if !status.is_terminal() {
            return self
                .reject(
                    main,
                    request.id,
                    format!("子任务尚未终止（{status:?}），请先取消再删除"),
                )
                .await;
        }
        if let Err(error) = self.store.delete_task(task_id).await {
            return self
                .reject(main, request.id, format!("删除子任务事件流失败：{error}"))
                .await;
        }
        self.unregister(task_id);
        self.accept(main, request.id, task_id).await?;
        Ok(())
    }

    /// 通过主会话事件流向子任务投递一条控制事件。
    ///
    /// 主会话（或经主会话转发的管理请求）是核心内部控制来源：控制事件以 `System`
    /// 直接权限写入子任务事件流，但仍必须在主会话流中留有请求与结果审计。
    ///
    /// # Errors
    ///
    /// 当目标为主会话、不存在、控制事件非法或主会话事件无法写入时返回错误。
    pub async fn request_control_child(
        &self,
        main: &mut TaskRuntime<S>,
        task_id: TaskId,
        control: ControlEvent,
        causation_id: Option<EventId>,
    ) -> Result<(), TaskManagerError> {
        if !is_manageable_control(&control) {
            return Err(TaskManagerError::OperationRejected(
                "主会话只能向子任务投递暂停、恢复、取消、模型选择或最低权限控制事件".into(),
            ));
        }
        let request = self
            .record_request(
                main,
                TaskOperation::ControlChild {
                    task_id,
                    control: Box::new(control.clone()),
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能控制主会话")
                .await;
        }
        let mut child = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(child) => child,
            Err(error) => return self.reject(main, request.id, error.to_string()).await,
        };
        let is_cancellation = matches!(control, ControlEvent::TaskCancelled { .. });
        if let Err(error) = ControlExecutor::execute(
            &mut child,
            ControlExecutionRequest {
                event: control,
                authority: DirectControlAuthority::system(),
                causation_id: Some(request.id),
            },
        )
        .await
        {
            return self.reject(main, request.id, error.to_string()).await;
        }
        if is_cancellation {
            if let Ok(tasks) = self.lock_tasks() {
                if let Some(task) = tasks.get(&task_id) {
                    task.cancel.cancel();
                }
            }
        }
        self.accept(main, request.id, task_id).await?;
        Ok(())
    }

    /// 将已终止子任务的最终输出回传为主会话事件流中的工具事件。
    ///
    /// 回传事件是 `ToolEvent::Finished`，绑定到主会话调用 `task.start` 时记录的
    /// `ToolEvent::Started`；重复调用是安全的，未绑定主会话工具调用的子任务返回
    /// `None`。
    ///
    /// 回传是一条显式的无权限限制通道：工具事件以 `Tool` 来源、`None` 直接权限
    /// 持久化，不经过会话最低控制权限审查即可进入主会话。安全性由授权规则保证——
    /// 工具事件永远不能作为权限父节点参与提权审查，因此它只能被模型阅读，不能
    /// 带来任何工具授权。
    ///
    /// # Errors
    ///
    /// 当事件读取或主会话写入失败时返回错误。
    pub async fn deliver_child_result(
        &self,
        main: &mut TaskRuntime<S>,
        task_id: TaskId,
    ) -> Result<Option<DeliveredChildResult>, TaskManagerError> {
        if task_id.is_main() {
            return Ok(None);
        }
        let child_events = self.store.load_task(task_id).await?;
        let Some(trigger_event_id) = child_events.iter().find_map(|event| match &event.payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::TaskCreated {
                    trigger_event_id: Some(trigger),
                } => Some(*trigger),
                _ => None,
            },
            _ => None,
        }) else {
            return Ok(None);
        };
        let outcome = child_outcome_summary(&child_events);
        let Some((terminal_event_id, summary)) = outcome else {
            return Ok(None);
        };

        let main_events = self.store.load_task(TaskId::MAIN).await?;
        let Some(started_event_id) = main_events.iter().find_map(|event| {
            if event.causation_id != Some(trigger_event_id) {
                return None;
            }
            let AgentEvent::Tool(tool) = &event.payload else {
                return None;
            };
            match tool.as_ref() {
                ToolEvent::Started { .. } => Some(event.id),
                _ => None,
            }
        }) else {
            return Ok(None);
        };
        let already_delivered = main_events.iter().any(|event| {
            matches!(
                &event.payload,
                AgentEvent::Tool(tool)
                    if matches!(tool.as_ref(), ToolEvent::Finished { execution_started_event_id, .. } if *execution_started_event_id == started_event_id)
            )
        });
        if already_delivered {
            return Ok(None);
        }

        let result = ToolResult {
            summary,
            data: serde_json::json!({
                "task_id": task_id.to_string(),
                "terminal_event_id": terminal_event_id.to_string(),
            }),
            truncated: false,
        };
        let finished = main
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Finished {
                    execution_started_event_id: started_event_id,
                    result: result.clone(),
                }),
                Some(terminal_event_id),
                EventProvenance::tool(),
            )
            .await?;
        Ok(Some(DeliveredChildResult {
            task_id,
            finished_event_id: finished.id,
            started_event_id,
            result,
        }))
    }

    /// 列出事件存储中已经存在的所有子任务。
    ///
    /// 这个查询故意不依赖当前进程中的活动租约，因此服务重启后仍能发现历史任务。
    /// 主会话不会出现在结果中；删除任务后其事件流也不会再被列出。
    ///
    /// # Errors
    ///
    /// 当任务枚举、事件读取或任务投影恢复失败时返回错误。
    pub async fn list_child_tasks(&self) -> Result<Vec<TaskSummary>, TaskManagerError> {
        let mut summaries = Vec::new();
        for task_id in self.store.list_task_ids().await? {
            if task_id.is_main() {
                continue;
            }
            let events = self.store.load_task(task_id).await?;
            if events.is_empty() {
                continue;
            }
            let runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id).await?;
            let status = runtime.projection().status;
            let has_result = status.is_terminal() && child_outcome_summary(&events).is_some();
            summaries.push(TaskSummary {
                task_id,
                status,
                title: runtime.projection().title.clone(),
                last_sequence: runtime.projection().last_sequence,
                has_result,
            });
        }
        summaries.sort_by_key(|summary| summary.task_id.to_string());
        Ok(summaries)
    }

    /// 读取一个已经存在的子任务及其当前执行周期的最终结果。
    ///
    /// 只有当前状态处于终态时才暴露最终结果。任务重新排队后，上一轮的旧结果不会被
    /// 误报成当前任务结果；调用方可以等待任务再次进入终态后重新查询。
    ///
    /// # Errors
    ///
    /// 当目标不是已存在的子任务，或事件读取、任务投影恢复失败时返回错误。
    pub async fn inspect_child_task(
        &self,
        task_id: TaskId,
    ) -> Result<TaskDetails, TaskManagerError> {
        if task_id.is_main() {
            return Err(TaskManagerError::OperationRejected(
                "任务查询接口不能查询主会话".into(),
            ));
        }
        let events = self.store.load_task(task_id).await?;
        if events.is_empty() {
            return Err(TaskManagerError::OperationRejected(format!(
                "目标子任务不存在：{task_id}"
            )));
        }
        let runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id).await?;
        let status = runtime.projection().status;
        let outcome = status
            .is_terminal()
            .then(|| child_outcome_summary(&events))
            .flatten();
        Ok(TaskDetails {
            summary: TaskSummary {
                task_id,
                status,
                title: runtime.projection().title.clone(),
                last_sequence: runtime.projection().last_sequence,
                has_result: outcome.is_some(),
            },
            latest_result_event_id: outcome.as_ref().map(|(event_id, _)| *event_id),
            latest_result: outcome.map(|(_, summary)| summary),
        })
    }

    /// 只读查询当前进程活动任务。
    ///
    /// # Errors
    ///
    /// 当管理器状态锁不可用时返回错误。
    pub fn active_tasks(&self) -> Result<Vec<ActiveTask>, TaskManagerError> {
        let tasks = self.lock_tasks()?;
        Ok(tasks
            .iter()
            .map(|(task_id, task)| ActiveTask {
                task_id: *task_id,
                is_main: task_id.is_main(),
                cancellation_requested: task.cancel.is_cancelled(),
            })
            .collect())
    }

    async fn record_request<RT>(
        &self,
        main: &mut TaskRuntime<RT>,
        operation: TaskOperation,
        causation_id: Option<EventId>,
    ) -> Result<crate::domain::EventEnvelope, TaskManagerError>
    where
        RT: EventStore,
    {
        Ok(main
            .record(
                AgentEvent::control(ControlEvent::TaskOperationRequested { operation }),
                causation_id,
            )
            .await?)
    }

    async fn accept<RT>(
        &self,
        main: &mut TaskRuntime<RT>,
        request_event_id: EventId,
        target_task_id: TaskId,
    ) -> Result<crate::domain::EventEnvelope, TaskManagerError>
    where
        RT: EventStore,
    {
        Ok(main
            .record(
                AgentEvent::control(ControlEvent::TaskOperationAccepted {
                    request_event_id,
                    target_task_id,
                }),
                Some(request_event_id),
            )
            .await?)
    }

    async fn reject<RT, T>(
        &self,
        main: &mut TaskRuntime<RT>,
        request_event_id: EventId,
        reason: impl Into<String>,
    ) -> Result<T, TaskManagerError>
    where
        RT: EventStore,
    {
        let reason = reason.into();
        main.record(
            AgentEvent::control(ControlEvent::TaskOperationRejected {
                request_event_id,
                reason: reason.clone(),
            }),
            Some(request_event_id),
        )
        .await?;
        Err(TaskManagerError::OperationRejected(reason))
    }

    fn register(&self, task_id: TaskId, cancel: CancellationToken) -> Result<(), TaskManagerError> {
        let mut tasks = self.lock_tasks()?;
        if tasks.contains_key(&task_id) {
            return Err(TaskManagerError::TaskAlreadyRunning(task_id));
        }
        tasks.insert(task_id, ManagedTask { cancel });
        Ok(())
    }

    fn unregister(&self, task_id: TaskId) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.remove(&task_id);
        }
    }

    fn lock_tasks(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<TaskId, ManagedTask>>, TaskManagerError> {
        self.tasks
            .lock()
            .map_err(|_| TaskManagerError::LockPoisoned)
    }
}

struct TaskLease<S: EventStore> {
    task_id: TaskId,
    runtime: TaskRuntime<Arc<S>>,
    cancel: CancellationToken,
    tasks: Arc<Mutex<HashMap<TaskId, ManagedTask>>>,
}

impl<S: EventStore> TaskLease<S> {
    fn new(
        task_id: TaskId,
        runtime: TaskRuntime<Arc<S>>,
        cancel: CancellationToken,
        tasks: Arc<Mutex<HashMap<TaskId, ManagedTask>>>,
    ) -> Self {
        Self {
            task_id,
            runtime,
            cancel,
            tasks,
        }
    }
}

impl<S: EventStore> Drop for TaskLease<S> {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.remove(&self.task_id);
        }
    }
}

/// 主会话唯一可取得的任务管理能力。
pub struct MainTaskLease<S: EventStore>(TaskLease<S>);

impl<S: EventStore> MainTaskLease<S> {
    #[must_use]
    pub fn runtime(&self) -> &TaskRuntime<Arc<S>> {
        &self.0.runtime
    }
    pub fn runtime_mut(&mut self) -> &mut TaskRuntime<Arc<S>> {
        &mut self.0.runtime
    }
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.0.cancel.clone()
    }
}

/// 子任务租约不暴露跨任务管理能力。
pub struct ChildTaskLease<S: EventStore>(TaskLease<S>);

impl<S: EventStore> ChildTaskLease<S> {
    #[must_use]
    pub const fn task_id(&self) -> TaskId {
        self.0.task_id
    }
    #[must_use]
    pub fn runtime(&self) -> &TaskRuntime<Arc<S>> {
        &self.0.runtime
    }
    pub fn runtime_mut(&mut self) -> &mut TaskRuntime<Arc<S>> {
        &mut self.0.runtime
    }
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.0.cancel.clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveTask {
    pub task_id: TaskId,
    pub is_main: bool,
    pub cancellation_requested: bool,
}

/// 面向主会话任务发现的持久化任务摘要。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSummary {
    pub task_id: TaskId,
    pub status: crate::domain::TaskStatus,
    pub title: Option<String>,
    pub last_sequence: u64,
    pub has_result: bool,
}

/// 面向主会话任务查询的任务详情。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDetails {
    pub summary: TaskSummary,
    pub latest_result_event_id: Option<EventId>,
    pub latest_result: Option<String>,
}

/// `TaskManager::request_create_child` 的返回值。
pub struct CreatedChild<S: EventStore> {
    pub task_id: TaskId,
    pub requested_event_id: EventId,
    pub accepted_event_id: EventId,
    pub runtime: TaskRuntime<Arc<S>>,
}

/// `TaskManager::deliver_child_result` 的返回值。
#[derive(Clone, Debug)]
pub struct DeliveredChildResult {
    pub task_id: TaskId,
    pub started_event_id: EventId,
    pub finished_event_id: EventId,
    pub result: ToolResult,
}

/// 主会话可向子任务投递的控制事件范围。
fn is_manageable_control(control: &ControlEvent) -> bool {
    matches!(
        control,
        ControlEvent::TaskPaused { .. }
            | ControlEvent::TaskResumed
            | ControlEvent::TaskCancelled { .. }
            | ControlEvent::ModelSelected { .. }
            | ControlEvent::MinimumControlPermissionChanged { .. }
    )
}

/// 取得主会话中可作为子任务输入授权父节点的直接外部输入权限。
///
/// 这里只接受核心已经登记并持久化的用户消息或告警。控制、模型、工具和系统事件
/// 即使携带较高的内部权限，也不能借此给子任务输入授权。
fn direct_external_input_permission(event: &EventEnvelope) -> Option<PermissionLevel> {
    if event
        .provenance
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now())
        || !matches!(
            event.provenance.creator,
            crate::domain::EventSource::External(_)
        )
    {
        return None;
    }
    let AgentEvent::Ingress(ingress) = &event.payload else {
        return None;
    };
    let IngressEvent::ContextReceived {
        context,
        assessment,
    } = ingress.as_ref()
    else {
        return None;
    };
    if !matches!(context.kind, ContextKind::UserMessage | ContextKind::Alert)
        || context.permission != assessment.effective_permission
        || assessment.effective_permission != event.provenance.direct_permission?
        || !assessment.effective_permission.can_authorize()
    {
        return None;
    }
    Some(assessment.effective_permission)
}

/// 从子任务事件流中提取终止结论摘要。
fn child_outcome_summary(events: &[EventEnvelope]) -> Option<(EventId, String)> {
    events.iter().rev().find_map(|event| {
        let AgentEvent::Control(control) = &event.payload else {
            return None;
        };
        let summary = match control.as_ref() {
            ControlEvent::TaskCompleted { response } => response
                .clone()
                .unwrap_or_else(|| "子任务已完成，但未返回文本摘要。".into()),
            ControlEvent::TaskFailed { reason } => format!("子任务执行失败：{reason}"),
            ControlEvent::TaskCancelled { reason } => format!("子任务已取消：{reason}"),
            ControlEvent::TaskExpired { reason } => format!("子任务已过期：{reason}"),
            _ => return None,
        };
        Some((event.id, summary))
    })
}

#[derive(Debug, Error)]
pub enum TaskManagerError {
    #[error("任务已在当前进程运行：{0}")]
    TaskAlreadyRunning(TaskId),
    #[error("任务管理操作被拒绝：{0}")]
    OperationRejected(String),
    #[error("任务管理器状态锁已中毒")]
    LockPoisoned,
    #[error(transparent)]
    Recovery(#[from] RuntimeRecoveryError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    EventStore(#[from] crate::ports::EventStoreError),
}
