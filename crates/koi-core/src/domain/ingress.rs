use serde::{Deserialize, Serialize};

use super::{ContextEnvelope, EventId, PermissionLevel, Principal, Scope};

/// 外部来源提交给核心的输入草稿。
///
/// 草稿不含 `EventId`、任务序号或实际权限。`suggested_permission` 只是来源建议，核心
/// 会在注册时按来源和身份权限重新计算 `effective_permission`。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum IngressDraft {
    Context {
        context: Box<ContextEnvelope>,
        suggested_permission: PermissionLevel,
    },
    Approval {
        approval_request_event_id: EventId,
        principal: Principal,
        scope: Scope,
        suggested_permission: PermissionLevel,
        approved: bool,
    },
    Cancellation {
        principal: Principal,
        scope: Scope,
        suggested_permission: PermissionLevel,
        reason: String,
    },
}

impl IngressDraft {
    /// 返回来源名称。该名称必须已在核心的来源权限注册表中登记。
    #[must_use]
    pub fn source(&self) -> &str {
        match self {
            Self::Context { context, .. } => &context.origin.source,
            Self::Approval { principal, .. } | Self::Cancellation { principal, .. } => {
                &principal.source
            }
        }
    }

    /// 返回身份与作用域，供核心权限解析器确定该输入的最高权限。
    #[must_use]
    pub fn subject(&self) -> IngressSubject {
        match self {
            Self::Context { context, .. } => IngressSubject {
                source: context.origin.source.clone(),
                principal: context.actor.clone(),
                scope: context.scope.clone(),
            },
            Self::Approval {
                principal, scope, ..
            }
            | Self::Cancellation {
                principal, scope, ..
            } => IngressSubject {
                source: principal.source.clone(),
                principal: Some(principal.clone()),
                scope: scope.clone(),
            },
        }
    }

    #[must_use]
    pub const fn suggested_permission(&self) -> PermissionLevel {
        match self {
            Self::Context {
                suggested_permission,
                ..
            }
            | Self::Approval {
                suggested_permission,
                ..
            }
            | Self::Cancellation {
                suggested_permission,
                ..
            } => *suggested_permission,
        }
    }
}

/// 交给核心身份权限解析器的稳定身份描述。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IngressSubject {
    pub source: String,
    pub principal: Option<Principal>,
    pub scope: Scope,
}
