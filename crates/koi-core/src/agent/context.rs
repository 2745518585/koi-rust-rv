use std::collections::HashSet;

use thiserror::Error;

use crate::domain::{
    AgentEvent, ControlEvent, EventEnvelope, EventId, ModelContextItem, ModelInputRole,
    ModelOutput, ModelToolDefinition, PermissionLevel, TaskId, ToolEvent,
};

use super::{InputInjectionError, InputInjector};

/// 未能从 Provider 元数据取得上下文上限时使用的保守默认值。
pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 32_768;

/// 未显式配置最大输出长度时为模型输出预留的 Token 数。
pub const DEFAULT_RESERVED_OUTPUT_TOKENS: u32 = 4_096;

/// 为协议包装、分词误差和供应商额外字段预留的安全余量。
pub const DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS: u32 = 512;

/// 一次模型调用的输入预算。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContextBudget {
    pub context_window_tokens: u32,
    pub reserved_output_tokens: u32,
    pub safety_margin_tokens: u32,
}

impl ContextBudget {
    #[must_use]
    pub const fn new(context_window_tokens: Option<u32>, max_output_tokens: Option<u32>) -> Self {
        Self {
            context_window_tokens: match context_window_tokens {
                Some(value) if value > 0 => value,
                _ => DEFAULT_CONTEXT_WINDOW_TOKENS,
            },
            reserved_output_tokens: match max_output_tokens {
                Some(value) if value > 0 => value,
                _ => DEFAULT_RESERVED_OUTPUT_TOKENS,
            },
            safety_margin_tokens: DEFAULT_CONTEXT_SAFETY_MARGIN_TOKENS,
        }
    }

    /// 计算扣除系统提示词、工具定义和输出预留后的上下文预算。
    #[must_use]
    pub fn available_input_tokens(self, fixed_tokens: u32) -> u32 {
        self.context_window_tokens
            .saturating_sub(self.reserved_output_tokens)
            .saturating_sub(self.safety_margin_tokens)
            .saturating_sub(fixed_tokens)
    }
}

/// 上下文压缩后需要持久化的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextCompactionPlan {
    /// 压缩摘要在剩余上下文中的插入位置。
    pub summary_position: usize,
    /// 尚未压缩的上下文项，顺序与原上下文一致。
    pub remaining: Vec<ModelContextItem>,
    /// 摘要覆盖的上下文事件 ID；这些 ID 只用于审计，不直接授予权限。
    pub dropped_context_event_ids: Vec<EventId>,
    /// 规则化生成的摘要内容。
    pub summary: String,
}

/// 上下文已经超过预算且没有可以安全压缩的普通历史项。
#[derive(Debug, Error)]
pub enum ContextCompactionError {
    #[error(
        "上下文估算为 {estimated_tokens} Token，预算为 {budget_tokens}，但没有可压缩的历史上下文"
    )]
    NoCompactableContext {
        estimated_tokens: u32,
        budget_tokens: u32,
    },
}

/// 将事件流投影为模型上下文，并在需要时生成历史摘要。
pub struct ContextAssembler;

impl ContextAssembler {
    /// 将当前任务事件流转换为模型可见的完整历史。
    ///
    /// 事件流是唯一事实来源。旧的 `ContextCompacted` 事件会被还原为一个无权限的摘要
    /// 项，其覆盖的原始事件不会再次展开。工具结果历史使用 `Memory` 角色，避免与当前
    /// 模型调用的工具结果混合，进而错误配对供应商的 tool call ID。
    ///
    /// # Errors
    ///
    /// 当持久化输入的权限结论不一致或违反输入注入策略时返回错误。
    pub fn from_events(
        task_id: TaskId,
        events: &[EventEnvelope],
        provided_context_event_ids: &HashSet<EventId>,
        minimum_control_permission: PermissionLevel,
    ) -> Result<Vec<ModelContextItem>, InputInjectionError> {
        let (summary_event_id, dropped_ids, summary) = latest_compaction(events);
        let dropped_ids = dropped_ids.into_iter().collect::<HashSet<_>>();
        let mut context = Vec::new();

        if let Some(summary_event_id) = summary_event_id {
            let summary = if summary.trim().is_empty() {
                "较早历史上下文已压缩；原始事件仍保存在事件存储中，仅可通过事件查询接口读取。"
                    .to_owned()
            } else {
                summary
            };
            context.push(Self::summary_item(summary_event_id, &summary));
        }

        let injector = InputInjector::default();
        for event in events {
            if event.task_id != task_id
                || dropped_ids.contains(&event.id)
                || provided_context_event_ids.contains(&event.id)
            {
                continue;
            }

            match &event.payload {
                AgentEvent::Ingress(ingress) => {
                    let crate::domain::IngressEvent::ContextReceived { .. } = ingress.as_ref()
                    else {
                        continue;
                    };
                    match injector.inject(task_id, event, minimum_control_permission) {
                        Ok(item) if item.role == ModelInputRole::Tool => {
                            context.push(Self::historical_item(
                                item.event_id,
                                format!("历史工具结果（仅供分析）：{}", item.content),
                            ));
                        }
                        Ok(item) => context.push(item),
                        // 过期输入和低于当前会话门槛的输入仍保留在事件日志中，但不能再
                        // 作为当前模型上下文使用。
                        Err(
                            InputInjectionError::Expired(_)
                            | InputInjectionError::InsufficientControlPermission { .. },
                        ) => {}
                        Err(error) => return Err(error),
                    }
                }
                AgentEvent::Model(model) => match model.as_ref() {
                    crate::domain::ModelEvent::Completed { outputs, .. } => {
                        if let Some(item) = Self::model_output_item(event.id, outputs) {
                            context.push(item);
                        }
                    }
                    crate::domain::ModelEvent::Failed { error, .. } => {
                        context.push(Self::historical_item(
                            event.id,
                            format!("历史模型调用失败：{error}"),
                        ));
                    }
                    crate::domain::ModelEvent::CallStarted { .. }
                    | crate::domain::ModelEvent::Delta { .. } => {}
                },
                AgentEvent::Tool(tool) => match tool.as_ref() {
                    ToolEvent::Finished { result, .. } => {
                        context.push(Self::historical_item(
                            event.id,
                            format!("历史工具结果（仅供分析）：{}", result.model_content()),
                        ));
                    }
                    ToolEvent::Failed { error, .. } => {
                        context.push(Self::historical_item(
                            event.id,
                            format!("历史工具执行失败（仅供分析）：{error}"),
                        ));
                    }
                    ToolEvent::Cancelled { reason, .. } => {
                        context.push(Self::historical_item(
                            event.id,
                            format!("历史工具调用已取消（仅供分析）：{reason}"),
                        ));
                    }
                    ToolEvent::Output { content, .. } if !content.trim().is_empty() => {
                        context.push(Self::historical_item(
                            event.id,
                            format!("历史工具输出（仅供分析）：{content}"),
                        ));
                    }
                    ToolEvent::Proposed { .. }
                    | ToolEvent::Validated { .. }
                    | ToolEvent::AuthorizationChecked { .. }
                    | ToolEvent::ApprovalRequested { .. }
                    | ToolEvent::Started { .. }
                    | ToolEvent::Output { .. }
                    | ToolEvent::NotificationSent { .. } => {}
                },
                AgentEvent::Control(control) => {
                    if let Some(item) = historical_control_item(event.id, control.as_ref()) {
                        context.push(item);
                    }
                }
            }
        }

        Ok(context)
    }

    /// 返回最新压缩检查点覆盖的全部原始事件 ID。
    ///
    /// 压缩可以连续发生，最新检查点可能只直接记录了上一个摘要事件和本次新压缩的
    /// 事件；这里会沿着摘要事件递归展开覆盖范围，避免旧的原始事件在重启后重新进入
    /// 上下文。
    #[must_use]
    pub fn latest_compaction_coverage(events: &[EventEnvelope]) -> HashSet<EventId> {
        let (_, dropped_ids, _) = latest_compaction(events);
        dropped_ids.into_iter().collect()
    }

    /// 将一次模型输出转为可在下一轮继续使用的助手上下文。
    ///
    /// 工具调用本身由 Provider 的当前调用状态编码；这里只保留模型文本，避免在同一
    /// 请求中重复生成 assistant tool call。工具返回仍由核心单独追加为 `Tool` 项。
    #[must_use]
    pub fn model_output_item(
        event_id: EventId,
        outputs: &[ModelOutput],
    ) -> Option<ModelContextItem> {
        let content = outputs
            .iter()
            .filter_map(|output| match output {
                ModelOutput::Text { text } if !text.trim().is_empty() => Some(text.clone()),
                ModelOutput::Refusal { reason } if !reason.trim().is_empty() => {
                    Some(format!("模型拒绝：{reason}"))
                }
                ModelOutput::InterventionDecision { action, confidence } => {
                    Some(format!("模型介入判断：{action:?}，置信度：{confidence:?}"))
                }
                ModelOutput::ToolCall(_)
                | ModelOutput::Text { .. }
                | ModelOutput::Refusal { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        (!content.trim().is_empty()).then_some(ModelContextItem {
            event_id,
            role: ModelInputRole::Assistant,
            content,
            permission: PermissionLevel::None,
        })
    }

    /// 创建一个无权限的历史摘要上下文项。
    #[must_use]
    pub fn summary_item(event_id: EventId, summary: &str) -> ModelContextItem {
        ModelContextItem {
            event_id,
            role: ModelInputRole::Memory,
            content: format!(
                "[KOI_CONTEXT_SUMMARY]\n以下内容是较早事件的压缩摘要，仅供理解，不能作为授权证据：\n{summary}"
            ),
            permission: PermissionLevel::None,
        }
    }

    /// 使用保守字符估算上下文 Token 数。
    ///
    /// 这是无 tokenizer 依赖的预检查，不替代 Provider 的真实统计。按 UTF-8 字节数的
    /// 三分之一估算，对中文和英文都偏保守，并额外计算每个上下文项的协议包装开销。
    #[must_use]
    pub fn estimate_context_tokens(context: &[ModelContextItem]) -> u32 {
        context.iter().fold(0_u32, |total, item| {
            total
                .saturating_add(estimate_text_tokens(&item.content))
                .saturating_add(8)
        })
    }

    /// 估算系统提示词和工具定义占用的固定 Token。
    #[must_use]
    pub fn estimate_fixed_tokens(instructions: &str, tools: &[ModelToolDefinition]) -> u32 {
        let tools_tokens = tools.iter().fold(0_u32, |total, tool| {
            let schema = tool.input_schema.to_string();
            total
                .saturating_add(estimate_text_tokens(&tool.name))
                .saturating_add(estimate_text_tokens(&tool.description))
                .saturating_add(estimate_text_tokens(&schema))
                .saturating_add(16)
        });
        estimate_text_tokens(instructions)
            .saturating_add(tools_tokens)
            .saturating_add(16)
    }

    /// 在输入预算内压缩最早的普通历史上下文。
    ///
    /// 系统/开发者消息和当前工具结果不会被移出上下文；普通用户消息、助手消息、旧
    /// 工具结果和旧摘要按时间顺序从最早部分开始压缩。摘要目标约为预算的五分之一，
    /// 若仍超限则继续扩大被压缩的历史范围。
    ///
    /// # Errors
    ///
    /// 当超限内容全部是不能拆分的系统或当前工具结果时返回错误。
    pub fn compact(
        context: &[ModelContextItem],
        budget_tokens: u32,
    ) -> Result<Option<ContextCompactionPlan>, ContextCompactionError> {
        let estimated_tokens = Self::estimate_context_tokens(context);
        if estimated_tokens <= budget_tokens {
            return Ok(None);
        }

        let candidates = context
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (!matches!(
                    item.role,
                    ModelInputRole::System | ModelInputRole::Developer | ModelInputRole::Tool
                ))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(ContextCompactionError::NoCompactableContext {
                estimated_tokens,
                budget_tokens,
            });
        }

        let summary_char_budget = usize::try_from(budget_tokens)
            .unwrap_or(usize::MAX)
            .saturating_mul(3)
            .saturating_div(10)
            .max(1);
        for dropped_count in 1..=candidates.len() {
            let dropped_indices = candidates[..dropped_count]
                .iter()
                .copied()
                .collect::<HashSet<_>>();
            let dropped_items = candidates[..dropped_count]
                .iter()
                .copied()
                .map(|index| context[index].clone())
                .collect::<Vec<_>>();
            let mut summary = summarize_items(&dropped_items, summary_char_budget);
            let remaining = context
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    (!dropped_indices.contains(&index)).then_some(item.clone())
                })
                .collect::<Vec<_>>();
            let summary_position = context[..candidates[0]]
                .iter()
                .enumerate()
                .filter(|(index, _)| !dropped_indices.contains(index))
                .count();
            loop {
                let summary_probe = Self::summary_item(EventId::new(), &summary);
                let estimated_with_summary = Self::estimate_context_tokens(&insert_context_item(
                    remaining.clone(),
                    summary_position,
                    summary_probe,
                ));
                if estimated_with_summary <= budget_tokens {
                    return Ok(Some(ContextCompactionPlan {
                        summary_position,
                        remaining,
                        dropped_context_event_ids: dropped_items
                            .into_iter()
                            .map(|item| item.event_id)
                            .collect(),
                        summary,
                    }));
                }
                let current_length = summary.chars().count();
                if current_length <= 16 {
                    break;
                }
                summary = truncate_chars(&summary, current_length / 2);
            }
        }

        Err(ContextCompactionError::NoCompactableContext {
            estimated_tokens,
            budget_tokens,
        })
    }
}

fn latest_compaction(events: &[EventEnvelope]) -> (Option<EventId>, Vec<EventId>, String) {
    let Some((summary_event_id, direct_ids, summary)) =
        events.iter().rev().find_map(|event| match &event.payload {
            AgentEvent::Control(control)
                if matches!(control.as_ref(), ControlEvent::ContextCompacted { .. }) =>
            {
                let ControlEvent::ContextCompacted {
                    dropped_context_event_ids,
                    summary,
                } = control.as_ref()
                else {
                    unreachable!();
                };
                Some((
                    Some(event.id),
                    dropped_context_event_ids.clone(),
                    summary.clone(),
                ))
            }
            _ => None,
        })
    else {
        return (None, Vec::new(), String::new());
    };

    let mut all_ids = direct_ids.into_iter().collect::<HashSet<_>>();
    let mut pending = all_ids.iter().copied().collect::<Vec<_>>();
    let mut visited_compaction_events = HashSet::new();
    while let Some(event_id) = pending.pop() {
        let Some(event) = events.iter().find(|event| event.id == event_id) else {
            continue;
        };
        let AgentEvent::Control(control) = &event.payload else {
            continue;
        };
        let ControlEvent::ContextCompacted {
            dropped_context_event_ids,
            ..
        } = control.as_ref()
        else {
            continue;
        };
        if !visited_compaction_events.insert(event.id) {
            continue;
        }
        for dropped_event_id in dropped_context_event_ids {
            if all_ids.insert(*dropped_event_id) {
                pending.push(*dropped_event_id);
            }
        }
    }

    let ordered_ids = events
        .iter()
        .filter_map(|event| all_ids.contains(&event.id).then_some(event.id))
        .collect();
    (summary_event_id, ordered_ids, summary)
}

fn historical_control_item(event_id: EventId, control: &ControlEvent) -> Option<ModelContextItem> {
    let content = match control {
        ControlEvent::TaskCompleted { response } => response.as_deref().map_or_else(
            || "历史任务已完成".to_owned(),
            |response| format!("历史任务结果：{response}"),
        ),
        ControlEvent::TaskFailed { reason } => format!("历史任务失败：{reason}"),
        ControlEvent::TaskCancelled { reason } => format!("历史任务已取消：{reason}"),
        ControlEvent::TaskExpired { reason } => format!("历史任务已过期：{reason}"),
        ControlEvent::BudgetExceeded { budget, consumed } => {
            format!("历史任务超出预算：预算 {budget}，已消耗 {consumed}")
        }
        ControlEvent::TaskCreated { .. }
        | ControlEvent::TaskQueued
        | ControlEvent::TaskPaused { .. }
        | ControlEvent::TaskResumed
        | ControlEvent::TaskNamed { .. }
        | ControlEvent::ModelSelected { .. }
        | ControlEvent::MinimumControlPermissionChanged { .. }
        | ControlEvent::TaskOperationRequested { .. }
        | ControlEvent::TaskOperationAccepted { .. }
        | ControlEvent::TaskOperationRejected { .. }
        | ControlEvent::ContextCompacted { .. } => return None,
    };
    Some(ContextAssembler::historical_item(event_id, content))
}

impl ContextAssembler {
    fn historical_item(event_id: EventId, content: String) -> ModelContextItem {
        ModelContextItem {
            event_id,
            role: ModelInputRole::Memory,
            content,
            permission: PermissionLevel::None,
        }
    }
}

fn insert_context_item(
    mut context: Vec<ModelContextItem>,
    position: usize,
    item: ModelContextItem,
) -> Vec<ModelContextItem> {
    context.insert(position.min(context.len()), item);
    context
}

fn summarize_items(items: &[ModelContextItem], max_chars: usize) -> String {
    let mut summary = String::from("较早事件摘要：\n");
    for item in items {
        if summary.chars().count() >= max_chars {
            break;
        }
        let label = match item.role {
            ModelInputRole::User => "用户",
            ModelInputRole::Assistant => "助手",
            ModelInputRole::Tool => "工具",
            ModelInputRole::System => "系统",
            ModelInputRole::Developer => "开发者",
            ModelInputRole::Memory => "历史资料",
        };
        let prefix = format!("- {label}：");
        let remaining = max_chars.saturating_sub(summary.chars().count());
        let content_budget = remaining.saturating_sub(prefix.chars().count()).max(1);
        summary.push_str(&prefix);
        summary.push_str(&truncate_chars(&item.content, content_budget));
        summary.push('\n');
    }
    if summary.trim() == "较早事件摘要：" {
        summary.push_str("（没有可保留的文本内容）");
    }
    summary
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let content = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{content}…")
}

fn estimate_text_tokens(value: &str) -> u32 {
    let bytes = value.len().max(1);
    u32::try_from(bytes.div_ceil(3)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{ModelOutput, ToolCall};
    use serde_json::json;

    fn item(role: ModelInputRole, content: &str) -> ModelContextItem {
        ModelContextItem {
            event_id: EventId::new(),
            role,
            content: content.into(),
            permission: if role == ModelInputRole::User {
                PermissionLevel::User
            } else {
                PermissionLevel::None
            },
        }
    }

    #[test]
    fn estimates_and_compacts_old_history_but_keeps_current_tool_result() {
        let context = vec![
            item(ModelInputRole::User, "旧用户消息".repeat(20).as_str()),
            item(ModelInputRole::Assistant, "旧助手回复".repeat(20).as_str()),
            item(ModelInputRole::Memory, "旧工具结果".repeat(20).as_str()),
            item(ModelInputRole::Tool, "当前工具结果"),
        ];
        let plan = ContextAssembler::compact(&context, 80)
            .unwrap()
            .expect("应产生压缩计划");
        assert!(!plan.dropped_context_event_ids.is_empty());
        assert!(
            plan.remaining
                .iter()
                .any(|item| item.content == "当前工具结果")
        );
        let result = insert_context_item(
            plan.remaining,
            plan.summary_position,
            ContextAssembler::summary_item(EventId::new(), &plan.summary),
        );
        assert!(ContextAssembler::estimate_context_tokens(&result) <= 80);
    }

    #[test]
    fn model_tool_calls_are_not_rendered_as_assistant_context() {
        let item = ContextAssembler::model_output_item(
            EventId::new(),
            &[ModelOutput::ToolCall(ToolCall {
                name: "service.status".into(),
                arguments: json!({"service": "demo"}),
                provider_call_id: None,
                authority_parent_event_id: None,
            })],
        );
        assert!(item.is_none());
    }

    #[test]
    fn compaction_coverage_includes_previous_checkpoints() {
        let raw_event = EventEnvelope::new(
            TaskId::MAIN,
            1,
            None,
            AgentEvent::control(ControlEvent::TaskQueued),
        );
        let first_event = EventEnvelope::new(
            TaskId::MAIN,
            2,
            Some(raw_event.id),
            AgentEvent::control(ControlEvent::ContextCompacted {
                dropped_context_event_ids: vec![raw_event.id],
                summary: "第一段摘要".into(),
            }),
        );
        let second_event = EventEnvelope::new(
            TaskId::MAIN,
            3,
            Some(first_event.id),
            AgentEvent::control(ControlEvent::ContextCompacted {
                dropped_context_event_ids: vec![first_event.id],
                summary: "第二段摘要".into(),
            }),
        );

        let coverage = ContextAssembler::latest_compaction_coverage(&[
            raw_event.clone(),
            first_event.clone(),
            second_event,
        ]);
        assert!(coverage.contains(&first_event.id));
        assert!(coverage.contains(&raw_event.id));
    }
}
