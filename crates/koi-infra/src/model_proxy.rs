//! 独立模型代理的私有 HTTP 协议。
//!
//! `koi-agent` 仅通过这里的客户端发送规范化模型请求；真正的供应商配置和 API Key
//! 只由运行 `koi-model-proxy` 的进程读取。代理输出 NDJSON，保留核心模型端口的流式
//! 语义，同时不会把供应商 Authorization 请求头传回调用方。

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use koi_core::domain::{
    ModelCapabilities, ModelCapability, ModelError, ModelErrorKind, ModelProviderDescriptor,
    ModelRequest, ModelSelection, ModelStreamEvent, TaskId,
};
use koi_core::ports::{ModelEventStream, ModelProvider};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::llm::{ModelProviderEntry, ModelProviderRegistry};
use crate::model_config::{ModelCatalog, ModelCatalogEntry};

const PROXY_TOKEN_HEADER: &str = "x-koi-model-proxy-token";
const MAX_PROXY_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Agent 侧连接独立模型代理所需的配置。令牌仅是调用能力，不是上游 API Key。
#[derive(Clone, Debug)]
pub struct ModelProxyClientConfig {
    pub base_url: String,
    pub token: String,
    pub request_timeout_secs: u64,
}

/// 代理的公开路由。调用方必须已从安全来源取得非空共享令牌。
pub fn router(registry: Arc<ModelProviderRegistry>, token: String) -> Router {
    Router::new()
        .route("/v1/catalog", get(get_catalog))
        .route("/v1/generate", post(generate))
        .route("/v1/reset", post(reset_task))
        .with_state(Arc::new(ProxyState { registry, token }))
}

#[derive(Clone)]
struct ProxyState {
    registry: Arc<ModelProviderRegistry>,
    token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelProxyStartRequest {
    selection: ModelSelection,
    request: ModelRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelProxyResetRequest {
    selection: ModelSelection,
    task_id: TaskId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ModelProxyStreamRecord {
    Event { event: ModelStreamEvent },
    Error { error: ModelError },
}

async fn get_catalog(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized_response();
    }
    Json(catalog_from_registry(&state.registry)).into_response()
}

async fn generate(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(command): Json<ModelProxyStartRequest>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized_response();
    }
    if let Err(error) = command.request.validate() {
        return model_error_response(ModelError::new(
            ModelErrorKind::InvalidResponse,
            error.to_string(),
            false,
        ));
    }
    let (_, entry) = match state.registry.resolve(Some(&command.selection)) {
        Ok(resolved) => resolved,
        Err(error) => {
            return model_error_response(ModelError::new(
                ModelErrorKind::UnsupportedCapability,
                error.to_string(),
                false,
            ));
        }
    };
    let stream = match entry
        .provider
        .start(command.request, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => return model_error_response(error),
    };

    let body = stream.map(|item| {
        let record = match item {
            Ok(event) => ModelProxyStreamRecord::Event { event },
            Err(error) => ModelProxyStreamRecord::Error { error },
        };
        let encoded = serde_json::to_vec(&record).unwrap_or_else(|error| {
            serde_json::to_vec(&ModelProxyStreamRecord::Error {
                error: ModelError::new(
                    ModelErrorKind::Internal,
                    format!("模型代理序列化流事件失败：{error}"),
                    false,
                ),
            })
            .expect("固定的模型代理错误必须可序列化")
        });
        Ok::<_, Infallible>(Bytes::from([encoded, b"\n".to_vec()].concat()))
    });
    let mut response = Response::new(Body::from_stream(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

async fn reset_task(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(command): Json<ModelProxyResetRequest>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized_response();
    }
    let (_, entry) = match state.registry.resolve(Some(&command.selection)) {
        Ok(resolved) => resolved,
        Err(error) => {
            return model_error_response(ModelError::new(
                ModelErrorKind::UnsupportedCapability,
                error.to_string(),
                false,
            ));
        }
    };
    entry.provider.reset_task(command.task_id);
    StatusCode::NO_CONTENT.into_response()
}

fn authorized(headers: &HeaderMap, expected_token: &str) -> bool {
    headers
        .get(PROXY_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|provided| provided == expected_token)
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ModelError::new(
            ModelErrorKind::Unavailable,
            "模型代理认证失败".to_owned(),
            false,
        )),
    )
        .into_response()
}

fn model_error_response(error: ModelError) -> Response {
    (StatusCode::BAD_GATEWAY, Json(error)).into_response()
}

fn catalog_from_registry(registry: &ModelProviderRegistry) -> ModelCatalog {
    ModelCatalog {
        default_model: registry.default_model().clone(),
        entries: registry
            .entries()
            .map(|(selection, entry)| {
                let descriptor = entry.provider.descriptor();
                ModelCatalogEntry {
                    selection: selection.clone(),
                    protocol: descriptor.protocol,
                    model_options: entry.model_options.clone(),
                    context_window_tokens: entry.context_window_tokens,
                }
            })
            .collect(),
    }
}

/// 从独立模型代理取得公开目录并构造 Agent 侧注册表。
pub async fn build_proxy_model_registry(
    config: ModelProxyClientConfig,
) -> Result<Arc<ModelProviderRegistry>, ModelProxyClientError> {
    let client = Arc::new(ModelProxyClient::new(config)?);
    let catalog = client.catalog().await?;
    let mut registry = ModelProviderRegistry::new(catalog.default_model)
        .map_err(|error| ModelProxyClientError::Protocol(error.to_string()))?;
    for entry in catalog.entries {
        let provider = Arc::new(ModelProxyProvider::new(Arc::clone(&client), entry.clone()));
        registry
            .register(
                entry.selection,
                ModelProviderEntry::new(provider, entry.model_options, entry.context_window_tokens),
            )
            .map_err(|error| ModelProxyClientError::Protocol(error.to_string()))?;
    }
    registry
        .resolve(None)
        .map_err(|error| ModelProxyClientError::Protocol(error.to_string()))?;
    Ok(Arc::new(registry))
}

#[derive(Debug, Error)]
pub enum ModelProxyClientError {
    #[error("模型代理地址无效：{0}")]
    InvalidBaseUrl(String),
    #[error("模型代理令牌不能为空")]
    EmptyToken,
    #[error("模型代理请求超时时间必须大于零")]
    ZeroTimeout,
    #[error("初始化模型代理 HTTP 客户端失败：{0}")]
    Client(String),
    #[error("模型代理通信失败：{0}")]
    Transport(String),
    #[error("模型代理返回无效响应：{0}")]
    Protocol(String),
    #[error("模型代理拒绝请求：{0}")]
    Remote(String),
}

struct ModelProxyClient {
    client: Client,
    base_url: Url,
    token: String,
}

impl ModelProxyClient {
    fn new(config: ModelProxyClientConfig) -> Result<Self, ModelProxyClientError> {
        if config.token.trim().is_empty() {
            return Err(ModelProxyClientError::EmptyToken);
        }
        if config.request_timeout_secs == 0 {
            return Err(ModelProxyClientError::ZeroTimeout);
        }
        let mut base_url = Url::parse(&config.base_url)
            .map_err(|error| ModelProxyClientError::InvalidBaseUrl(error.to_string()))?;
        if !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(ModelProxyClientError::InvalidBaseUrl(
                "只支持不含用户信息、query 或 fragment 的 http(s) 地址".into(),
            ));
        }
        base_url.set_path(&format!("{}/", base_url.path().trim_end_matches('/')));
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|error| ModelProxyClientError::Client(error.to_string()))?;
        Ok(Self {
            client,
            base_url,
            token: config.token,
        })
    }

    async fn catalog(&self) -> Result<ModelCatalog, ModelProxyClientError> {
        let response = self
            .client
            .get(self.endpoint("v1/catalog")?)
            .header(PROXY_TOKEN_HEADER, &self.token)
            .send()
            .await
            .map_err(|error| ModelProxyClientError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(remote_error(response).await);
        }
        response
            .json::<ModelCatalog>()
            .await
            .map_err(|error| ModelProxyClientError::Protocol(error.to_string()))
    }

    async fn start(
        &self,
        selection: ModelSelection,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        request.validate().map_err(|error| {
            ModelError::new(ModelErrorKind::InvalidResponse, error.to_string(), false)
        })?;
        let request_builder = self
            .request(
                self.endpoint("v1/generate")
                    .map_err(proxy_error_to_model_error)?,
            )
            .json(&ModelProxyStartRequest { selection, request });
        let response = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled_error()),
            result = request_builder.send() => result.map_err(|error| ModelError::new(
                ModelErrorKind::Unavailable,
                format!("模型代理通信失败：{error}"),
                true,
            ))?,
        };
        if !response.status().is_success() {
            return Err(remote_model_error(response).await);
        }
        let stream = ProxyResponseStream {
            chunks: response.bytes_stream().boxed(),
            buffer: Vec::new(),
            cancel,
        };
        Ok(Box::pin(stream::unfold(stream, |mut state| async move {
            state.next_item().await.map(|item| (item, state))
        })))
    }

    async fn reset(
        &self,
        selection: ModelSelection,
        task_id: TaskId,
    ) -> Result<(), ModelProxyClientError> {
        let response = self
            .request(self.endpoint("v1/reset")?)
            .json(&ModelProxyResetRequest { selection, task_id })
            .send()
            .await
            .map_err(|error| ModelProxyClientError::Transport(error.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(remote_error(response).await)
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, ModelProxyClientError> {
        self.base_url
            .join(path)
            .map_err(|error| ModelProxyClientError::InvalidBaseUrl(error.to_string()))
    }

    fn request(&self, url: Url) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header(PROXY_TOKEN_HEADER, &self.token)
    }
}

struct ModelProxyProvider {
    client: Arc<ModelProxyClient>,
    entry: ModelCatalogEntry,
    /// `ModelProvider::reset_task` 是同步端口；将重置标记保留到下一次异步调用前，
    /// 可避免后台 HTTP 请求与新的模型调用竞争，导致旧的供应商续接状态被带入新周期。
    reset_pending: Mutex<HashSet<TaskId>>,
}

impl ModelProxyProvider {
    fn new(client: Arc<ModelProxyClient>, entry: ModelCatalogEntry) -> Self {
        Self {
            client,
            entry,
            reset_pending: Mutex::new(HashSet::new()),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for ModelProxyProvider {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: self.entry.selection.provider.clone(),
            model_id: self.entry.selection.model_id.clone(),
            protocol: self.entry.protocol,
            capabilities: ModelCapabilities::new([
                ModelCapability::Streaming,
                ModelCapability::NativeToolCalls,
                ModelCapability::StructuredOutput,
            ]),
        }
    }

    fn context_window_tokens(&self) -> Option<u32> {
        Some(self.entry.context_window_tokens)
    }

    async fn start(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let task_id = request.task_id;
        let reset_pending = self
            .reset_pending
            .lock()
            .map(|mut pending| pending.remove(&task_id))
            .unwrap_or(true);
        if reset_pending {
            self.client
                .reset(self.entry.selection.clone(), task_id)
                .await
                .map_err(proxy_error_to_model_error)?;
        }
        self.client
            .start(self.entry.selection.clone(), request, cancel)
            .await
    }

    fn reset_task(&self, task_id: TaskId) {
        if let Ok(mut pending) = self.reset_pending.lock() {
            pending.insert(task_id);
        }
    }
}

struct ProxyResponseStream {
    chunks: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    buffer: Vec<u8>,
    cancel: CancellationToken,
}

impl ProxyResponseStream {
    async fn next_item(&mut self) -> Option<Result<ModelStreamEvent, ModelError>> {
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let mut line = self.buffer.drain(..=position).collect::<Vec<_>>();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    continue;
                }
                let record =
                    serde_json::from_slice::<ModelProxyStreamRecord>(&line).map_err(|error| {
                        ModelError::new(
                            ModelErrorKind::InvalidResponse,
                            format!("模型代理流记录无法解析：{error}"),
                            false,
                        )
                    });
                return Some(match record {
                    Ok(ModelProxyStreamRecord::Event { event }) => Ok(event),
                    Ok(ModelProxyStreamRecord::Error { error }) => Err(error),
                    Err(error) => Err(error),
                });
            }
            if self.buffer.len() > MAX_PROXY_RECORD_BYTES {
                return Some(Err(ModelError::new(
                    ModelErrorKind::InvalidResponse,
                    "模型代理单条流记录超过限制".to_owned(),
                    false,
                )));
            }
            let next = tokio::select! {
                () = self.cancel.cancelled() => return Some(Err(cancelled_error())),
                next = self.chunks.next() => next,
            };
            match next {
                Some(Ok(bytes)) => self.buffer.extend_from_slice(&bytes),
                Some(Err(error)) => {
                    return Some(Err(ModelError::new(
                        ModelErrorKind::Unavailable,
                        format!("读取模型代理流失败：{error}"),
                        true,
                    )));
                }
                None if self.buffer.is_empty() => return None,
                None => {
                    return Some(Err(ModelError::new(
                        ModelErrorKind::InvalidResponse,
                        "模型代理流结束时留下不完整记录".to_owned(),
                        false,
                    )));
                }
            }
        }
    }
}

async fn remote_error(response: reqwest::Response) -> ModelProxyClientError {
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_default();
    if let Ok(error) = serde_json::from_slice::<ModelError>(&bytes) {
        return ModelProxyClientError::Remote(error.to_string());
    }
    let body = String::from_utf8_lossy(&bytes);
    ModelProxyClientError::Remote(format!("HTTP {status}：{}", body.trim()))
}

async fn remote_model_error(response: reqwest::Response) -> ModelError {
    let status = response.status();
    let bytes = response.bytes().await.unwrap_or_default();
    if let Ok(error) = serde_json::from_slice::<ModelError>(&bytes) {
        return error;
    }
    ModelError::new(
        ModelErrorKind::Unavailable,
        format!("模型代理返回 HTTP {status}"),
        status.is_server_error(),
    )
}

fn proxy_error_to_model_error(error: ModelProxyClientError) -> ModelError {
    ModelError::new(ModelErrorKind::Internal, error.to_string(), false)
}

fn cancelled_error() -> ModelError {
    ModelError::new(
        ModelErrorKind::Cancelled,
        "模型调用已取消".to_owned(),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use koi_core::domain::{
        ModelGenerationOptions, ModelOutput, ModelOutputContract, ModelProtocol, ModelTurn, Usage,
    };

    struct TestProvider;

    #[async_trait::async_trait]
    impl ModelProvider for TestProvider {
        fn descriptor(&self) -> ModelProviderDescriptor {
            ModelProviderDescriptor {
                provider: "example".into(),
                model_id: "model".into(),
                protocol: ModelProtocol::Responses,
                capabilities: ModelCapabilities::new([ModelCapability::Streaming]),
            }
        }

        async fn start(
            &self,
            _request: ModelRequest,
            _cancel: CancellationToken,
        ) -> Result<ModelEventStream, ModelError> {
            Ok(Box::pin(stream::iter([
                Ok(ModelStreamEvent::Delta {
                    sequence: 0,
                    kind: koi_core::domain::ModelDeltaKind::Text,
                    content: "hello ".into(),
                }),
                Ok(ModelStreamEvent::Completed(ModelTurn {
                    outputs: vec![ModelOutput::Text {
                        text: "hello proxy".into(),
                    }],
                    usage: Usage {
                        input_tokens: 2,
                        output_tokens: 2,
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                    },
                    provider_response_id: Some("proxy-test".into()),
                })),
            ])))
        }
    }

    #[test]
    fn catalog_never_contains_provider_credentials() {
        let entry = ModelCatalogEntry {
            selection: ModelSelection::new("example", "model").unwrap(),
            protocol: ModelProtocol::Responses,
            model_options: ModelGenerationOptions::default(),
            context_window_tokens: 1024,
        };
        let encoded = serde_json::to_string(&entry).unwrap();
        assert!(!encoded.contains("api_key"));
    }

    #[tokio::test]
    async fn proxy_round_trips_normalized_model_stream() {
        let selection = ModelSelection::new("example", "model").unwrap();
        let mut local_registry = ModelProviderRegistry::new(selection.clone()).unwrap();
        local_registry
            .register(
                selection.clone(),
                ModelProviderEntry::new(
                    Arc::new(TestProvider),
                    ModelGenerationOptions::default(),
                    4096,
                ),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router(Arc::new(local_registry), "test-token".into()),
            )
            .await;
        });

        let remote_registry = build_proxy_model_registry(ModelProxyClientConfig {
            base_url: format!("http://{address}"),
            token: "test-token".into(),
            request_timeout_secs: 30,
        })
        .await
        .unwrap();
        let request = ModelRequest {
            task_id: TaskId::new(),
            instructions: "test".into(),
            instructions_hash: "test".into(),
            context: Vec::new(),
            tools: Vec::new(),
            output_contract: ModelOutputContract::Text,
            options: ModelGenerationOptions::default(),
        };
        let events = remote_registry
            .default_entry()
            .unwrap()
            .provider
            .start(request, CancellationToken::new())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        server.abort();

        assert!(matches!(
            events.first(),
            Some(Ok(ModelStreamEvent::Delta { content, .. })) if content == "hello "
        ));
        assert!(matches!(
            events.last(),
            Some(Ok(ModelStreamEvent::Completed(ModelTurn { provider_response_id, .. })))
                if provider_response_id.as_deref() == Some("proxy-test")
        ));
    }
}
