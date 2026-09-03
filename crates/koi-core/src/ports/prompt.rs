use thiserror::Error;

use crate::domain::TaskId;

/// 系统提示词所服务的任务类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptTaskKind {
    Main,
    Child,
}

impl PromptTaskKind {
    #[must_use]
    pub const fn for_task(task_id: TaskId) -> Self {
        if task_id.is_main() {
            Self::Main
        } else {
            Self::Child
        }
    }
}

/// 由应用层维护的系统提示词文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemPrompt {
    pub content: String,
}

impl SystemPrompt {
    /// # Errors
    ///
    /// 当提示词为空时返回错误。
    pub fn validate(&self) -> Result<(), PromptError> {
        if self.content.trim().is_empty() {
            return Err(PromptError::new("系统提示词不能为空"));
        }
        Ok(())
    }
}

/// 为不同任务类别提供系统提示词的应用层接口。
pub trait SystemPromptProvider: Send + Sync {
    /// # Errors
    ///
    /// 当模板缺失、加载失败或内容非法时返回错误。
    fn prompt_for(&self, task_kind: PromptTaskKind) -> Result<SystemPrompt, PromptError>;
}

#[derive(Debug, Error)]
#[error("系统提示词加载失败：{message}")]
pub struct PromptError {
    pub message: String,
}

impl PromptError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
