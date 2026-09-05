use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::{EventEnvelope, EventId, TaskId};

#[derive(Debug, Error)]
#[error("event store error: {message}")]
pub struct EventStoreError {
    pub message: String,
}

impl EventStoreError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// 任务事件流的仅追加持久化与读取边界。
#[async_trait]
pub trait EventStore: Send + Sync {
    /// 在所属任务的事件流中持久化一条事件。
    ///
    /// # Errors
    ///
    /// 当底层存储无法持久追加事件时返回错误。
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError>;

    /// 按任务内序号升序读取完整事件流。
    ///
    /// 默认实现用于兼容仅支持追加的旧适配器；可恢复运行时必须使用实现了读取能力的存储。
    ///
    /// # Errors
    ///
    /// 当存储不支持读取或读取失败时返回错误。
    async fn load_task(&self, _task_id: TaskId) -> Result<Vec<EventEnvelope>, EventStoreError> {
        Err(EventStoreError::new("事件存储不支持任务事件读取"))
    }

    /// 列出当前持久化的任务事件流。
    ///
    /// 任务管理器需要通过这个接口发现重启前创建的任务，而不能只依赖当前进程中
    /// 的活动租约。仅支持追加、但没有任务枚举能力的旧适配器会显式返回错误。
    ///
    /// # Errors
    ///
    /// 当存储不支持任务枚举或枚举失败时返回错误。
    async fn list_task_ids(&self) -> Result<Vec<TaskId>, EventStoreError> {
        Err(EventStoreError::new("事件存储不支持任务枚举"))
    }

    /// 读取任务内的单条事件。
    ///
    /// # Errors
    ///
    /// 当存储不支持读取或读取失败时返回错误。
    async fn load_event(
        &self,
        task_id: TaskId,
        event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        Ok(self
            .load_task(task_id)
            .await?
            .into_iter()
            .find(|event| event.id == event_id))
    }

    /// 按全局事件 ID读取事件。
    ///
    /// 权限父事件通常位于当前任务内，但主会话委托给子任务的输入可以跨任务引用主
    /// 会话事件。支持跨任务授权溯源的存储应实现此方法；不支持的适配器会显式失败，
    /// 核心随后拒绝该授权链。
    ///
    /// # Errors
    ///
    /// 当存储不支持全局事件读取或读取失败时返回错误。
    async fn load_event_any(
        &self,
        _event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        Err(EventStoreError::new("事件存储不支持按全局 ID读取事件"))
    }

    /// 删除一个任务的全部事件。
    ///
    /// 只有任务管理流程在确认目标已终止后才允许调用。默认实现拒绝删除，保证只支持
    /// 追加的存储适配器不会静默丢数据。
    ///
    /// # Errors
    ///
    /// 当存储不支持删除或删除失败时返回错误。
    async fn delete_task(&self, _task_id: TaskId) -> Result<(), EventStoreError> {
        Err(EventStoreError::new("事件存储不支持删除任务"))
    }
}

/// 允许运行时和任务管理器共享同一个事件存储实例。
#[async_trait]
impl<T> EventStore for Arc<T>
where
    T: EventStore + ?Sized,
{
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError> {
        self.as_ref().append(event).await
    }

    async fn load_task(&self, task_id: TaskId) -> Result<Vec<EventEnvelope>, EventStoreError> {
        self.as_ref().load_task(task_id).await
    }

    async fn list_task_ids(&self) -> Result<Vec<TaskId>, EventStoreError> {
        self.as_ref().list_task_ids().await
    }

    async fn load_event(
        &self,
        task_id: TaskId,
        event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        self.as_ref().load_event(task_id, event_id).await
    }

    async fn load_event_any(
        &self,
        event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        self.as_ref().load_event_any(event_id).await
    }

    async fn delete_task(&self, task_id: TaskId) -> Result<(), EventStoreError> {
        self.as_ref().delete_task(task_id).await
    }
}

/// 用于本地开发和测试的进程内事件存储。
///
/// 它保留事件完整负载、拒绝重复事件和错误序号，但进程退出后数据会丢失。
#[derive(Default)]
pub struct InMemoryEventStore {
    events: Mutex<HashMap<TaskId, Vec<EventEnvelope>>>,
}

impl InMemoryEventStore {
    /// # Errors
    ///
    /// 当互斥锁已中毒时返回错误。
    pub fn event_count(&self, task_id: TaskId) -> Result<usize, EventStoreError> {
        let events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        Ok(events.get(&task_id).map_or(0, Vec::len))
    }

    /// 列出当前持有事件流的任务，主要用于测试与诊断。
    #[must_use]
    pub fn known_tasks(&self) -> Vec<TaskId> {
        self.events
            .lock()
            .map(|events| {
                let mut task_ids = events.keys().copied().collect::<Vec<_>>();
                task_ids.sort_by_cached_key(ToString::to_string);
                task_ids
            })
            .unwrap_or_default()
    }
}

#[async_trait]
impl EventStore for InMemoryEventStore {
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        let task_events = events.entry(event.task_id).or_default();
        let expected_sequence = task_events
            .last()
            .map_or(1, |previous| previous.sequence.saturating_add(1));
        if event.sequence != expected_sequence {
            return Err(EventStoreError::new(format!(
                "任务 {} 的事件序号为 {}，期望为 {}",
                event.task_id, event.sequence, expected_sequence
            )));
        }
        if task_events.iter().any(|previous| previous.id == event.id) {
            return Err(EventStoreError::new(format!("事件已存在：{}", event.id)));
        }
        task_events.push(event.clone());
        Ok(())
    }

    async fn load_task(&self, task_id: TaskId) -> Result<Vec<EventEnvelope>, EventStoreError> {
        let events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        Ok(events.get(&task_id).cloned().unwrap_or_default())
    }

    async fn list_task_ids(&self) -> Result<Vec<TaskId>, EventStoreError> {
        let events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        let mut task_ids = events.keys().copied().collect::<Vec<_>>();
        task_ids.sort_by_cached_key(ToString::to_string);
        Ok(task_ids)
    }

    async fn load_event_any(
        &self,
        event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        let events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        Ok(events
            .values()
            .flat_map(|task_events| task_events.iter())
            .find(|event| event.id == event_id)
            .cloned())
    }

    async fn delete_task(&self, task_id: TaskId) -> Result<(), EventStoreError> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| EventStoreError::new("内存事件存储锁已中毒"))?;
        events.remove(&task_id);
        Ok(())
    }
}
