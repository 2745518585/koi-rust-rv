use async_trait::async_trait;
use thiserror::Error;

use crate::domain::{MemoryQuery, MemorySearchResult, MemoryWriteRequest};

/// 持久化与检索跨任务记忆的端口。
///
/// 实现可使用 SQLite、Postgres 或向量数据库，但必须尊重核心提供的作用域、过期时间、
/// 结果数量和 Token 预算。实现位于 `koi-infra::memory`。
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// 追加一条已通过核心策略校验的记忆。
    ///
    /// # Errors
    ///
    /// 当底层存储不可用或无法保证写入持久性时返回错误。
    async fn append(&self, request: MemoryWriteRequest) -> Result<(), MemoryError>;

    /// 按作用域和检索条件返回相关记忆。
    ///
    /// 返回条目不应包含已过期记忆；核心仍会在注入模型前将每条结果转为 `None` 权限。
    ///
    /// # Errors
    ///
    /// 当查询非法、索引不可用或底层存储查询失败时返回错误。
    async fn search(&self, query: MemoryQuery) -> Result<Vec<MemorySearchResult>, MemoryError>;
}

/// 记忆存储实现返回的统一错误。
#[derive(Debug, Error)]
#[error("记忆操作失败：{message}")]
pub struct MemoryError {
    pub message: String,
}

impl MemoryError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
