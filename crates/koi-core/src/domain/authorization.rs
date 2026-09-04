use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{EventId, EventSource, PermissionLevel, Principal, TaskId, ToolDefinition};

/// 授权证据对应事件的顶级类别。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuthorizationEvidenceEventKind {
    Ingress,
    Control,
    Model,
    Tool,
}

/// 一条可用于工具授权的、已由事件读取层验证的输入证据。
///
/// 控制事件与 System 来源的核心内部事件可以携带很高的直接权限（例如 `System`），
/// 但这些权限只服务于核心自身的运转判定：它们永远不能作为 `authority_parent_event_id`
/// 的目标参与模型的提权审查。工具事件回传以 `None` 权限持久化，可以无条件进入会话
/// 供模型分析，但同样不能带来权限提升。模型、工具来源在没有合法上级来源时不能获得
/// 权限。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthorizationEvidence {
    pub event_id: EventId,
    pub source: EventSource,
    pub event_kind: AuthorizationEvidenceEventKind,
    pub principal: Option<Principal>,
    /// 当前来源注册配置或已核定直接来源权限所允许的最高等级。
    pub source_maximum_permission: PermissionLevel,
    pub permission: PermissionLevel,
    pub status: AuthorizationEvidenceStatus,
    pub authority_parent_event_id: Option<EventId>,
    pub expires_at: Option<DateTime<Utc>>,
    /// 若此证据来自一次补充授权，必须绑定对应的授权请求事件。
    pub approval_request_event_id: Option<EventId>,
}

/// 来源方对指令有效性的判定结果。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuthorizationEvidenceStatus {
    Active,
    Expired,
    Revoked,
}

impl AuthorizationEvidence {
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.status == AuthorizationEvidenceStatus::Active
            && !self.is_expired()
            && self.permission.can_authorize()
    }

    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
    }

    /// 该证据对应的事件能否作为其他事件的权限父节点（提权审查的上级）。
    ///
    /// 两类事件被显式排除：控制事件（类别级规则），以及 System 来源的核心内部事件
    /// （来源级规则，例如主会话引导输入与核心写人的控制指令）。它们携带的权限只用
    /// 于核心自身运转，模型与工具永远不能借用。
    #[must_use]
    pub const fn can_be_authority_parent(&self) -> bool {
        !matches!(self.event_kind, AuthorizationEvidenceEventKind::Control)
            && !matches!(self.source, EventSource::System)
    }
}

/// 核心对某个工具提议发起补充授权时交给来源适配器的请求。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthorizationRequest {
    pub task_id: TaskId,
    /// 已持久化的 `Tool::ApprovalRequested` 事件，用于串联回复与原始请求。
    pub approval_request_event_id: EventId,
    pub tool_proposal_event_id: EventId,
    pub tool_name: String,
    /// 工具参数的稳定指纹；来源方展示确认界面时不得改写它。
    pub arguments_hash: String,
    pub required_permission: PermissionLevel,
    pub original_evidence_event_ids: Vec<EventId>,
}

/// 来源方处理补充授权请求后的即时结果。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuthorizationRequestResult {
    /// 来源方已经明确拒绝，例如用户点按拒绝或群聊管理员否决。
    Denied { reason: String },
    /// 来源方已展示确认界面或发送确认消息，稍后会以新的 Ingress 事件继续。
    Pending,
    /// 来源方已提交一条新的输入事件；核心仍须读取并独立验证该事件。
    Authorized { authorization_event_id: EventId },
}

/// 权限检查的可审计结果。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PermissionCheckResult {
    Allowed {
        effective_permission: PermissionLevel,
        evidence_event_ids: Vec<EventId>,
    },
    Insufficient {
        effective_permission: PermissionLevel,
        required_permission: PermissionLevel,
    },
}

/// 依据已验证输入证据进行的确定性权限检查。
pub struct PermissionChecker;

impl PermissionChecker {
    /// 检查可用证据是否达到工具定义的最低权限。
    ///
    /// 无效、过期、撤销或 `None` 权限的证据不会参与计算。多个有效证据中权限最高者
    /// 决定本次调用的有效权限；调用方必须确保补充授权证据绑定了正确的请求事件。
    #[must_use]
    pub fn check(
        definition: &ToolDefinition,
        evidence: &[AuthorizationEvidence],
    ) -> PermissionCheckResult {
        let mut effective_permission = PermissionLevel::None;
        let mut evidence_event_ids = Vec::new();
        let mut seen = HashSet::with_capacity(evidence.len());

        for item in evidence {
            if item.is_usable() && seen.insert(item.event_id) {
                effective_permission = effective_permission.max(item.permission);
                evidence_event_ids.push(item.event_id);
            }
        }

        if effective_permission.allows(definition.required_permission) {
            PermissionCheckResult::Allowed {
                effective_permission,
                evidence_event_ids,
            }
        } else {
            PermissionCheckResult::Insufficient {
                effective_permission,
                required_permission: definition.required_permission,
            }
        }
    }
}

/// 将授权检查结果转换为执行请求前的错误。
#[derive(Debug, Error)]
pub enum PermissionError {
    #[error(
        "工具 {tool_name} 需要 {required_permission:?} 权限，当前最高为 {effective_permission:?}"
    )]
    InsufficientPermission {
        tool_name: String,
        effective_permission: PermissionLevel,
        required_permission: PermissionLevel,
    },
}

impl PermissionCheckResult {
    /// 取得允许调用时的证据；权限不足时返回可向来源方展示的错误。
    ///
    /// # Errors
    ///
    /// 当结果表示权限不足时返回错误。
    pub fn require_allowed(
        self,
        tool_name: impl Into<String>,
    ) -> Result<Vec<EventId>, PermissionError> {
        match self {
            Self::Allowed {
                evidence_event_ids, ..
            } => Ok(evidence_event_ids),
            Self::Insufficient {
                effective_permission,
                required_permission,
            } => Err(PermissionError::InsufficientPermission {
                tool_name: tool_name.into(),
                effective_permission,
                required_permission,
            }),
        }
    }
}
