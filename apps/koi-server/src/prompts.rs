use koi_core::ports::{PromptError, PromptTaskKind, SystemPrompt, SystemPromptProvider};

/// `koi-server` 随程序发布的主会话、QQ 来源片段与子任务提示词。
#[derive(Default)]
pub struct ServerPromptProvider;

const MAIN_PROMPT: &str = concat!(
    include_str!("../prompts/main.md"),
    "\n\n",
    include_str!("../prompts/qq.md"),
);

impl SystemPromptProvider for ServerPromptProvider {
    fn prompt_for(&self, task_kind: PromptTaskKind) -> Result<SystemPrompt, PromptError> {
        let content = match task_kind {
            PromptTaskKind::Main => MAIN_PROMPT,
            PromptTaskKind::Child => include_str!("../prompts/child.md"),
        };
        Ok(SystemPrompt {
            content: content.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_prompt_includes_qq_guidance() {
        let prompt = ServerPromptProvider
            .prompt_for(PromptTaskKind::Main)
            .expect("main prompt");
        assert!(prompt.content.contains("## QQ source context"));
        assert!(prompt.content.contains("明确@bot=否"));
        assert!(prompt.content.contains("qq.group_send"));
        assert!(prompt.content.contains("qq.reply"));
        assert!(prompt.content.contains("qq.report"));
        assert!(prompt.content.contains("not automatically sent back to QQ"));
        assert!(
            prompt
                .content
                .contains("Only a runtime-provided delivery or notification tool")
        );
        assert!(
            prompt
                .content
                .contains("failed operation, blocked operation")
        );
        assert!(prompt.content.contains("For a Web conversation"));
        assert!(prompt.content.contains("call `qq.reply`"));
        assert!(prompt.content.contains("For a child task"));
        assert!(prompt.content.contains("## Completion protocol"));
        assert!(prompt.content.contains("progress-only text"));
        assert!(
            prompt
                .content
                .contains("## Permission denial and user escalation")
        );
        assert!(
            prompt
                .content
                .contains("A permission denial is a final core decision")
        );
        assert!(
            prompt
                .content
                .contains("prefer asking the user or reporting the block")
        );
        assert!(prompt.content.contains("After a delivery succeeds"));
        assert!(
            prompt
                .content
                .contains("all currently visible input events")
        );
        assert!(
            prompt
                .content
                .contains("An alert input can therefore authorize a delivery")
        );
        assert!(prompt.content.contains("never set the"));
        assert!(prompt.content.contains("authority parent to `null`"));
    }

    #[test]
    fn child_prompt_remains_without_main_session_qq_routing_rules() {
        let prompt = ServerPromptProvider
            .prompt_for(PromptTaskKind::Child)
            .expect("child prompt");
        assert!(!prompt.content.contains("## QQ source context"));
        assert!(prompt.content.contains("every visible input event"));
        assert!(prompt.content.contains("An alert can authorize"));
        assert!(prompt.content.contains("## Completion protocol"));
        assert!(
            prompt
                .content
                .contains("## Permission denial and user escalation")
        );
        assert!(prompt.content.contains("Do not call the same tool again"));
    }
}
