use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

use crate::domain::{
    AuthorizationEvidence, AuthorizationRequest, AuthorizationRequestResult, EventId, TaskId,
};

/// 从已持久化事件中读取基础权限来源信息的端口。
///
/// 实现返回事件自身的创建来源、顶级类别、可选权限父事件、直接权限和有效期；核心据此
/// 递归解析权限链。控制事件可以保留其直接来源权限，但绝不能作为其他事件的权限父节点。
/// 实现不能信任模型自行提供的权限字段。
#[async_trait]
pub trait AuthorizationEvidenceResolver: Send + Sync {
    /// 读取一条事件的基础权限来源信息。
    ///
    /// # Errors
    ///
    /// 当事件不存在、不属于任务或无法验证来源状态时返回错误。
    async fn resolve(
        &self,
        task_id: TaskId,
        event_id: EventId,
    ) -> Result<AuthorizationEvidence, AuthorizationError>;

    /// 按全局事件 ID读取权限证据。
    ///
    /// 主会话委托给子任务的输入会跨任务引用主会话事件。默认实现保持旧版解析器的
    /// fail-closed 行为；支持跨任务权限链的实现应覆盖此方法。
    async fn resolve_any(
        &self,
        _event_id: EventId,
    ) -> Result<AuthorizationEvidence, AuthorizationError> {
        Err(AuthorizationError::new("权限解析器不支持跨任务事件读取"))
    }
}

/// 特定输入来源的补充授权能力，例如 QQ 确认消息或 Web 提权窗口。
#[async_trait]
pub trait SourceAuthorizationProvider: Send + Sync {
    /// 返回该 Provider 负责的稳定来源名称，例如 `qq` 或 `web`。
    fn source(&self) -> &'static str;

    /// 发送或处理一次补充授权请求。
    ///
    /// `Authorized` 返回的事件仍须由核心经 `AuthorizationEvidenceResolver` 复核。
    ///
    /// # Errors
    ///
    /// 当来源不可用、请求无法投递或无法验证来源侧会话时返回错误。
    async fn request_authorization(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationRequestResult, AuthorizationError>;
}

/// 来源授权 Provider 的进程内注册表。
#[derive(Default)]
pub struct SourceAuthorizationRegistry {
    providers: BTreeMap<String, Arc<dyn SourceAuthorizationProvider>>,
}

impl SourceAuthorizationRegistry {
    /// 注册一个来源授权 Provider。来源名称在当前进程内必须唯一。
    ///
    /// # Errors
    ///
    /// 当来源名称为空或同名 Provider 已注册时返回错误。
    pub fn register(
        &mut self,
        provider: Arc<dyn SourceAuthorizationProvider>,
    ) -> Result<(), SourceAuthorizationRegistrationError> {
        let source = provider.source().trim();
        if source.is_empty() {
            return Err(SourceAuthorizationRegistrationError::EmptySource);
        }
        if self.providers.contains_key(source) {
            return Err(SourceAuthorizationRegistrationError::DuplicateSource(
                source.into(),
            ));
        }

        self.providers.insert(source.into(), provider);
        Ok(())
    }

    /// 查询特定来源的授权 Provider。
    #[must_use]
    pub fn get(&self, source: &str) -> Option<Arc<dyn SourceAuthorizationProvider>> {
        self.providers.get(source).cloned()
    }
}

#[derive(Debug, Error)]
pub enum SourceAuthorizationRegistrationError {
    #[error("来源名称不能为空")]
    EmptySource,
    #[error("来源授权 Provider 已注册：{0}")]
    DuplicateSource(String),
}

/// 授权来源、事件读取和交互适配器统一返回的错误。
#[derive(Debug, Error)]
#[error("授权处理失败：{message}")]
pub struct AuthorizationError {
    pub message: String,
}

impl AuthorizationError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
