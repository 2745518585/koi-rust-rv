use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::domain::{ModelError, ModelProviderDescriptor, ModelRequest, ModelStreamEvent};

/// 模型流中的每一项要么是增量输出，要么是调用完成结果。
pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ModelError>> + Send>>;

/// 模型供应商适配器的核心端口。
///
/// 实现位于 `koi-infra::llm`：Responses 与 Chat Completions 均将各自协议转换为
/// 该端口定义的请求、流和结果。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// 返回当前配置下 Provider 的协议、模型和能力描述。
    fn descriptor(&self) -> ModelProviderDescriptor;

    /// 启动一次模型调用并返回规范化流。
    ///
    /// 即使底层接口不支持流式输出，也应返回只包含一个 `Completed` 项的流。
    ///
    /// # Errors
    ///
    /// 当请求非法、调用被取消、网络或供应商接口失败时返回错误。
    async fn start(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError>;
}
