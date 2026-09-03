use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    AuthorizedToolInvocation, ToolDefinition, ToolDefinitionValidationError, ToolError, ToolResult,
};

/// 一种具体工具的执行边界。实现通常位于 `koi-infra::tools`。
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// 返回不可变的工具元数据。
    fn definition(&self) -> &ToolDefinition;

    /// 执行已获授权的调用。
    ///
    /// # Errors
    ///
    /// 当调用被取消、超时、参数非法或底层目标执行失败时返回错误。
    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError>;
}

/// 核心使用的进程内工具注册表。
///
/// 它只负责注册、查询和转交已授权调用；权限判断、参数 Schema 校验和审计事件记录
/// 由 Agent 主循环及策略模块完成。
#[derive(Default)]
pub struct ToolRegistry {
    executors: BTreeMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolRegistry {
    /// 注册一个工具。工具名称在当前进程内必须唯一。
    ///
    /// # Errors
    ///
    /// 当元数据不合法或名称已被注册时返回错误。
    pub fn register(
        &mut self,
        executor: Arc<dyn ToolExecutor>,
    ) -> Result<(), ToolRegistrationError> {
        let definition = executor.definition();
        definition.validate()?;

        if self.executors.contains_key(&definition.name) {
            return Err(ToolRegistrationError::DuplicateTool(
                definition.name.clone(),
            ));
        }

        self.executors.insert(definition.name.clone(), executor);
        Ok(())
    }

    /// 返回指定工具的元数据副本。
    #[must_use]
    pub fn get_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.executors
            .get(name)
            .map(|executor| executor.definition().clone())
    }

    /// 按名称稳定排序返回全部工具元数据。
    #[must_use]
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.executors
            .values()
            .map(|executor| executor.definition().clone())
            .collect()
    }

    /// 调用名称匹配的工具执行器。
    ///
    /// 该方法拒绝没有授权证据的调用；调用方仍须在此前完成证据来源、权限等级和参数
    /// Schema 的完整校验。
    ///
    /// # Errors
    ///
    /// 当调用格式不合法、工具不存在或执行器返回错误时返回错误。
    pub async fn invoke(
        &self,
        invocation: AuthorizedToolInvocation,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolInvocationError> {
        invocation.validate()?;
        let tool_name = invocation.tool_call.name.clone();
        let executor = self
            .executors
            .get(&tool_name)
            .ok_or(ToolInvocationError::ToolNotFound(tool_name))?;

        executor
            .execute(invocation, cancel)
            .await
            .map_err(Into::into)
    }
}

#[derive(Debug, Error)]
pub enum ToolRegistrationError {
    #[error(transparent)]
    InvalidDefinition(#[from] ToolDefinitionValidationError),
    #[error("工具已注册：{0}")]
    DuplicateTool(String),
}

#[derive(Debug, Error)]
pub enum ToolInvocationError {
    #[error(transparent)]
    InvalidInvocation(#[from] crate::domain::ToolInvocationValidationError),
    #[error("未注册工具：{0}")]
    ToolNotFound(String),
    #[error(transparent)]
    ExecutionFailed(#[from] ToolError),
}
