use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{RuntimeError, RuntimeRecoveryError, TaskRuntime};
use crate::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, ControlEvent, EventId,
    EventProvenance, EventSource, IngressEvent, PermissionAssessment, PermissionLevel, Scope,
    TaskId, TaskOperation,
};
use crate::ports::EventStore;

/// 进程内任务管理器。跨任务写操作仅接受 `MainTaskLease`。
pub struct TaskManager<S>
where
    S: EventStore + ?Sized,
{
    store: Arc<S>,
    tasks: Arc<Mutex<HashMap<TaskId, ManagedTask>>>,
}

struct ManagedTask {
    cancel: CancellationToken,
}

impl<S> TaskManager<S>
where
    S: EventStore + ?Sized,
{
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
            .record_request(main, TaskOperation::CreateChild, causation_id)
            .await?;
        let task_id = TaskId::new();
        let cancel = CancellationToken::new();
        self.register(task_id, cancel.clone())?;
        if let Err(error) = self.accept(main, request.id, task_id).await {
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
            .record_request(main, TaskOperation::ResumeChild { task_id }, causation_id)
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能恢复主会话")
                .await;
        }
        let runtime = match TaskRuntime::recover(Arc::clone(&self.store), task_id).await {
            Ok(runtime) if !runtime.projection().status.is_terminal() => runtime,
            Ok(runtime) => {
                return self
                    .reject(
                        main,
                        request.id,
                        format!("目标子任务已终止：{:?}", runtime.projection().status),
                    )
                    .await;
            }
            Err(error) => return self.reject(main, request.id, error.to_string()).await,
        };
        let cancel = CancellationToken::new();
        if let Err(error) = self.register(task_id, cancel.clone()) {
            return self.reject(main, request.id, error.to_string()).await;
        }
        if let Err(error) = self.accept(main, request.id, task_id).await {
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
                main,
                TaskOperation::CancelChild {
                    task_id,
                    reason: reason.into(),
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "任务管理接口不能控制主会话")
                .await;
        }
        let cancel = {
            let tasks = self.lock_tasks()?;
            tasks.get(&task_id).map(|task| task.cancel.clone())
        };
        let Some(cancel) = cancel else {
            return self
                .reject(main, request.id, "目标子任务未在当前进程运行")
                .await;
        };
        cancel.cancel();
        self.accept(main, request.id, task_id).await
    }

    /// 验证子任务完成事件，并将无权限结果回传为主会话可注入的工具上下文。
    ///
    /// # Errors
    ///
    /// 当目标是主会话、完成事件不存在/类型不符或主会话事件无法写入时返回错误。
    pub async fn forward_child_result(
        &self,
        main: &mut MainTaskLease<S>,
        task_id: TaskId,
        completed_event_id: EventId,
        causation_id: Option<EventId>,
    ) -> Result<EventId, TaskManagerError> {
        let request = self
            .record_request(
                main,
                TaskOperation::DeliverChildResult {
                    task_id,
                    completed_event_id,
                },
                causation_id,
            )
            .await?;
        if task_id.is_main() {
            return self
                .reject(main, request.id, "主会话不能作为子任务结果来源")
                .await;
        }
        let completed = self.store.load_event(task_id, completed_event_id).await?;
        let Some(completed) = completed else {
            return self.reject(main, request.id, "子任务完成事件不存在").await;
        };
        let AgentEvent::Control(control) = completed.payload else {
            return self
                .reject(main, request.id, "子任务结果必须引用完成控制事件")
                .await;
        };
        let ControlEvent::TaskCompleted { response } = *control else {
            return self
                .reject(main, request.id, "引用事件不是子任务完成事件")
                .await;
        };
        let now = chrono::Utc::now();
        let summary = response.unwrap_or_else(|| "子任务已完成，但未返回文本摘要。".into());
        let assessment = PermissionAssessment::new(
            PermissionLevel::None,
            PermissionLevel::None,
            PermissionLevel::None,
        );
        let result = main
            .0
            .runtime
            .record_with_provenance(
                AgentEvent::ingress(IngressEvent::ContextReceived {
                    context: Box::new(ContextEnvelope {
                        schema_version: 1,
                        kind: ContextKind::ToolResult,
                        origin: ContextOrigin {
                            source: "internal-task".into(),
                            source_instance: task_id.to_string(),
                            native_event_id: completed_event_id.to_string(),
                        },
                        actor: None,
                        scope: Scope::new("task", TaskId::MAIN.to_string()),
                        occurred_at: now,
                        received_at: now,
                        position: None,
                        permission: PermissionLevel::None,
                        payload: ContextPayload::Text {
                            text: format!("子任务 {task_id} 的结果：{summary}"),
                            mentions: Vec::new(),
                        },
                        causation_id: Some(completed_event_id),
                        content_hash: format!("child-result:{completed_event_id}"),
                    }),
                    assessment,
                }),
                Some(request.id),
                EventProvenance {
                    creator: EventSource::System,
                    direct_permission: Some(PermissionLevel::None),
                    authority_parent_event_id: None,
                    expires_at: None,
                },
            )
            .await?;
        self.accept(main, request.id, task_id).await?;
        Ok(result.id)
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

    async fn record_request(
        &self,
        main: &mut MainTaskLease<S>,
        operation: TaskOperation,
        causation_id: Option<EventId>,
    ) -> Result<crate::domain::EventEnvelope, TaskManagerError> {
        Ok(main
            .0
            .runtime
            .record(
                AgentEvent::control(ControlEvent::TaskOperationRequested { operation }),
                causation_id,
            )
            .await?)
    }

    async fn accept(
        &self,
        main: &mut MainTaskLease<S>,
        request_event_id: EventId,
        target_task_id: TaskId,
    ) -> Result<(), TaskManagerError> {
        main.0
            .runtime
            .record(
                AgentEvent::control(ControlEvent::TaskOperationAccepted {
                    request_event_id,
                    target_task_id,
                }),
                Some(request_event_id),
            )
            .await?;
        Ok(())
    }

    async fn reject<T>(
        &self,
        main: &mut MainTaskLease<S>,
        request_event_id: EventId,
        reason: impl Into<String>,
    ) -> Result<T, TaskManagerError> {
        let reason = reason.into();
        main.0
            .runtime
            .record(
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

struct TaskLease<S>
where
    S: EventStore + ?Sized,
{
    task_id: TaskId,
    runtime: TaskRuntime<Arc<S>>,
    cancel: CancellationToken,
    tasks: Arc<Mutex<HashMap<TaskId, ManagedTask>>>,
}

impl<S> TaskLease<S>
where
    S: EventStore + ?Sized,
{
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

impl<S> Drop for TaskLease<S>
where
    S: EventStore + ?Sized,
{
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.remove(&self.task_id);
        }
    }
}

/// 主会话唯一可取得的任务管理能力。
pub struct MainTaskLease<S>(TaskLease<S>)
where
    S: EventStore + ?Sized;

impl<S> MainTaskLease<S>
where
    S: EventStore + ?Sized,
{
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
pub struct ChildTaskLease<S>(TaskLease<S>)
where
    S: EventStore + ?Sized;

impl<S> ChildTaskLease<S>
where
    S: EventStore + ?Sized,
{
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
