use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use koi_core::domain::{EventEnvelope, EventId, TaskId};
use koi_core::ports::{EventStore, EventStoreError};

/// 每个任务一个 JSON Lines 文件的本地事件存储。
///
/// 文件内按任务序号追加，适合单进程开发部署。未来可替换为 SQLite/PostgreSQL 适配器，
/// 不影响核心层的 `EventStore` 接口。
pub struct JsonlEventStore {
    directory: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlEventStore {
    /// 打开或创建指定目录下的事件存储。
    ///
    /// # Errors
    ///
    /// 当目录无法创建时返回错误。
    pub fn open(directory: impl Into<PathBuf>) -> Result<Self, EventStoreError> {
        let directory = directory.into();
        fs::create_dir_all(&directory).map_err(|error| io_error(&error))?;
        Ok(Self {
            directory,
            write_lock: Mutex::new(()),
        })
    }

    /// Returns every task stream currently persisted in this local store.
    ///
    /// The list is derived from filenames and is intended for the single-process API adapter.
    /// Reading events themselves remains the source of truth and validates continuity.
    ///
    /// # Errors
    ///
    /// Returns an error if the storage directory cannot be enumerated.
    pub fn list_task_ids(&self) -> Result<Vec<TaskId>, EventStoreError> {
        let entries = fs::read_dir(&self.directory).map_err(|error| io_error(&error))?;
        let mut task_ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error(&error))?;
            let path = entry.path();
            if path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(uuid) = uuid::Uuid::parse_str(stem) else {
                continue;
            };
            task_ids.push(TaskId(uuid));
        }
        task_ids.sort_by_key(ToString::to_string);
        Ok(task_ids)
    }

    fn task_path(&self, task_id: TaskId) -> PathBuf {
        self.directory.join(format!("{task_id}.jsonl"))
    }

    fn read_task_file(path: &Path) -> Result<Vec<EventEnvelope>, EventStoreError> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(path).map_err(|error| io_error(&error))?;
        let mut events = Vec::new();
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|error| io_error(&error))?;
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str(&line).map_err(|error| {
                EventStoreError::new(format!(
                    "事件文件第 {} 行无法解析：{error}",
                    line_number + 1
                ))
            })?;
            events.push(event);
        }
        Ok(events)
    }
}

#[async_trait]
impl EventStore for JsonlEventStore {
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| EventStoreError::new("事件存储写锁已中毒"))?;
        let path = self.task_path(event.task_id);
        let existing = Self::read_task_file(&path)?;
        let expected_sequence = existing
            .last()
            .map_or(1, |previous| previous.sequence.saturating_add(1));
        if event.sequence != expected_sequence {
            return Err(EventStoreError::new(format!(
                "任务 {} 的事件序号为 {}，期望为 {}",
                event.task_id, event.sequence, expected_sequence
            )));
        }
        if existing.iter().any(|previous| previous.id == event.id) {
            return Err(EventStoreError::new(format!("事件已存在：{}", event.id)));
        }

        let serialized = serde_json::to_string(event)
            .map_err(|error| EventStoreError::new(format!("事件序列化失败：{error}")))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| io_error(&error))?;
        writeln!(file, "{serialized}").map_err(|error| io_error(&error))?;
        file.sync_data().map_err(|error| io_error(&error))
    }

    async fn load_task(&self, task_id: TaskId) -> Result<Vec<EventEnvelope>, EventStoreError> {
        let events = Self::read_task_file(&self.task_path(task_id))?;
        validate_task_events(task_id, &events)?;
        Ok(events)
    }

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
}

fn validate_task_events(task_id: TaskId, events: &[EventEnvelope]) -> Result<(), EventStoreError> {
    for (index, event) in events.iter().enumerate() {
        let expected_sequence = u64::try_from(index)
            .map_err(|_| EventStoreError::new("事件数量超出序号范围"))?
            .saturating_add(1);
        if event.task_id != task_id || event.sequence != expected_sequence {
            return Err(EventStoreError::new(format!(
                "任务 {task_id} 的事件流不连续或包含其他任务事件"
            )));
        }
    }
    Ok(())
}

fn io_error(error: &std::io::Error) -> EventStoreError {
    EventStoreError::new(format!("事件文件读写失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use koi_core::domain::{AgentEvent, ControlEvent};

    #[tokio::test]
    async fn jsonl_store_persists_and_reads_a_task_stream() {
        let directory = std::env::temp_dir().join(format!("koi-event-store-{}", EventId::new()));
        let store = JsonlEventStore::open(&directory).unwrap();
        let task_id = TaskId::new();
        let created = EventEnvelope::new(
            task_id,
            1,
            None,
            AgentEvent::control(ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
        );
        let queued = EventEnvelope::new(
            task_id,
            2,
            Some(created.id),
            AgentEvent::control(ControlEvent::TaskQueued),
        );

        store.append(&created).await.unwrap();
        store.append(&queued).await.unwrap();

        let events = store.load_task(task_id).await.unwrap();
        assert_eq!(events, vec![created.clone(), queued]);
        assert_eq!(
            store.load_event(task_id, created.id).await.unwrap(),
            Some(created)
        );

        fs::remove_dir_all(directory).unwrap();
    }
}
