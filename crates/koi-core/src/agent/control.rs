use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::agent::{RuntimeError, TaskRuntime};
use crate::domain::{
    AgentEvent, ControlEvent, EventEnvelope, EventId, EventProvenance, EventSource,
    PermissionLevel, Principal, SourceName,
};
use crate::ports::EventStore;

/// 外部适配器已验证的控制指令直接来源。
///
/// 该值必须由来源注册、身份认证和权限截断流程产生；它不接受模型或工具作为直接来源。
#[derive(Clone, Debug)]
pub struct DirectControlAuthority {
    pub source: EventSource,
    pub principal: Option<Principal>,
    pub permission: PermissionLevel,
    pub expires_at: Option<DateTime<Utc>>,
}

impl DirectControlAuthority {
    /// 构造一个已由外部来源适配器验证的直接控制权限。
    ///
    /// # Errors
    ///
    /// 当来源非法、权限为空或授权已经过期时返回错误。
    pub fn external(
        source: SourceName,
        principal: Principal,
        permission: PermissionLevel,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<Self, ControlExecutionError> {
        let authority = Self {
            source: EventSource::External(source),
            principal: Some(principal),
            permission,
            expires_at,
        };
        authority.validate()?;
        Ok(authority)
    }

    /// 构造核心内部的系统控制权限。
    #[must_use]
    pub const fn system() -> Self {
        Self {
            source: EventSource::System,
            principal: None,
            permission: PermissionLevel::System,
            expires_at: None,
        }
    }

    fn validate(&self) -> Result<(), ControlExecutionError> {
        if !matches!(self.source, EventSource::External(_) | EventSource::System) {
            return Err(ControlExecutionError::InvalidDirectSource);
        }
        if !self.permission.can_authorize() {
            return Err(ControlExecutionError::NoPermission);
        }
        if self
            .expires_at
            .is_some_and(|expires_at| expires_at <= Utc::now())
        {
            return Err(ControlExecutionError::ExpiredAuthority);
        }
        Ok(())
    }
}

/// 已验证来源提交给控制执行器的请求。
#[derive(Clone, Debug)]
pub struct ControlExecutionRequest {
    pub event: ControlEvent,
    pub authority: DirectControlAuthority,
    pub causation_id: Option<EventId>,
}

/// 控制事件的确定性执行入口。
///
/// 控制事件不进入模型上下文，也不会以权限父事件形式传播。调用前必须由来源适配器完成
/// 身份认证；执行器只接受其截断后的直接权限。
pub struct ControlExecutor;

impl ControlExecutor {
    /// 执行并持久化一条控制事件。
    ///
    /// # Errors
    ///
    /// 当来源无效、权限不足、尝试由外部执行内部控制事件、最低权限配置非法或状态迁移
    /// 非法时返回错误。
    pub async fn execute<S>(
        runtime: &mut TaskRuntime<S>,
        request: ControlExecutionRequest,
    ) -> Result<EventEnvelope, ControlExecutionError>
    where
        S: EventStore,
    {
        request.authority.validate()?;
        let is_system = request.authority.source == EventSource::System;
        if !is_system && !is_external_control(&request.event) {
            return Err(ControlExecutionError::InternalControlEvent);
        }

        let required = runtime.projection().minimum_control_permission;
        if !is_system && !request.authority.permission.allows(required) {
            return Err(ControlExecutionError::InsufficientPermission {
                required,
                actual: request.authority.permission,
            });
        }
        if let ControlEvent::MinimumControlPermissionChanged { minimum_permission } = &request.event
        {
            if !minimum_permission.allows(PermissionLevel::User) {
                return Err(ControlExecutionError::InvalidMinimumPermission);
            }
            if !request.authority.permission.allows(*minimum_permission) {
                return Err(ControlExecutionError::CannotRaiseMinimumPermission {
                    requested: *minimum_permission,
                    actual: request.authority.permission,
                });
            }
        }

        runtime
            .record_with_provenance(
                AgentEvent::control(request.event),
                request.causation_id,
                EventProvenance {
                    creator: request.authority.source,
                    direct_permission: Some(request.authority.permission),
                    authority_parent_event_id: None,
                    expires_at: request.authority.expires_at,
                },
            )
            .await
            .map_err(Into::into)
    }
}

fn is_external_control(event: &ControlEvent) -> bool {
    matches!(
        event,
        ControlEvent::TaskPaused { .. }
            | ControlEvent::TaskResumed
            | ControlEvent::TaskCancelled { .. }
            | ControlEvent::MinimumControlPermissionChanged { .. }
    )
}

#[derive(Debug, Error)]
pub enum ControlExecutionError {
    #[error("控制事件直接来源只能是外部来源或系统")]
    InvalidDirectSource,
    #[error("控制事件不具备有效权限")]
    NoPermission,
    #[error("控制事件的直接来源权限已过期")]
    ExpiredAuthority,
    #[error("外部来源不能执行内部控制事件")]
    InternalControlEvent,
    #[error("控制事件需要 {required:?} 权限，当前为 {actual:?}")]
    InsufficientPermission {
        required: PermissionLevel,
        actual: PermissionLevel,
    },
    #[error("最低控制权限不能低于 User")]
    InvalidMinimumPermission,
    #[error("不能将最低控制权限提高到 {requested:?}，当前直接权限为 {actual:?}")]
    CannotRaiseMinimumPermission {
        requested: PermissionLevel,
        actual: PermissionLevel,
    },
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}
