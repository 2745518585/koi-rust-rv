use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use tokio_util::sync::CancellationToken;

use crate::domain::{ModelError, ModelProviderDescriptor, ModelRequest, ModelStreamEvent, TaskId};

/// 模型流中的每一项要么是增量输出，要么是调用完成结果。
pub type ModelEventStream =
    Pin<Box<dyn Stream<Item = Result<ModelStreamEvent, ModelError>> + Send>>;

/// 模型供应商适配器的核心端口。
///
/// 实现位于 `koi-infra::llm`：Responses 与 Chat Completions 均将各自协议转换为
/// 该端口定义的请求、流和结果。
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// 返回当前配置下 Provider 的供应商、模型 ID、协议和能力描述。
    fn descriptor(&self) -> ModelProviderDescriptor;

    /// 返回模型的上下文窗口上限（Token）。
    ///
    /// OpenAI-compatible 网关不一定会在 `/models` 响应中提供该字段，因此未知时返回
    /// `None`，核心会使用保守默认值并在真正调用前自行压缩上下文。
    fn context_window_tokens(&self) -> Option<u32> {
        None
    }

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

    /// 清理任务级的 Provider 续接状态。
    ///
    /// 当一次模型调用被取消、超时、流异常结束，或者任务执行失败后，调用方会调用此
    /// 方法。Provider 不得把未完成调用中的工具调用 ID 带入下一次独立模型请求。没有
    /// 续接状态的 Provider 可以保留默认的空实现。
    fn reset_task(&self, _task_id: TaskId) {}
}
