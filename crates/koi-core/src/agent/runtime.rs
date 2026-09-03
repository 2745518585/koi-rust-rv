use chrono::Utc;
use thiserror::Error;

use crate::domain::{
    AgentEvent, EventEnvelope, EventId, EventProvenance, TaskId, TaskProjection,
    TaskProjectionError,
};
use crate::ports::{EventStore, EventStoreError};

/// 创建带顺序号的任务事件，并仅在持久化成功后更新状态投影。
#[derive(Debug)]
pub struct TaskRuntime<S> {
    store: S,
    projection: TaskProjection,
}

impl<S> TaskRuntime<S>
where
    S: EventStore,
{
    #[must_use]
    pub fn new(store: S, task_id: TaskId) -> Self {
        Self {
            store,
            projection: TaskProjection::new(task_id),
        }
    }

    /// 从已持久化事件流恢复任务运行时。
    ///
    /// # Errors
    ///
    /// 当事件读取失败，或事件流无法按顺序重建任务投影时返回错误。
    pub async fn recover(store: S, task_id: TaskId) -> Result<Self, RuntimeRecoveryError> {
        let events = store.load_task(task_id).await?;
        if events.is_empty() {
            return Err(RuntimeRecoveryError::EmptyEventStream(task_id));
        }
        let mut projection = TaskProjection::new(task_id);
        for event in &events {
            projection.apply(event)?;
        }
        Ok(Self { store, projection })
    }

    #[must_use]
    pub fn projection(&self) -> &TaskProjection {
        &self.projection
    }

    /// 读取当前任务的完整事件流。
    ///
    /// Agent 恢复、审批绑定和调度器重启恢复都必须以持久化事件为准，不能只依赖运行时
    /// 内存中的投影。
    ///
    /// # Errors
    ///
    /// 当底层事件存储读取失败时返回错误。
    pub async fn load_events(&self) -> Result<Vec<EventEnvelope>, RuntimeError> {
        Ok(self.store.load_task(self.projection.task_id).await?)
    }

    /// 取得底层事件存储的所有权，常用于完成任务后重建或迁移运行时。
    #[must_use]
    pub fn into_store(self) -> S {
        self.store
    }

    /// 先持久化事件，再将其合并到内存中的状态投影。
    ///
    /// # Errors
    ///
    /// 当持久化失败或事件会违反任务状态投影规则时返回错误。
    pub async fn record(
        &mut self,
        payload: AgentEvent,
        causation_id: Option<EventId>,
    ) -> Result<EventEnvelope, RuntimeError> {
        self.record_with_provenance(payload, causation_id, EventProvenance::system())
            .await
    }

    /// 记录具有显式创建来源与权限继承关系的事件。
    ///
    /// # Errors
    ///
    /// 当持久化失败或事件会违反任务状态投影规则时返回错误。
    pub async fn record_with_provenance(
        &mut self,
        payload: AgentEvent,
        causation_id: Option<EventId>,
        provenance: EventProvenance,
    ) -> Result<EventEnvelope, RuntimeError> {
        if payload_is_child_task_management_operation(&payload, self.projection.task_id) {
            return Err(RuntimeError::ChildTaskManagementForbidden(
                self.projection.task_id,
            ));
        }
        let mut event = EventEnvelope {
            id: crate::domain::EventId::new(),
            task_id: self.projection.task_id,
            sequence: self.projection.last_sequence.saturating_add(1),
            occurred_at: Utc::now(),
            recorded_at: Utc::now(),
            causation_id,
            provenance,
            payload,
        };

        match self.store.append(&event).await {
            Ok(()) => self.projection.apply(&event)?,
            Err(error) if is_sequence_conflict(&error) => {
                // 外部来源可能在 Agent 思考期间追加了输入。用持久化事件流刷新投影后重试
                // 一次，避免长时间模型调用与来源写入之间产生不可恢复的序号竞争。
                self.refresh_projection().await?;
                event = EventEnvelope {
                    id: crate::domain::EventId::new(),
                    task_id: self.projection.task_id,
                    sequence: self.projection.last_sequence.saturating_add(1),
                    occurred_at: Utc::now(),
                    recorded_at: Utc::now(),
                    causation_id: event.causation_id,
                    provenance: event.provenance.clone(),
                    payload: event.payload.clone(),
                };
                self.store.append(&event).await?;
                self.projection.apply(&event)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(event)
    }

    async fn refresh_projection(&mut self) -> Result<(), RuntimeError> {
        let events = self.store.load_task(self.projection.task_id).await?;
        let mut projection = TaskProjection::new(self.projection.task_id);
        for event in &events {
            projection.apply(event)?;
        }
        self.projection = projection;
        Ok(())
    }
}

fn is_sequence_conflict(error: &EventStoreError) -> bool {
    error.message.contains("序号") || error.message.contains("sequence")
}

fn payload_is_child_task_management_operation(payload: &AgentEvent, task_id: TaskId) -> bool {
    !task_id.is_main()
        && matches!(
            payload,
            AgentEvent::Control(control) if control.is_task_management_operation()
        )
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("子任务不能创建跨任务管理控制事件：{0}")]
    ChildTaskManagementForbidden(TaskId),
    #[error(transparent)]
    EventStore(#[from] EventStoreError),
    #[error(transparent)]
    Projection(#[from] TaskProjectionError),
}

#[derive(Debug, Error)]
pub enum RuntimeRecoveryError {
    #[error("任务没有可恢复的事件流：{0}")]
    EmptyEventStream(TaskId),
    #[error(transparent)]
    EventStore(#[from] EventStoreError),
    #[error(transparent)]
    Projection(#[from] TaskProjectionError),
}
