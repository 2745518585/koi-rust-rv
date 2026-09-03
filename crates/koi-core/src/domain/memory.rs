use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{EventId, ModelContextItem, ModelInputRole, PermissionLevel, Scope};

/// 记忆的业务类别，用于筛选、保留与展示。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MemoryKind {
    ConversationSummary,
    ServiceFact,
    IncidentConclusion,
    Runbook,
    UserPreference,
}

/// 一条长期或跨任务记忆的可审计来源。
///
/// 不提供“模型直接写入”变体：模型只能建议记忆，Agent 主循环须将建议转换为已经验证
/// 的工具结果、管理员确认或系统摘要后才能写入。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum MemoryOrigin {
    VerifiedToolResult { event_id: EventId },
    AdministratorConfirmation { event_id: EventId },
    SystemSummary { event_id: EventId },
}

impl MemoryOrigin {
    #[must_use]
    pub const fn event_id(&self) -> EventId {
        match self {
            Self::VerifiedToolResult { event_id }
            | Self::AdministratorConfirmation { event_id }
            | Self::SystemSummary { event_id } => *event_id,
        }
    }
}

/// 可追加保存的记忆条目。
///
/// `id` 使用统一的 `EventId`，应与记录本次记忆写入的核心事件一致。记忆条目本身不
/// 携带可执行权限；其内容在注入模型时始终降级为 `PermissionLevel::None`。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryRecord {
    pub id: EventId,
    pub schema_version: u16,
    pub kind: MemoryKind,
    pub scopes: Vec<Scope>,
    pub content: String,
    pub metadata: BTreeMap<String, String>,
    pub origin: MemoryOrigin,
    pub source_event_ids: Vec<EventId>,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl MemoryRecord {
    /// 生成模型上下文项。记忆始终是参考资料，不能提供指令或工具调用权限。
    #[must_use]
    pub fn as_model_context(&self) -> ModelContextItem {
        ModelContextItem {
            event_id: self.id,
            role: ModelInputRole::Memory,
            content: self.content.clone(),
            permission: PermissionLevel::None,
        }
    }

    /// 校验记忆条目的来源、作用域与生命周期。
    ///
    /// # Errors
    ///
    /// 当条目版本、内容、作用域、来源事件或过期时间不合法时返回错误。
    pub fn validate(&self) -> Result<(), MemoryValidationError> {
        if self.schema_version == 0 {
            return Err(MemoryValidationError::ZeroSchemaVersion);
        }
        if self.content.trim().is_empty() {
            return Err(MemoryValidationError::EmptyContent);
        }
        if self.scopes.is_empty() {
            return Err(MemoryValidationError::MissingScope);
        }
        if self.source_event_ids.is_empty() {
            return Err(MemoryValidationError::MissingSourceEvents);
        }

        let mut source_events = HashSet::with_capacity(self.source_event_ids.len());
        for event_id in &self.source_event_ids {
            if !source_events.insert(*event_id) {
                return Err(MemoryValidationError::DuplicateSourceEvent(*event_id));
            }
        }
        if !source_events.contains(&self.origin.event_id()) {
            return Err(MemoryValidationError::OriginNotInSourceEvents);
        }
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= self.created_at)
        {
            return Err(MemoryValidationError::InvalidExpiry);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum MemoryValidationError {
    #[error("记忆 schema_version 必须大于零")]
    ZeroSchemaVersion,
    #[error("记忆内容不能为空")]
    EmptyContent,
    #[error("记忆必须至少属于一个作用域")]
    MissingScope,
    #[error("记忆必须至少关联一个来源事件")]
    MissingSourceEvents,
    #[error("来源事件 {0} 重复")]
    DuplicateSourceEvent(EventId),
    #[error("记忆来源必须包含在 source_event_ids 中")]
    OriginNotInSourceEvents,
    #[error("记忆过期时间必须晚于创建时间")]
    InvalidExpiry,
}

/// 写入记忆前的核心请求。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryWriteRequest {
    pub record: MemoryRecord,
}

impl MemoryWriteRequest {
    /// # Errors
    ///
    /// 当记忆条目不符合核心写入规则时返回错误。
    pub fn validate(&self) -> Result<(), MemoryValidationError> {
        self.record.validate()
    }
}

/// 检索记忆的供应商无关条件。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryQuery {
    pub scopes: Vec<Scope>,
    pub text: String,
    pub kinds: Vec<MemoryKind>,
    pub limit: u16,
    pub token_budget: u32,
    pub now: DateTime<Utc>,
}

impl MemoryQuery {
    /// # Errors
    ///
    /// 当检索范围、结果数或 Token 预算不合法时返回错误。
    pub fn validate(&self) -> Result<(), MemoryQueryValidationError> {
        if self.scopes.is_empty() {
            return Err(MemoryQueryValidationError::MissingScope);
        }
        if self.limit == 0 {
            return Err(MemoryQueryValidationError::ZeroLimit);
        }
        if self.token_budget == 0 {
            return Err(MemoryQueryValidationError::ZeroTokenBudget);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum MemoryQueryValidationError {
    #[error("记忆检索必须至少指定一个作用域")]
    MissingScope,
    #[error("记忆检索结果数量必须大于零")]
    ZeroLimit,
    #[error("记忆检索 Token 预算必须大于零")]
    ZeroTokenBudget,
}

/// 检索器返回的记忆及其排序分数。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemorySearchResult {
    pub record: MemoryRecord,
    pub relevance_score: f32,
    /// 存储实现使用当前模型对应的 tokenizer 估算的注入成本。
    pub estimated_tokens: u32,
}

/// 将记忆检索结果变为模型上下文的确定性构建器。
pub struct MemoryContextBuilder;

impl MemoryContextBuilder {
    /// 过滤过期或不合法记忆，按相关度排序后在 Token 预算内选择结果。
    ///
    /// 每一项都经 `MemoryRecord::as_model_context` 降级为 `PermissionLevel::None`，因此
    /// 记忆永远不能成为模型指令或工具授权的依据。
    #[must_use]
    pub fn build(
        query: &MemoryQuery,
        mut results: Vec<MemorySearchResult>,
    ) -> Vec<ModelContextItem> {
        if query.validate().is_err() {
            return Vec::new();
        }

        results.sort_by(|left, right| right.relevance_score.total_cmp(&left.relevance_score));

        let mut selected = Vec::new();
        let mut selected_ids = HashSet::new();
        let mut consumed_tokens = 0_u32;

        for result in results {
            if selected.len() >= usize::from(query.limit)
                || result
                    .record
                    .expires_at
                    .is_some_and(|expires_at| expires_at <= query.now)
                || result.record.validate().is_err()
                || !result
                    .record
                    .scopes
                    .iter()
                    .any(|scope| query.scopes.contains(scope))
                || (!query.kinds.is_empty() && !query.kinds.contains(&result.record.kind))
                || !selected_ids.insert(result.record.id)
            {
                continue;
            }

            let Some(next_total) = consumed_tokens.checked_add(result.estimated_tokens) else {
                continue;
            };
            if next_total > query.token_budget {
                continue;
            }

            consumed_tokens = next_total;
            selected.push(result.record.as_model_context());
        }

        selected
    }
}
