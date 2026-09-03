use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::{EventId, PermissionLevel, TaskId, ToolCall};

/// 工具对外部世界可能造成的影响。
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum ToolSideEffect {
    /// 只读取数据，不改变外部状态。
    ReadOnly,
    /// 向外部渠道发送消息。
    Notification,
    /// 改变可恢复的外部状态，例如重启服务。
    Stateful,
    /// 可能造成难以恢复影响的操作。
    Destructive,
}

/// 工具的声明式元数据。
///
/// 注册表、模型上下文、Web 管理界面和策略模块都使用这份定义；具体执行逻辑由
/// `ToolExecutor` 的 Rust 实现提供。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub required_permission: PermissionLevel,
    pub side_effect: ToolSideEffect,
    pub timeout_ms: u64,
    pub model_visible: bool,
}

impl ToolDefinition {
    /// 校验所有工具实现都必须满足的元数据不变量。
    ///
    /// # Errors
    ///
    /// 当名称、描述、参数 Schema、权限或超时设置不合法时返回错误。
    pub fn validate(&self) -> Result<(), ToolDefinitionValidationError> {
        if self.name.trim().is_empty() {
            return Err(ToolDefinitionValidationError::EmptyName);
        }
        if self.description.trim().is_empty() {
            return Err(ToolDefinitionValidationError::EmptyDescription);
        }
        if !self.input_schema.is_object() {
            return Err(ToolDefinitionValidationError::InputSchemaNotObject);
        }
        if self.required_permission == PermissionLevel::None {
            return Err(ToolDefinitionValidationError::NoRequiredPermission);
        }
        if self.timeout_ms == 0 {
            return Err(ToolDefinitionValidationError::ZeroTimeout);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ToolDefinitionValidationError {
    #[error("工具名称不能为空")]
    EmptyName,
    #[error("工具描述不能为空")]
    EmptyDescription,
    #[error("工具输入 Schema 必须是 JSON 对象")]
    InputSchemaNotObject,
    #[error("工具最低权限不能为 None")]
    NoRequiredPermission,
    #[error("工具超时必须大于零")]
    ZeroTimeout,
}

/// 已通过核心策略审查、可以交给工具执行器的调用。
///
/// 构造它之前，核心必须先验证工具参数、授权证据、权限等级与目标资源范围。工具
/// 执行器不负责重新解释群聊或 Web 的授权语义。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AuthorizedToolInvocation {
    pub task_id: TaskId,
    pub proposal_event_id: EventId,
    pub execution_started_event_id: EventId,
    pub tool_call: ToolCall,
    /// 此次执行实际采纳的输入证据。模型生成、工具输出等 `None` 权限事件不得出现。
    pub authorization_evidence_event_ids: Vec<EventId>,
}

impl AuthorizedToolInvocation {
    /// 校验调用在交给执行器前具有可审计的授权链。
    ///
    /// # Errors
    ///
    /// 当工具名称为空、没有授权证据或授权证据重复时返回错误。
    pub fn validate(&self) -> Result<(), ToolInvocationValidationError> {
        if self.tool_call.name.trim().is_empty() {
            return Err(ToolInvocationValidationError::EmptyToolName);
        }
        if self.authorization_evidence_event_ids.is_empty() {
            return Err(ToolInvocationValidationError::MissingAuthorizationEvidence);
        }

        let mut evidence = HashSet::with_capacity(self.authorization_evidence_event_ids.len());
        for event_id in &self.authorization_evidence_event_ids {
            if !evidence.insert(*event_id) {
                return Err(
                    ToolInvocationValidationError::DuplicateAuthorizationEvidence(*event_id),
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ToolInvocationValidationError {
    #[error("工具名称不能为空")]
    EmptyToolName,
    #[error("工具调用缺少授权证据")]
    MissingAuthorizationEvidence,
    #[error("授权证据事件 {0} 重复")]
    DuplicateAuthorizationEvidence(EventId),
}

/// 工具执行器返回的统一错误。
#[derive(Debug, Error)]
#[error("工具调用失败（{kind:?}）：{message}")]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
    pub retryable: bool,
}

impl ToolError {
    #[must_use]
    pub fn new(kind: ToolErrorKind, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ToolErrorKind {
    Cancelled,
    Timeout,
    InvalidArguments,
    TargetUnavailable,
    ExecutionFailed,
    Internal,
}
