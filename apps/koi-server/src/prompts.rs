use koi_core::ports::{PromptError, PromptTaskKind, SystemPrompt, SystemPromptProvider};

/// `koi-server` 随程序发布的主会话与子任务提示词。
#[derive(Default)]
pub struct ServerPromptProvider;

impl SystemPromptProvider for ServerPromptProvider {
    fn prompt_for(&self, task_kind: PromptTaskKind) -> Result<SystemPrompt, PromptError> {
        let content = match task_kind {
            PromptTaskKind::Main => include_str!("../prompts/main.md"),
            PromptTaskKind::Child => include_str!("../prompts/child.md"),
        };
        Ok(SystemPrompt {
            content: content.into(),
        })
    }
}
