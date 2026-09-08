//! In-memory model reasoning stream for logs and local operators.
//!
//! Reasoning summaries are deliberately kept outside the event store. A receiver only sees
//! summaries produced after it subscribes; a restart or disconnect never replays them.

use std::sync::Arc;

use koi_core::domain::{EventId, TaskId};
use koi_core::ports::ModelReasoningSink;
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 1_024;

#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Debug)]
pub(crate) struct ReasoningSummary {
    pub(crate) task_id: TaskId,
    pub(crate) call_started_event_id: EventId,
    pub(crate) sequence: u32,
    pub(crate) content: String,
}

pub(crate) struct ModelTrace {
    sender: broadcast::Sender<ReasoningSummary>,
}

impl ModelTrace {
    pub(crate) fn new() -> Arc<Self> {
        let (sender, _) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self { sender })
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<ReasoningSummary> {
        self.sender.subscribe()
    }
}

impl ModelReasoningSink for ModelTrace {
    fn publish_reasoning_summary(
        &self,
        task_id: TaskId,
        call_started_event_id: EventId,
        sequence: u32,
        content: &str,
    ) {
        let _ = self.sender.send(ReasoningSummary {
            task_id,
            call_started_event_id,
            sequence,
            content: content.to_owned(),
        });
    }
}
