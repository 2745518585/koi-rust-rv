use chrono::{DateTime, Utc};
use thiserror::Error;

use super::{
    AgentEvent, ControlEvent, EventEnvelope, EventId, PolicyDecision, TaskId, ToolEvent, Usage,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskStatus {
    New,
    Created,
    Queued,
    Running,
    WaitingApproval,
    Paused,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Expired,
}

impl TaskStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

impl UsageTotals {
    fn add(&mut self, usage: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.unwrap_or_default());
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(usage.reasoning_tokens.unwrap_or_default());
    }
}

/// 可由事件流重建的单任务状态；它是投影，而非事实来源。
#[derive(Clone, Debug)]
pub struct TaskProjection {
    pub task_id: TaskId,
    pub status: TaskStatus,
    /// 外部控制指令生效所需的最低权限，不影响工具调用权限。
    pub minimum_control_permission: super::PermissionLevel,
    /// 由任务管理操作设置的稳定显示名称；为空时由调用方按事件流推断。
    pub title: Option<String>,
    pub last_sequence: u64,
    pub last_event_id: Option<EventId>,
    pub usage: UsageTotals,
    pub updated_at: Option<DateTime<Utc>>,
}

impl TaskProjection {
    #[must_use]
    pub fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            status: TaskStatus::New,
            minimum_control_permission: super::PermissionLevel::User,
            title: None,
            last_sequence: 0,
            last_event_id: None,
            usage: UsageTotals::default(),
            updated_at: None,
        }
    }

    /// 将下一条已持久化事件应用到该投影。
    ///
    /// # Errors
    ///
    /// 当事件属于其他任务、顺序错误、试图修改终态任务或导致非法状态转换时返回错误。
    pub fn apply(&mut self, event: &EventEnvelope) -> Result<(), TaskProjectionError> {
        if event.task_id != self.task_id {
            return Err(TaskProjectionError::WrongTask {
                expected: self.task_id,
                actual: event.task_id,
            });
        }

        let expected_sequence = self.last_sequence.saturating_add(1);
        if event.sequence != expected_sequence {
            return Err(TaskProjectionError::UnexpectedSequence {
                expected: expected_sequence,
                actual: event.sequence,
            });
        }

        if self.status.is_terminal() {
            return Err(TaskProjectionError::TerminalTask(self.status));
        }

        self.apply_payload(&event.payload)?;
        self.last_sequence = event.sequence;
        self.last_event_id = Some(event.id);
        self.updated_at = Some(event.recorded_at);
        Ok(())
    }

    fn apply_payload(&mut self, payload: &AgentEvent) -> Result<(), TaskProjectionError> {
        match payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::TaskCreated { .. } => self.transition(TaskStatus::Created)?,
                ControlEvent::TaskQueued => self.transition(TaskStatus::Queued)?,
                ControlEvent::TaskPaused { .. } => self.transition(TaskStatus::Paused)?,
                ControlEvent::TaskResumed => self.transition(TaskStatus::Running)?,
                ControlEvent::TaskNamed { name } => {
                    self.title = Some(name.clone());
                }
                ControlEvent::MinimumControlPermissionChanged { minimum_permission } => {
                    self.minimum_control_permission = *minimum_permission;
                }
                ControlEvent::TaskOperationRequested { .. }
                | ControlEvent::TaskOperationAccepted { .. }
                | ControlEvent::TaskOperationRejected { .. }
                | ControlEvent::ContextCompacted { .. } => {}
                ControlEvent::TaskCompleted { .. } => self.transition(TaskStatus::Completed)?,
                ControlEvent::TaskFailed { .. } | ControlEvent::BudgetExceeded { .. } => {
                    self.transition(TaskStatus::Failed)?;
                }
                ControlEvent::TaskCancelled { .. } => self.transition(TaskStatus::Cancelled)?,
                ControlEvent::TaskExpired { .. } => self.transition(TaskStatus::Expired)?,
            },
            AgentEvent::Ingress(ingress) => {
                if matches!(
                    ingress.as_ref(),
                    super::IngressEvent::CancellationRequested { .. }
                ) {
                    self.transition(TaskStatus::Cancelling)?;
                }
            }
            AgentEvent::Model(model) => match model.as_ref() {
                super::ModelEvent::CallStarted { .. } => self.transition(TaskStatus::Running)?,
                super::ModelEvent::Completed { usage, .. } => self.usage.add(usage),
                super::ModelEvent::Delta { .. } | super::ModelEvent::Failed { .. } => {}
            },
            AgentEvent::Tool(tool) => match tool.as_ref() {
                ToolEvent::Started { .. } => self.transition(TaskStatus::Running)?,
                ToolEvent::AuthorizationChecked {
                    decision: PolicyDecision::RequireApproval,
                    ..
                }
                | ToolEvent::ApprovalRequested { .. } => {
                    self.transition(TaskStatus::WaitingApproval)?;
                }
                _ => {}
            },
        }

        Ok(())
    }

    fn transition(&mut self, next: TaskStatus) -> Result<(), TaskProjectionError> {
        if self.status == next {
            return Ok(());
        }

        let allowed = matches!(
            (self.status, next),
            (TaskStatus::New, TaskStatus::Created)
                | (
                    TaskStatus::Created,
                    TaskStatus::Queued | TaskStatus::Running | TaskStatus::Cancelled
                )
                | (
                    TaskStatus::Queued,
                    TaskStatus::Running | TaskStatus::Cancelled | TaskStatus::Expired
                )
                | (
                    TaskStatus::Running,
                    TaskStatus::WaitingApproval
                        | TaskStatus::Paused
                        | TaskStatus::Cancelling
                        | TaskStatus::Completed
                        | TaskStatus::Failed
                        | TaskStatus::Cancelled
                        | TaskStatus::Expired
                )
                | (
                    TaskStatus::WaitingApproval,
                    TaskStatus::Running
                        | TaskStatus::Paused
                        | TaskStatus::Cancelling
                        | TaskStatus::Cancelled
                        | TaskStatus::Expired
                )
                | (
                    TaskStatus::Paused,
                    TaskStatus::Running
                        | TaskStatus::Cancelling
                        | TaskStatus::Cancelled
                        | TaskStatus::Expired
                )
                | (
                    TaskStatus::Cancelling,
                    TaskStatus::Cancelled | TaskStatus::Failed | TaskStatus::Expired
                )
        );

        if !allowed {
            return Err(TaskProjectionError::InvalidTransition {
                from: self.status,
                to: next,
            });
        }

        self.status = next;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum TaskProjectionError {
    #[error("event belongs to task {actual}, expected {expected}")]
    WrongTask { expected: TaskId, actual: TaskId },
    #[error("event sequence is {actual}, expected {expected}")]
    UnexpectedSequence { expected: u64, actual: u64 },
    #[error("task is already terminal: {0:?}")]
    TerminalTask(TaskStatus),
    #[error("invalid task transition from {from:?} to {to:?}")]
    InvalidTransition { from: TaskStatus, to: TaskStatus },
}
