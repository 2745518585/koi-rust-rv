use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::{EventId, ModelDeltaKind, ModelOutput, PermissionLevel, TaskId, Usage};

/// 供应商无关的模型输入角色。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelInputRole {
    System,
    Developer,
    User,
    Assistant,
    Tool,
    /// 从长期记忆检索的参考资料，Provider 不得将其编码为高优先级指令。
    Memory,
}

/// 一条将被注入模型上下文的文本项。
///
/// 每个上下文项都必须指向已持久化事件，以便后续审计模型实际使用过的信息。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelContextItem {
    pub event_id: EventId,
    pub role: ModelInputRole,
    pub content: String,
    /// 来自原始输入事件的权限上限。它只帮助模型选择证据，不能直接授权模型。
    pub permission: PermissionLevel,
}

/// 模型可见的工具定义。风险等级和实际执行策略仍由核心工具策略处理。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict: bool,
}

/// 对模型最终文本输出施加的约束。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ModelOutputContract {
    Text,
    JsonSchema {
        name: String,
        schema: Value,
        strict: bool,
    },
}

/// 一次生成调用的供应商无关选项。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelGenerationOptions {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<String>,
    pub stream: bool,
    pub allow_parallel_tool_calls: bool,
}

impl Default for ModelGenerationOptions {
    fn default() -> Self {
        Self {
            max_output_tokens: None,
            temperature: None,
            reasoning_effort: None,
            stream: true,
            allow_parallel_tool_calls: false,
        }
    }
}

/// 发送给 `ModelProvider` 的规范化请求。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelRequest {
    pub task_id: TaskId,
    /// 系统指令来自配置或固定人格模板，因此使用内容哈希而非事件 ID 追踪版本。
    pub instructions: String,
    pub instructions_hash: String,
    pub context: Vec<ModelContextItem>,
    pub tools: Vec<ModelToolDefinition>,
    pub output_contract: ModelOutputContract,
    pub options: ModelGenerationOptions,
}

impl ModelRequest {
    /// 校验可由所有 Provider 共同遵守的请求不变量。
    ///
    /// # Errors
    ///
    /// 当上下文事件重复、工具名称重复、Schema 不是对象或采样参数越界时返回错误。
    pub fn validate(&self) -> Result<(), ModelRequestValidationError> {
        if let Some(temperature) = self.options.temperature {
            if !(0.0..=2.0).contains(&temperature) {
                return Err(ModelRequestValidationError::TemperatureOutOfRange(
                    temperature,
                ));
            }
        }

        let mut context_ids = HashSet::with_capacity(self.context.len());
        for item in &self.context {
            if !context_ids.insert(item.event_id) {
                return Err(ModelRequestValidationError::DuplicateContextEvent(
                    item.event_id,
                ));
            }
        }

        let mut tool_names = HashSet::with_capacity(self.tools.len());
        for tool in &self.tools {
            if tool.name.trim().is_empty() {
                return Err(ModelRequestValidationError::EmptyToolName);
            }
            if !tool_names.insert(&tool.name) {
                return Err(ModelRequestValidationError::DuplicateToolName(
                    tool.name.clone(),
                ));
            }
            if !tool.input_schema.is_object() {
                return Err(ModelRequestValidationError::ToolSchemaNotObject(
                    tool.name.clone(),
                ));
            }
        }

        if let ModelOutputContract::JsonSchema { name, schema, .. } = &self.output_contract {
            if name.trim().is_empty() {
                return Err(ModelRequestValidationError::EmptyOutputSchemaName);
            }
            if !schema.is_object() {
                return Err(ModelRequestValidationError::OutputSchemaNotObject);
            }
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ModelRequestValidationError {
    #[error("temperature 必须位于 0 到 2 之间，实际为 {0}")]
    TemperatureOutOfRange(f32),
    #[error("上下文事件 {0} 在同一模型请求中重复")]
    DuplicateContextEvent(EventId),
    #[error("工具名称不能为空")]
    EmptyToolName,
    #[error("工具名称重复：{0}")]
    DuplicateToolName(String),
    #[error("工具 {0} 的输入 Schema 必须是 JSON 对象")]
    ToolSchemaNotObject(String),
    #[error("输出 Schema 名称不能为空")]
    EmptyOutputSchemaName,
    #[error("输出 Schema 必须是 JSON 对象")]
    OutputSchemaNotObject,
}

/// 一个模型调用完成后返回的规范化结果。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelTurn {
    pub outputs: Vec<ModelOutput>,
    pub usage: Usage,
    /// 仅用于同一供应商的续写优化和审计，核心权限逻辑不会信任它。
    pub provider_response_id: Option<String>,
}

/// 模型调用过程中可增量产生的事件。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ModelStreamEvent {
    Delta {
        sequence: u32,
        kind: ModelDeltaKind,
        content: String,
    },
    Completed(ModelTurn),
}

/// Provider 的协议类别，用于配置校验和 Web UI 展示。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelProtocol {
    Responses,
    ChatCompletions,
}

/// Provider 可提供的单项能力。能力只影响请求编码，不授予工具权限。
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ModelCapability {
    Streaming,
    NativeToolCalls,
    StructuredOutput,
    ProviderManagedConversation,
}

/// Provider 的能力集合。
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelCapabilities {
    pub supported: BTreeSet<ModelCapability>,
}

impl ModelCapabilities {
    #[must_use]
    pub fn new(capabilities: impl IntoIterator<Item = ModelCapability>) -> Self {
        Self {
            supported: capabilities.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn supports(&self, capability: ModelCapability) -> bool {
        self.supported.contains(&capability)
    }
}

/// 一个已配置模型 Provider 的稳定描述。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelProviderDescriptor {
    pub provider: String,
    pub model: String,
    pub protocol: ModelProtocol,
    pub capabilities: ModelCapabilities,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelErrorKind {
    Cancelled,
    Timeout,
    RateLimited,
    Unavailable,
    InvalidResponse,
    UnsupportedCapability,
    Internal,
}

/// 由模型 Provider 统一返回的错误。
#[derive(Debug, Error)]
#[error("模型调用失败（{kind:?}）：{message}")]
pub struct ModelError {
    pub kind: ModelErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl ModelError {
    #[must_use]
    pub fn new(kind: ModelErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }
}
