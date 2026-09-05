//! OpenAI-compatible model provider.
//!
//! The core crate deliberately knows nothing about a vendor wire format.  This module translates
//! the normalized `koi-core` model contract to the Responses API or Chat Completions API and turns
//! JSON/SSE responses back into the same contract.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use koi_core::domain::{
    EventId, ModelCapabilities, ModelCapability, ModelContextItem, ModelDeltaKind, ModelError,
    ModelErrorKind, ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract,
    ModelProtocol, ModelProviderDescriptor, ModelRequest, ModelSelection,
    ModelSelectionValidationError, ModelStreamEvent, ModelToolDefinition, ModelTurn, TaskId,
    ToolCall, Usage, validate_model_id, validate_model_provider, validate_model_selection,
};
use koi_core::ports::{ModelEventStream, ModelProvider};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode, Url};
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 60;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;
const CHAT_COMPLETIONS_START_MESSAGE: &str = "请根据系统中的任务要求开始处理。";
const AUTHORITY_PARENT_FIELD: &str = "__koi_authority_parent_event_id";
const AUTHORITY_PARENT_DESCRIPTION: &str =
    "授权此调用的 KOI_CONTEXT event_id；无可用授权事件时必须传 null，不能编造。";

/// Configuration for an OpenAI-compatible model endpoint.
///
/// `api_key` is optional so local OpenAI-compatible gateways can be used without an
/// Authorization header. The server loads it from the local TOML runtime configuration;
/// it is never part of the model identity.
#[derive(Clone)]
pub struct OpenAiCompatibleModelConfig {
    pub provider: String,
    pub base_url: String,
    pub model_id: String,
    pub api_key: Option<String>,
    pub protocol: ModelProtocol,
    pub request_timeout_secs: u64,
    /// 由部署配置提供的模型上下文窗口上限；兼容网关通常不会动态返回它。
    pub context_window_tokens: Option<u32>,
}

impl fmt::Debug for OpenAiCompatibleModelConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleModelConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model_id", &self.model_id)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("protocol", &self.protocol)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("context_window_tokens", &self.context_window_tokens)
            .finish()
    }
}

impl OpenAiCompatibleModelConfig {
    /// Creates a Responses API configuration with an optional API key.
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            base_url: base_url.into(),
            model_id: model_id.into(),
            api_key,
            protocol: ModelProtocol::Responses,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            context_window_tokens: None,
        }
    }

    #[must_use]
    pub fn with_protocol(mut self, protocol: ModelProtocol) -> Self {
        self.protocol = protocol;
        self
    }

    #[must_use]
    pub fn with_request_timeout_secs(mut self, seconds: u64) -> Self {
        self.request_timeout_secs = seconds;
        self
    }

    #[must_use]
    pub fn with_context_window_tokens(mut self, tokens: u32) -> Self {
        self.context_window_tokens = Some(tokens);
        self
    }

    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    fn validate(&self) -> Result<Url, ModelProviderConfigError> {
        validate_model_provider(&self.provider)
            .map_err(|error| ModelProviderConfigError::InvalidProvider(error.to_string()))?;
        if self.model_id.trim().is_empty() {
            return Err(ModelProviderConfigError::EmptyModel);
        }
        validate_model_id(&self.model_id)
            .map_err(|error| ModelProviderConfigError::InvalidModelId(error.to_string()))?;
        if self.request_timeout_secs == 0 {
            return Err(ModelProviderConfigError::ZeroRequestTimeout);
        }
        if self.context_window_tokens == Some(0) {
            return Err(ModelProviderConfigError::ZeroContextWindow);
        }
        if self
            .api_key
            .as_deref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err(ModelProviderConfigError::EmptyApiKey);
        }

        let mut url = Url::parse(&self.base_url)
            .map_err(|error| ModelProviderConfigError::InvalidBaseUrl(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ModelProviderConfigError::InvalidBaseUrl(
                "只支持 http 和 https 协议".into(),
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ModelProviderConfigError::InvalidBaseUrl(
                "模型地址不能包含 URL 用户信息".into(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ModelProviderConfigError::InvalidBaseUrl(
                "模型地址不能包含 query 或 fragment".into(),
            ));
        }

        let normalized_path = if url.path().is_empty() {
            "/".to_owned()
        } else {
            format!("{}/", url.path().trim_end_matches('/'))
        };
        url.set_path(&normalized_path);
        Ok(url)
    }
}

/// Errors found before a model request can be sent.
#[derive(Debug, Error)]
pub enum ModelProviderConfigError {
    #[error("模型供应商无效：{0}")]
    InvalidProvider(String),
    #[error("模型 ID 不能为空")]
    EmptyModel,
    #[error("模型 ID 无效：{0}")]
    InvalidModelId(String),
    #[error("模型请求超时时间必须大于零")]
    ZeroRequestTimeout,
    #[error("模型上下文窗口必须大于零")]
    ZeroContextWindow,
    #[error("模型 API key 不能为空")]
    EmptyApiKey,
    #[error("模型 base URL 无效：{0}")]
    InvalidBaseUrl(String),
    #[error("模型 HTTP 客户端初始化失败：{0}")]
    ClientBuild(String),
}

/// An OpenAI-compatible Responses or Chat Completions provider.
pub struct OpenAiCompatibleModelProvider {
    client: Client,
    endpoint: Url,
    config: OpenAiCompatibleModelConfig,
    conversations: Arc<Mutex<HashMap<TaskId, ConversationState>>>,
}

/// Short aliases kept for callers that refer to the adapter as a model or an LLM.
pub type OpenAiCompatibleModel = OpenAiCompatibleModelProvider;
pub type LlmConfig = OpenAiCompatibleModelConfig;
pub type ModelProviderConfig = OpenAiCompatibleModelConfig;

impl OpenAiCompatibleModelProvider {
    /// Builds a provider with a reqwest client configured with the model request timeout.
    ///
    /// # Errors
    ///
    /// Returns an error if the base URL or model configuration is invalid, or if reqwest cannot
    /// build the client.
    pub fn new(config: OpenAiCompatibleModelConfig) -> Result<Self, ModelProviderConfigError> {
        let endpoint = endpoint_for(&config)?;
        let client = Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(|error| ModelProviderConfigError::ClientBuild(error.to_string()))?;
        Ok(Self::with_client_and_endpoint(client, config, endpoint))
    }

    /// Builds a provider around a caller-supplied reqwest client.
    ///
    /// This is useful when the application has its own proxy, TLS, or connection-pool settings.
    /// The supplied client remains responsible for its own timeout policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider configuration is invalid.
    pub fn with_client(
        client: Client,
        config: OpenAiCompatibleModelConfig,
    ) -> Result<Self, ModelProviderConfigError> {
        let endpoint = endpoint_for(&config)?;
        Ok(Self::with_client_and_endpoint(client, config, endpoint))
    }

    fn with_client_and_endpoint(
        client: Client,
        config: OpenAiCompatibleModelConfig,
        endpoint: Url,
    ) -> Self {
        Self {
            client,
            endpoint,
            config,
            conversations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[must_use]
    pub fn config(&self) -> &OpenAiCompatibleModelConfig {
        &self.config
    }

    /// Drops provider-local tool continuation state for a task.
    pub fn reset_task(&self, task_id: TaskId) {
        if let Ok(mut conversations) = self.conversations.lock() {
            conversations.remove(&task_id);
        }
    }

    fn request_body(&self, request: &ModelRequest) -> Result<Value, ModelError> {
        let pending_tool_calls = self.conversation_snapshot(request.task_id)?;
        Ok(match self.config.protocol {
            ModelProtocol::Responses => {
                build_responses_request(&self.config.model_id, request, &pending_tool_calls)
            }
            ModelProtocol::ChatCompletions => {
                build_chat_request(&self.config.model_id, request, &pending_tool_calls)
            }
        })
    }

    fn conversation_snapshot(&self, task_id: TaskId) -> Result<Vec<PendingToolCall>, ModelError> {
        let conversations = self
            .conversations
            .lock()
            .map_err(|_| internal_error("模型会话状态锁已中毒"))?;
        Ok(conversations
            .get(&task_id)
            .map(|state| state.pending_tool_calls.clone())
            .unwrap_or_default())
    }

    fn remember_turn(&self, task_id: TaskId, turn: &ModelTurn) -> Result<(), ModelError> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| internal_error("模型会话状态锁已中毒"))?;
        remember_tool_calls(&mut conversations, task_id, turn);
        Ok(())
    }
}

#[async_trait::async_trait]
impl ModelProvider for OpenAiCompatibleModelProvider {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: self.config.provider.clone(),
            model_id: self.config.model_id.clone(),
            protocol: self.config.protocol,
            capabilities: ModelCapabilities::new([
                ModelCapability::Streaming,
                ModelCapability::NativeToolCalls,
                ModelCapability::StructuredOutput,
            ]),
        }
    }

    fn context_window_tokens(&self) -> Option<u32> {
        self.config.context_window_tokens
    }

    async fn start(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        request
            .validate()
            .map_err(|error| invalid_response(error.to_string()))?;
        let body = self.request_body(&request)?;
        let mut request_builder = self
            .client
            .post(self.endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(
                ACCEPT,
                if request.options.stream {
                    "text/event-stream, application/json"
                } else {
                    "application/json"
                },
            )
            .json(&body);
        if let Some(api_key) = &self.config.api_key {
            request_builder = request_builder.header(AUTHORIZATION, format!("Bearer {api_key}"));
        }

        let response = tokio::select! {
            () = cancel.cancelled() => return Err(cancelled_error()),
            result = request_builder.send() => result.map_err(map_request_error)?,
        };
        let status = response.status();
        if !status.is_success() {
            return Err(read_http_error(response, cancel).await);
        }

        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_sse = content_type.contains("text/event-stream")
            || (request.options.stream && !content_type.contains("json"));

        if is_sse {
            let body = response.bytes_stream().boxed();
            let state = ModelResponseStream::new(
                body,
                self.config.protocol,
                request.task_id,
                cancel,
                Arc::clone(&self.conversations),
            );
            return Ok(Box::pin(stream::unfold(state, |mut state| async move {
                state.next_item().await.map(|item| (item, state))
            })));
        }

        let bytes = read_response_body(response, cancel.clone()).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| invalid_response(format!("响应 JSON 无法解析：{error}")))?;
        let turn = parse_turn(self.config.protocol, &value)?;
        self.remember_turn(request.task_id, &turn)?;
        Ok(Box::pin(stream::once(async move {
            Ok(ModelStreamEvent::Completed(turn))
        })))
    }

    fn reset_task(&self, task_id: TaskId) {
        OpenAiCompatibleModelProvider::reset_task(self, task_id);
    }
}

/// Runtime settings associated with one configured model selection.
#[derive(Clone)]
pub struct ModelProviderEntry {
    pub provider: Arc<dyn ModelProvider>,
    pub model_options: ModelGenerationOptions,
    /// 配置中的模型上下文窗口上限，供应用层展示和调度策略使用。
    pub context_window_tokens: u32,
}

impl ModelProviderEntry {
    #[must_use]
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        model_options: ModelGenerationOptions,
        context_window_tokens: u32,
    ) -> Self {
        Self {
            provider,
            model_options,
            context_window_tokens: context_window_tokens.max(1),
        }
    }
}

/// Registry of the models available to the running server.
///
/// A registry key is the provider/model pair supplied by the vendor. No application-owned alias
/// is introduced, so identical vendor model IDs can coexist when they come from different
/// providers.
pub struct ModelProviderRegistry {
    default_model: ModelSelection,
    providers: BTreeMap<ModelSelection, ModelProviderEntry>,
}

#[derive(Debug, Error)]
pub enum ModelRegistryError {
    #[error("供应商模型标识无效：{0}")]
    InvalidModelSelection(#[from] ModelSelectionValidationError),
    #[error("供应商模型已注册：{0}")]
    DuplicateModel(ModelSelection),
    #[error("供应商模型未配置：{0}")]
    UnknownModel(ModelSelection),
    #[error("默认供应商模型未配置：{0}")]
    MissingDefaultModel(ModelSelection),
    #[error("Provider 描述与配置标识不一致：配置为 {configured}，实际为 {actual}")]
    ProviderIdentityMismatch {
        configured: ModelSelection,
        actual: ModelSelection,
    },
}

impl ModelProviderRegistry {
    /// Creates an empty registry with the given default selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the default provider/model pair is invalid.
    pub fn new(default_model: ModelSelection) -> Result<Self, ModelRegistryError> {
        validate_model_selection(&default_model.provider, &default_model.model_id)
            .map_err(ModelRegistryError::InvalidModelSelection)?;
        Ok(Self {
            default_model,
            providers: BTreeMap::new(),
        })
    }

    /// Adds a configured model under its provider/model identity.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate IDs.
    pub fn register(
        &mut self,
        selection: ModelSelection,
        entry: ModelProviderEntry,
    ) -> Result<(), ModelRegistryError> {
        let selection = checked_model_selection(selection)?;
        let descriptor = entry.provider.descriptor();
        let actual = ModelSelection::new(descriptor.provider, descriptor.model_id)
            .map_err(ModelRegistryError::InvalidModelSelection)?;
        if actual != selection {
            return Err(ModelRegistryError::ProviderIdentityMismatch {
                configured: selection,
                actual,
            });
        }
        if self.providers.contains_key(&selection) {
            return Err(ModelRegistryError::DuplicateModel(selection));
        }
        self.providers.insert(selection, entry);
        Ok(())
    }

    /// Resolves an explicit task selection, or the configured default when it is absent.
    ///
    /// # Errors
    ///
    /// Returns an error when the selection is invalid, unknown, or the default was not registered.
    pub fn resolve(
        &self,
        selected_model: Option<&ModelSelection>,
    ) -> Result<(&ModelSelection, &ModelProviderEntry), ModelRegistryError> {
        let selection = selected_model.unwrap_or(&self.default_model);
        validate_model_selection(&selection.provider, &selection.model_id)
            .map_err(ModelRegistryError::InvalidModelSelection)?;
        self.providers
            .get_key_value(selection)
            .ok_or_else(|| match selected_model {
                Some(_) => ModelRegistryError::UnknownModel(selection.clone()),
                None => ModelRegistryError::MissingDefaultModel(selection.clone()),
            })
    }

    #[must_use]
    pub fn default_model(&self) -> &ModelSelection {
        &self.default_model
    }

    #[must_use]
    pub fn default_entry(&self) -> Option<&ModelProviderEntry> {
        self.providers.get(&self.default_model)
    }

    #[must_use]
    pub fn contains(&self, selection: &ModelSelection) -> bool {
        self.providers.contains_key(selection)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.providers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }

    pub fn model_selections(&self) -> impl Iterator<Item = &ModelSelection> {
        self.providers.keys()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&ModelSelection, &ModelProviderEntry)> {
        self.providers.iter()
    }

    /// Resets continuation state in every provider for one task.
    pub fn reset_task(&self, task_id: TaskId) {
        for entry in self.providers.values() {
            entry.provider.reset_task(task_id);
        }
    }
}

fn checked_model_selection(
    selection: ModelSelection,
) -> Result<ModelSelection, ModelRegistryError> {
    validate_model_selection(&selection.provider, &selection.model_id)
        .map_err(ModelRegistryError::InvalidModelSelection)?;
    Ok(selection)
}

fn endpoint_for(config: &OpenAiCompatibleModelConfig) -> Result<Url, ModelProviderConfigError> {
    let url = config.validate()?;
    let endpoint = match config.protocol {
        ModelProtocol::Responses => "responses",
        ModelProtocol::ChatCompletions => "chat/completions",
    };
    url.join(endpoint)
        .map_err(|error| ModelProviderConfigError::InvalidBaseUrl(error.to_string()))
}

fn build_responses_request(
    model: &str,
    request: &ModelRequest,
    pending_tool_calls: &[PendingToolCall],
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.into()));
    body.insert(
        "instructions".into(),
        Value::String(request.instructions.clone()),
    );
    body.insert(
        "input".into(),
        Value::Array(responses_input(&request.context, pending_tool_calls)),
    );
    body.insert(
        "tools".into(),
        Value::Array(request.tools.iter().map(responses_tool).collect::<Vec<_>>()),
    );
    apply_common_request_options(&mut body, &request.options, ModelProtocol::Responses);
    if let ModelOutputContract::JsonSchema {
        name,
        schema,
        strict,
    } = &request.output_contract
    {
        body.insert(
            "text".into(),
            json!({
                "format": {
                    "type": "json_schema",
                    "name": name,
                    "schema": schema,
                    "strict": strict,
                }
            }),
        );
    }
    Value::Object(body)
}

fn build_chat_request(
    model: &str,
    request: &ModelRequest,
    pending_tool_calls: &[PendingToolCall],
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.into()));
    body.insert(
        "messages".into(),
        Value::Array(chat_messages(
            &request.instructions,
            &request.context,
            pending_tool_calls,
        )),
    );
    body.insert(
        "tools".into(),
        Value::Array(request.tools.iter().map(chat_tool).collect::<Vec<_>>()),
    );
    apply_common_request_options(&mut body, &request.options, ModelProtocol::ChatCompletions);
    if request.options.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !matches!(request.output_contract, ModelOutputContract::Text) {
        body.insert(
            "response_format".into(),
            chat_output_contract(&request.output_contract),
        );
    }
    Value::Object(body)
}

fn apply_common_request_options(
    body: &mut Map<String, Value>,
    options: &ModelGenerationOptions,
    protocol: ModelProtocol,
) {
    if let Some(max_output_tokens) = options.max_output_tokens {
        let field = match protocol {
            ModelProtocol::Responses => "max_output_tokens",
            ModelProtocol::ChatCompletions => "max_completion_tokens",
        };
        body.insert(field.into(), Value::Number(max_output_tokens.into()));
    }
    if let Some(temperature) = options.temperature {
        if let Some(value) = serde_json::Number::from_f64(f64::from(temperature)) {
            body.insert("temperature".into(), Value::Number(value));
        }
    }
    if let Some(reasoning_effort) = options
        .reasoning_effort
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        match protocol {
            ModelProtocol::Responses => {
                body.insert("reasoning".into(), json!({"effort": reasoning_effort}));
            }
            ModelProtocol::ChatCompletions => {
                body.insert(
                    "reasoning_effort".into(),
                    Value::String(reasoning_effort.into()),
                );
            }
        }
    }
    body.insert("stream".into(), Value::Bool(options.stream));
    body.insert(
        "parallel_tool_calls".into(),
        Value::Bool(options.allow_parallel_tool_calls),
    );
}

fn responses_input(
    context: &[ModelContextItem],
    pending_tool_calls: &[PendingToolCall],
) -> Vec<Value> {
    let mut input = Vec::with_capacity(context.len() + pending_tool_calls.len());
    let mut tool_index = 0;
    let mut inserted_pending_calls = false;
    for item in context {
        if item.role == ModelInputRole::Tool {
            if !inserted_pending_calls && !pending_tool_calls.is_empty() {
                input.extend(pending_tool_calls.iter().map(responses_function_call));
                inserted_pending_calls = true;
            }
            if let Some(call) = pending_tool_calls.get(tool_index) {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call.id,
                    "output": item.content,
                }));
            } else {
                input.push(responses_message("user", "input_text", &item.content));
            }
            tool_index += 1;
        } else {
            input.push(responses_message(
                responses_role(item.role),
                responses_content_type(item.role),
                &model_context_content(item),
            ));
        }
    }
    input
}

fn chat_messages(
    instructions: &str,
    context: &[ModelContextItem],
    pending_tool_calls: &[PendingToolCall],
) -> Vec<Value> {
    // 部分 OpenAI 兼容服务（学校接口也属于这一类）不接受多个连续的 system
    // 消息，甚至不接受 developer role。核心中的系统事件仍然要保留其语义，
    // 因此在 Chat Completions 协议下将它们合并到唯一的首条 system 消息中。
    // Responses 协议不经过这里，仍可保留独立的 system/developer input item。
    let system_context = context
        .iter()
        .filter(|item| {
            matches!(
                item.role,
                ModelInputRole::System | ModelInputRole::Developer
            )
        })
        .map(model_context_content)
        .collect::<Vec<_>>();
    let system_message = if system_context.is_empty() {
        instructions.to_owned()
    } else {
        std::iter::once(instructions)
            .chain(system_context.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let mut messages = Vec::with_capacity(context.len() + pending_tool_calls.len() + 1);
    messages.push(json!({"role": "system", "content": system_message}));
    let mut tool_index = 0;
    let mut inserted_pending_calls = false;
    let mut has_user_message = false;
    for item in context {
        if matches!(
            item.role,
            ModelInputRole::System | ModelInputRole::Developer
        ) {
            continue;
        }
        if item.role == ModelInputRole::Tool {
            if !inserted_pending_calls && !pending_tool_calls.is_empty() {
                messages.push(chat_assistant_tool_call(pending_tool_calls));
                inserted_pending_calls = true;
            }
            if let Some(call) = pending_tool_calls.get(tool_index) {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": item.content,
                }));
            } else {
                // After a process restart there is no provider call ID in the core context. A
                // user message keeps the result visible without fabricating an invalid tool link.
                messages.push(json!({
                    "role": "user",
                    "content": format!("工具执行结果（仅供分析）：{}", item.content),
                }));
            }
            tool_index += 1;
        } else {
            has_user_message |= chat_role(item.role) == "user";
            messages.push(json!({
                "role": chat_role(item.role),
                "content": model_context_content(item),
            }));
        }
    }
    // 学校的 Chat Completions 兼容接口要求 messages 中至少有一条 user 消息。
    // 子任务刚启动时上下文可能只有一条系统创建指令；这时添加固定的线协议占位消息，
    // 不伪造事件、不携带权限，也不向模型提供可作为授权证据的 event_id。
    if !has_user_message {
        messages.push(json!({
            "role": "user",
            "content": CHAT_COMPLETIONS_START_MESSAGE,
        }));
    }
    messages
}

fn responses_message(role: &str, content_type: &str, content: &str) -> Value {
    json!({
        "role": role,
        "content": [{"type": content_type, "text": content}],
    })
}

fn responses_function_call(call: &PendingToolCall) -> Value {
    json!({
        "type": "function_call",
        "call_id": call.id,
        "name": call.name,
        "arguments": call.arguments,
    })
}

fn chat_assistant_tool_call(calls: &[PendingToolCall]) -> Value {
    json!({
        "role": "assistant",
        "content": Value::Null,
        "tool_calls": calls.iter().map(|call| json!({
            "id": call.id,
            "type": "function",
            "function": {
                "name": call.name,
                "arguments": call.arguments,
            },
        })).collect::<Vec<_>>(),
    })
}

fn remember_tool_calls(
    conversations: &mut HashMap<TaskId, ConversationState>,
    task_id: TaskId,
    turn: &ModelTurn,
) {
    let tool_calls = turn
        .outputs
        .iter()
        .filter_map(|output| match output {
            ModelOutput::ToolCall(call) => Some(PendingToolCall {
                id: call.provider_call_id.clone()?,
                name: call.name.clone(),
                arguments: serde_json::to_string(&call.arguments).ok()?,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if tool_calls.is_empty() {
        // 工具链已经结束；不能把上一轮的 provider call ID 带到下一次独立对话中。
        conversations.remove(&task_id);
    } else {
        // 一次模型响应对应一组新的待执行工具调用。替换而不是追加，避免跨轮次重复
        // 配对历史工具结果。
        conversations.entry(task_id).or_default().pending_tool_calls = tool_calls;
    }
}

fn responses_role(role: ModelInputRole) -> &'static str {
    match role {
        ModelInputRole::System => "system",
        ModelInputRole::Developer => "developer",
        ModelInputRole::User | ModelInputRole::Memory | ModelInputRole::Tool => "user",
        ModelInputRole::Assistant => "assistant",
    }
}

fn responses_content_type(role: ModelInputRole) -> &'static str {
    match role {
        ModelInputRole::Assistant => "output_text",
        _ => "input_text",
    }
}

fn chat_role(role: ModelInputRole) -> &'static str {
    match role {
        ModelInputRole::System => "system",
        ModelInputRole::Developer => "developer",
        ModelInputRole::User | ModelInputRole::Memory | ModelInputRole::Tool => "user",
        ModelInputRole::Assistant => "assistant",
    }
}

/// 为模型渲染上下文。所有持久化上下文都公开稳定事件 ID，方便模型定位更早的事实；
/// 只有用户/告警类、且本身具有授权能力的输入才使用可作为授权依据的 `KOI_CONTEXT`。
/// `KOI_HISTORY` 仅用于标识历史资料，绝不能作为授权父事件。
///
/// `event_id` 不是从消息正文提取的用户数据，而是核心绑定到上下文项的持久化事件标识。
fn model_context_content(item: &ModelContextItem) -> String {
    if item.role == ModelInputRole::User && item.permission.can_authorize() {
        format!(
            "[KOI_CONTEXT event_id={} permission={:?}]\n{}",
            item.event_id, item.permission, item.content
        )
    } else {
        format!(
            "[KOI_HISTORY event_id={} role={:?}]\n{}",
            item.event_id, item.role, item.content
        )
    }
}

fn responses_tool(tool: &ModelToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": model_tool_schema(tool),
        "strict": tool.strict,
    })
}

fn chat_tool(tool: &ModelToolDefinition) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": model_tool_schema(tool),
            "strict": tool.strict,
        },
    })
}

/// 扩展每个模型可见工具的 wire schema，使严格 schema 模式下模型也能显式选择授权证据。
/// 核心解析响应时会移除这个保留字段，因此实际工具实现不会收到它。
fn model_tool_schema(tool: &ModelToolDefinition) -> Value {
    let mut schema = tool.input_schema.clone();
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    let properties = root
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(properties) = properties.as_object_mut() else {
        return schema;
    };
    properties.insert(
        AUTHORITY_PARENT_FIELD.into(),
        json!({
            "type": ["string", "null"],
            "description": AUTHORITY_PARENT_DESCRIPTION,
        }),
    );
    let required = root
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(required) = required.as_array_mut() {
        if !required
            .iter()
            .any(|field| field.as_str() == Some(AUTHORITY_PARENT_FIELD))
        {
            required.push(Value::String(AUTHORITY_PARENT_FIELD.into()));
        }
    }
    schema
}

fn chat_output_contract(contract: &ModelOutputContract) -> Value {
    match contract {
        ModelOutputContract::Text => json!({"type": "text"}),
        ModelOutputContract::JsonSchema {
            name,
            schema,
            strict,
        } => json!({
            "type": "json_schema",
            "json_schema": {
                "name": name,
                "schema": schema,
                "strict": strict,
            },
        }),
    }
}

#[derive(Clone, Debug)]
struct PendingToolCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Debug, Default)]
struct ConversationState {
    pending_tool_calls: Vec<PendingToolCall>,
}

type ResponseBodyStream = BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>;

struct ModelResponseStream {
    body: ResponseBodyStream,
    decoder: SseDecoder,
    protocol: ModelProtocol,
    task_id: TaskId,
    cancel: CancellationToken,
    conversations: Arc<Mutex<HashMap<TaskId, ConversationState>>>,
    queue: VecDeque<Result<ModelStreamEvent, ModelError>>,
    accumulator: StreamAccumulator,
    body_finished: bool,
    decoder_finished: bool,
    done: bool,
}

impl ModelResponseStream {
    fn new(
        body: ResponseBodyStream,
        protocol: ModelProtocol,
        task_id: TaskId,
        cancel: CancellationToken,
        conversations: Arc<Mutex<HashMap<TaskId, ConversationState>>>,
    ) -> Self {
        Self {
            body,
            decoder: SseDecoder::default(),
            protocol,
            task_id,
            cancel,
            conversations,
            queue: VecDeque::new(),
            accumulator: StreamAccumulator::new(protocol),
            body_finished: false,
            decoder_finished: false,
            done: false,
        }
    }

    async fn next_item(&mut self) -> Option<Result<ModelStreamEvent, ModelError>> {
        if let Some(item) = self.queue.pop_front() {
            return Some(item);
        }
        if self.done {
            return None;
        }

        loop {
            if let Some(message) = self.decoder.next_message() {
                match message {
                    Ok(message) => self.handle_message(&message),
                    Err(error) => self.fail(error),
                }
                if let Some(item) = self.queue.pop_front() {
                    return Some(item);
                }
                if self.done {
                    return None;
                }
                continue;
            }

            if self.body_finished {
                if !self.decoder_finished {
                    self.decoder_finished = true;
                    match self.decoder.finish() {
                        Ok(Some(message)) => self.handle_message(&message),
                        Ok(None) => {}
                        Err(error) => self.fail(error),
                    }
                    if let Some(item) = self.queue.pop_front() {
                        return Some(item);
                    }
                    if self.done {
                        return None;
                    }
                }
                self.fail(invalid_response("模型 SSE 在完成事件前结束"));
                return self.queue.pop_front();
            }

            let chunk = tokio::select! {
                () = self.cancel.cancelled() => {
                    self.fail(cancelled_error());
                    return self.queue.pop_front();
                },
                chunk = self.body.next() => chunk,
            };
            match chunk {
                Some(Ok(bytes)) => {
                    if let Err(error) = self.decoder.push(&bytes) {
                        self.fail(error);
                    }
                }
                Some(Err(error)) => {
                    self.fail(map_request_error(error));
                }
                None => self.body_finished = true,
            }
            if let Some(item) = self.queue.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
        }
    }

    fn handle_message(&mut self, message: &SseMessage) {
        if message.data.trim() == "[DONE]" {
            match self.accumulator.finish() {
                Ok(turn) => self.complete(turn),
                Err(error) => self.fail(error),
            }
            return;
        }
        if message.data.trim().is_empty() {
            return;
        }
        let value: Value = match serde_json::from_str(&message.data) {
            Ok(value) => value,
            Err(error) => {
                self.fail(invalid_response(format!("SSE 数据 JSON 无法解析：{error}")));
                return;
            }
        };
        match self.protocol {
            ModelProtocol::Responses => {
                self.handle_responses_event(&value, message.event.as_deref());
            }
            ModelProtocol::ChatCompletions => {
                self.handle_chat_event(&value, message.event.as_deref());
            }
        }
    }

    fn handle_responses_event(&mut self, value: &Value, event_name: Option<&str>) {
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .or(event_name)
            .unwrap_or_default();
        match event_type {
            "response.created" | "response.in_progress" | "response.output_item.added" => {
                self.accumulator.capture_response_metadata(value);
                if event_type == "response.output_item.added" {
                    if let Some(item) = value.get("item") {
                        self.accumulator
                            .capture_responses_item(item, output_index(value));
                        if let Some(name) = item.get("name").and_then(Value::as_str) {
                            self.delta(ModelDeltaKind::ToolName, name.to_owned());
                        }
                    }
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.accumulator.capture_text(delta);
                    self.delta(ModelDeltaKind::Text, delta.to_owned());
                }
            }
            "response.output_text.done" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    if self.accumulator.capture_completed_text(text) {
                        self.delta(ModelDeltaKind::Text, text.to_owned());
                    }
                }
            }
            "response.refusal.delta" => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.accumulator.capture_refusal(delta);
                    self.delta(ModelDeltaKind::Status, delta.to_owned());
                }
            }
            "response.refusal.done" => {
                if let Some(refusal) = value.get("refusal").and_then(Value::as_str) {
                    if self.accumulator.capture_completed_refusal(refusal) {
                        self.delta(ModelDeltaKind::Status, refusal.to_owned());
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = output_index(value).unwrap_or_default();
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.accumulator.capture_tool_arguments(index, delta);
                    self.delta(ModelDeltaKind::ToolArguments, delta.to_owned());
                }
            }
            "response.function_call_arguments.done" => {
                let index = output_index(value).unwrap_or_default();
                self.accumulator.capture_responses_tool_done(index, value);
            }
            "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    self.accumulator
                        .capture_responses_item(item, output_index(value));
                }
            }
            "response.completed" => {
                self.accumulator.capture_response_metadata(value);
                let response = value.get("response").unwrap_or(value);
                let result = match &self.accumulator {
                    StreamAccumulator::Responses(accumulator) => {
                        parse_responses_turn(response, Some(accumulator))
                    }
                    StreamAccumulator::Chat(_) => {
                        Err(invalid_response("Responses 流状态与 Provider 协议不一致"))
                    }
                };
                match result {
                    Ok(turn) => self.complete(turn),
                    Err(error) => self.fail(error),
                }
            }
            "response.failed" => {
                self.fail(provider_event_error(
                    value,
                    ModelErrorKind::Unavailable,
                    true,
                ));
            }
            "response.incomplete" => {
                self.fail(provider_event_error(
                    value,
                    ModelErrorKind::InvalidResponse,
                    false,
                ));
            }
            "error" => self.fail(provider_event_error(value, ModelErrorKind::Internal, false)),
            _ => {}
        }
    }

    fn handle_chat_event(&mut self, value: &Value, _event_name: Option<&str>) {
        self.accumulator.capture_chat_metadata(value);
        if let Some(usage) = value.get("usage").and_then(parse_usage) {
            self.accumulator.capture_usage(usage);
        }
        let Some(choices) = value.get("choices").and_then(Value::as_array) else {
            return;
        };
        for choice in choices {
            let choice_index = choice
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or_default();
            let delta = choice.get("delta").or_else(|| choice.get("message"));
            let Some(delta) = delta else {
                continue;
            };
            if let Some(content) = text_content(delta.get("content")) {
                self.accumulator.capture_choice_text(choice_index, &content);
                self.delta(ModelDeltaKind::Text, content);
            }
            if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
                self.accumulator
                    .capture_choice_refusal(choice_index, refusal);
                self.delta(ModelDeltaKind::Status, refusal.to_owned());
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let index = tool_call
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or_default();
                    let function = tool_call.get("function").unwrap_or(tool_call);
                    if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                        self.accumulator
                            .capture_chat_tool_id(choice_index, index, id);
                    }
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        self.accumulator
                            .capture_chat_tool_name(choice_index, index, name);
                        self.delta(ModelDeltaKind::ToolName, name.to_owned());
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        self.accumulator.capture_chat_tool_arguments(
                            choice_index,
                            index,
                            arguments,
                        );
                        self.delta(ModelDeltaKind::ToolArguments, arguments.to_owned());
                    }
                }
            }
            if let Some(function_call) = delta.get("function_call") {
                if let Some(name) = function_call.get("name").and_then(Value::as_str) {
                    self.accumulator
                        .capture_chat_tool_name(choice_index, 0, name);
                    self.delta(ModelDeltaKind::ToolName, name.to_owned());
                }
                if let Some(arguments) = function_call.get("arguments").and_then(Value::as_str) {
                    self.accumulator
                        .capture_chat_tool_arguments(choice_index, 0, arguments);
                    self.delta(ModelDeltaKind::ToolArguments, arguments.to_owned());
                }
            }
        }
    }

    fn delta(&mut self, kind: ModelDeltaKind, content: String) {
        if content.is_empty() {
            return;
        }
        let sequence = self.accumulator.next_sequence();
        self.queue.push_back(Ok(ModelStreamEvent::Delta {
            sequence,
            kind,
            content,
        }));
    }

    fn complete(&mut self, turn: ModelTurn) {
        if self.done {
            return;
        }
        if let Ok(mut conversations) = self.conversations.lock() {
            remember_tool_calls(&mut conversations, self.task_id, &turn);
        }
        self.done = true;
        self.queue.push_back(Ok(ModelStreamEvent::Completed(turn)));
    }

    fn fail(&mut self, error: ModelError) {
        if !self.done {
            self.done = true;
            self.queue.push_back(Err(error));
        }
    }
}

enum StreamAccumulator {
    Responses(ResponsesAccumulator),
    Chat(ChatAccumulator),
}

impl StreamAccumulator {
    fn new(protocol: ModelProtocol) -> Self {
        match protocol {
            ModelProtocol::Responses => Self::Responses(ResponsesAccumulator::default()),
            ModelProtocol::ChatCompletions => Self::Chat(ChatAccumulator::default()),
        }
    }

    fn next_sequence(&mut self) -> u32 {
        match self {
            Self::Responses(accumulator) => accumulator.next_sequence(),
            Self::Chat(accumulator) => accumulator.next_sequence(),
        }
    }

    fn capture_response_metadata(&mut self, value: &Value) {
        if let Self::Responses(accumulator) = self {
            accumulator.capture_metadata(value);
        }
    }

    fn capture_chat_metadata(&mut self, value: &Value) {
        if let Self::Chat(accumulator) = self {
            accumulator.capture_metadata(value);
        }
    }

    fn capture_text(&mut self, text: &str) {
        if let Self::Responses(accumulator) = self {
            accumulator.text.push_str(text);
        }
    }

    fn capture_completed_text(&mut self, text: &str) -> bool {
        if let Self::Responses(accumulator) = self {
            if accumulator.text.is_empty() {
                accumulator.text.push_str(text);
                return true;
            }
        }
        false
    }

    fn capture_refusal(&mut self, text: &str) {
        if let Self::Responses(accumulator) = self {
            accumulator.refusal.push_str(text);
        }
    }

    fn capture_completed_refusal(&mut self, text: &str) -> bool {
        if let Self::Responses(accumulator) = self {
            if accumulator.refusal.is_empty() {
                accumulator.refusal.push_str(text);
                return true;
            }
        }
        false
    }

    fn capture_tool_arguments(&mut self, index: usize, arguments: &str) {
        if let Self::Responses(accumulator) = self {
            accumulator
                .tools
                .entry(index)
                .or_default()
                .arguments
                .push_str(arguments);
        }
    }

    fn capture_responses_tool_done(&mut self, index: usize, value: &Value) {
        if let Self::Responses(accumulator) = self {
            let tool = accumulator.tools.entry(index).or_default();
            if let Some(name) = value.get("name").and_then(Value::as_str) {
                name.clone_into(&mut tool.name);
            }
            if let Some(arguments) = value.get("arguments").and_then(Value::as_str) {
                arguments.clone_into(&mut tool.arguments);
            }
            if let Some(id) = value.get("call_id").and_then(Value::as_str) {
                tool.id = Some(id.to_owned());
            } else if let Some(id) = value.get("item_id").and_then(Value::as_str) {
                tool.id = Some(id.to_owned());
            }
        }
    }

    fn capture_responses_item(&mut self, item: &Value, event_index: Option<usize>) {
        if let Self::Responses(accumulator) = self {
            let Some(item_type) = item.get("type").and_then(Value::as_str) else {
                return;
            };
            if item_type != "function_call" {
                return;
            }
            let index = event_index
                .or_else(|| {
                    item.get("output_index")
                        .and_then(Value::as_u64)
                        .and_then(|value| usize::try_from(value).ok())
                })
                .unwrap_or(accumulator.tools.len());
            let tool = accumulator.tools.entry(index).or_default();
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                name.clone_into(&mut tool.name);
            }
            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                arguments.clone_into(&mut tool.arguments);
            }
            tool.id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
    }

    fn capture_choice_text(&mut self, choice: usize, content: &str) {
        if let Self::Chat(accumulator) = self {
            accumulator
                .choices
                .entry(choice)
                .or_default()
                .text
                .push_str(content);
        }
    }

    fn capture_choice_refusal(&mut self, choice: usize, refusal: &str) {
        if let Self::Chat(accumulator) = self {
            accumulator
                .choices
                .entry(choice)
                .or_default()
                .refusal
                .push_str(refusal);
        }
    }

    fn capture_chat_tool_id(&mut self, choice: usize, index: usize, id: &str) {
        if let Self::Chat(accumulator) = self {
            accumulator
                .choices
                .entry(choice)
                .or_default()
                .tools
                .entry(index)
                .or_default()
                .id = Some(id.to_owned());
        }
    }

    fn capture_chat_tool_name(&mut self, choice: usize, index: usize, name: &str) {
        if let Self::Chat(accumulator) = self {
            accumulator
                .choices
                .entry(choice)
                .or_default()
                .tools
                .entry(index)
                .or_default()
                .name
                .push_str(name);
        }
    }

    fn capture_chat_tool_arguments(&mut self, choice: usize, index: usize, arguments: &str) {
        if let Self::Chat(accumulator) = self {
            accumulator
                .choices
                .entry(choice)
                .or_default()
                .tools
                .entry(index)
                .or_default()
                .arguments
                .push_str(arguments);
        }
    }

    fn capture_usage(&mut self, usage: Usage) {
        match self {
            Self::Responses(accumulator) => accumulator.usage = Some(usage),
            Self::Chat(accumulator) => accumulator.usage = Some(usage),
        }
    }

    fn finish(&self) -> Result<ModelTurn, ModelError> {
        match self {
            Self::Responses(accumulator) => accumulator.finish(),
            Self::Chat(accumulator) => accumulator.finish(),
        }
    }
}

#[derive(Default)]
struct ResponsesAccumulator {
    response_id: Option<String>,
    text: String,
    refusal: String,
    tools: BTreeMap<usize, ToolAccumulator>,
    usage: Option<Usage>,
    sequence: u32,
}

impl ResponsesAccumulator {
    fn next_sequence(&mut self) -> u32 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }

    fn capture_metadata(&mut self, value: &Value) {
        let response = value.get("response").unwrap_or(value);
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
        if let Some(usage) = response.get("usage").and_then(parse_usage) {
            self.usage = Some(usage);
        }
    }

    fn finish(&self) -> Result<ModelTurn, ModelError> {
        let mut outputs = Vec::new();
        if !self.text.is_empty() {
            outputs.push(ModelOutput::Text {
                text: self.text.clone(),
            });
        }
        if !self.refusal.is_empty() {
            outputs.push(ModelOutput::Refusal {
                reason: self.refusal.clone(),
            });
        }
        for tool in self.tools.values() {
            outputs.push(tool.to_model_output()?);
        }
        if outputs.is_empty() {
            return Err(invalid_response("模型响应没有文本、拒答或工具调用输出"));
        }
        Ok(ModelTurn {
            outputs,
            usage: self.usage.clone().unwrap_or_else(empty_usage),
            provider_response_id: self.response_id.clone(),
        })
    }
}

#[derive(Default)]
struct ChatAccumulator {
    response_id: Option<String>,
    choices: BTreeMap<usize, ChatChoiceAccumulator>,
    usage: Option<Usage>,
    sequence: u32,
}

impl ChatAccumulator {
    fn next_sequence(&mut self) -> u32 {
        let sequence = self.sequence;
        self.sequence = self.sequence.saturating_add(1);
        sequence
    }

    fn capture_metadata(&mut self, value: &Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.response_id = Some(id.to_owned());
        }
    }

    fn finish(&self) -> Result<ModelTurn, ModelError> {
        let mut outputs = Vec::new();
        for choice in self.choices.values() {
            if !choice.text.is_empty() {
                outputs.push(ModelOutput::Text {
                    text: choice.text.clone(),
                });
            }
            if !choice.refusal.is_empty() {
                outputs.push(ModelOutput::Refusal {
                    reason: choice.refusal.clone(),
                });
            }
            for tool in choice.tools.values() {
                outputs.push(tool.to_model_output()?);
            }
        }
        if outputs.is_empty() {
            return Err(invalid_response("模型响应没有文本、拒答或工具调用输出"));
        }
        Ok(ModelTurn {
            outputs,
            usage: self.usage.clone().unwrap_or_else(empty_usage),
            provider_response_id: self.response_id.clone(),
        })
    }
}

#[derive(Default)]
struct ChatChoiceAccumulator {
    text: String,
    refusal: String,
    tools: BTreeMap<usize, ToolAccumulator>,
}

#[derive(Default)]
struct ToolAccumulator {
    id: Option<String>,
    name: String,
    arguments: String,
}

impl ToolAccumulator {
    fn to_model_output(&self) -> Result<ModelOutput, ModelError> {
        if self.name.trim().is_empty() {
            return Err(invalid_response("工具调用缺少名称"));
        }
        let (arguments, authority_parent_event_id) = parse_tool_arguments(&self.arguments)?;
        Ok(ModelOutput::ToolCall(ToolCall {
            name: self.name.clone(),
            arguments,
            provider_call_id: self.id.clone(),
            authority_parent_event_id,
        }))
    }
}

fn parse_turn(protocol: ModelProtocol, value: &Value) -> Result<ModelTurn, ModelError> {
    match protocol {
        ModelProtocol::Responses => {
            let response = value.get("response").unwrap_or(value);
            parse_responses_turn(response, None)
        }
        ModelProtocol::ChatCompletions => parse_chat_turn(value),
    }
}

fn parse_responses_turn(
    response: &Value,
    fallback: Option<&ResponsesAccumulator>,
) -> Result<ModelTurn, ModelError> {
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| fallback.and_then(|value| value.response_id.clone()));
    let usage = response
        .get("usage")
        .and_then(parse_usage)
        .or_else(|| fallback.and_then(|value| value.usage.clone()))
        .unwrap_or_else(empty_usage);
    let mut outputs = Vec::new();
    if let Some(items) = response.get("output").and_then(Value::as_array) {
        for item in items {
            parse_responses_item(item, &mut outputs)?;
        }
    }
    if outputs.is_empty() {
        if let Some(text) = response.get("output_text").and_then(Value::as_str) {
            if !text.is_empty() {
                outputs.push(ModelOutput::Text { text: text.into() });
            }
        }
    }
    if outputs.is_empty() {
        if let Some(fallback) = fallback {
            return fallback.finish();
        }
    }
    if outputs.is_empty() {
        return Err(invalid_response("Responses 响应没有可识别的输出"));
    }
    Ok(ModelTurn {
        outputs,
        usage,
        provider_response_id: response_id,
    })
}

fn parse_responses_item(item: &Value, outputs: &mut Vec<ModelOutput>) -> Result<(), ModelError> {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message" => {
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    match part.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "output_text" => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    outputs.push(ModelOutput::Text { text: text.into() });
                                }
                            }
                        }
                        "refusal" => {
                            if let Some(reason) = part.get("refusal").and_then(Value::as_str) {
                                if !reason.is_empty() {
                                    outputs.push(ModelOutput::Refusal {
                                        reason: reason.into(),
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            } else if let Some(text) = item.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    outputs.push(ModelOutput::Text { text: text.into() });
                }
            }
        }
        "function_call" => {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_response("Responses 工具调用缺少名称"))?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (arguments, authority_parent_event_id) = parse_tool_arguments(arguments)?;
            outputs.push(ModelOutput::ToolCall(ToolCall {
                name: name.into(),
                arguments,
                provider_call_id: item
                    .get("call_id")
                    .or_else(|| item.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                authority_parent_event_id,
            }));
        }
        // Reasoning and provider-hosted tools are not part of koi-core's normalized output. They
        // are ignored, while unknown output types remain forward-compatible.
        _ => {}
    }
    Ok(())
}

fn parse_chat_turn(value: &Value) -> Result<ModelTurn, ModelError> {
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_response("Chat Completions 响应缺少 choices"))?;
    let mut outputs = Vec::new();
    for choice in choices {
        let message = choice
            .get("message")
            .or_else(|| choice.get("delta"))
            .ok_or_else(|| invalid_response("Chat Completions choice 缺少 message"))?;
        if let Some(content) = text_content(message.get("content")) {
            if !content.is_empty() {
                outputs.push(ModelOutput::Text { text: content });
            }
        }
        if let Some(refusal) = message.get("refusal").and_then(Value::as_str) {
            if !refusal.is_empty() {
                outputs.push(ModelOutput::Refusal {
                    reason: refusal.into(),
                });
            }
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let function = tool_call.get("function").unwrap_or(tool_call);
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_response("Chat 工具调用缺少名称"))?;
                let arguments = function
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (arguments, authority_parent_event_id) = parse_tool_arguments(arguments)?;
                outputs.push(ModelOutput::ToolCall(ToolCall {
                    name: name.into(),
                    arguments,
                    provider_call_id: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    authority_parent_event_id,
                }));
            }
        }
        if let Some(function_call) = message.get("function_call") {
            let name = function_call
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_response("Chat function_call 缺少名称"))?;
            let arguments = function_call
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (arguments, authority_parent_event_id) = parse_tool_arguments(arguments)?;
            outputs.push(ModelOutput::ToolCall(ToolCall {
                name: name.into(),
                arguments,
                provider_call_id: None,
                authority_parent_event_id,
            }));
        }
    }
    if outputs.is_empty() {
        return Err(invalid_response("Chat Completions 响应没有可识别的输出"));
    }
    Ok(ModelTurn {
        outputs,
        usage: value
            .get("usage")
            .and_then(parse_usage)
            .unwrap_or_else(empty_usage),
        provider_response_id: value.get("id").and_then(Value::as_str).map(str::to_owned),
    })
}

fn parse_tool_arguments(raw: &str) -> Result<(Value, Option<EventId>), ModelError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|error| invalid_response(format!("工具参数不是有效 JSON：{error}")))?;
    let Value::Object(mut object) = value else {
        return Err(invalid_response("工具参数必须是 JSON 对象"));
    };
    let authority_parent_event_id =
        match object.remove(AUTHORITY_PARENT_FIELD) {
            None | Some(Value::Null) => None,
            Some(Value::String(raw)) => {
                let raw = raw.trim();
                // 一些 Chat Completions 兼容模型会把 schema 中的 null 错误编码为字符串
                // "null"、"none" 或 "nil"。这些值都表示“本次没有可用授权证据”，不应
                // 让整个模型调用失败；它们不会产生任何权限，因为核心仍会对 None 做拒绝。
                if raw.is_empty()
                    || raw.eq_ignore_ascii_case("null")
                    || raw.eq_ignore_ascii_case("none")
                    || raw.eq_ignore_ascii_case("nil")
                {
                    None
                } else {
                    Some(uuid::Uuid::parse_str(raw).map(EventId).map_err(|error| {
                        invalid_response(format!("授权父事件 ID 无效：{error}"))
                    })?)
                }
            }
            Some(value) => {
                return Err(invalid_response(format!(
                    "授权父事件 ID 必须是字符串或 null，实际为 {value}"
                )));
            }
        };
    Ok((Value::Object(object), authority_parent_event_id))
}

fn parse_usage(value: &Value) -> Option<Usage> {
    if !value.is_object() {
        return None;
    }
    let input_tokens = value
        .get("input_tokens")
        .or_else(|| value.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let output_tokens = value
        .get("output_tokens")
        .or_else(|| value.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let cached_input_tokens = value
        .get("input_tokens_details")
        .or_else(|| value.get("prompt_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    let reasoning_tokens = value
        .get("output_tokens_details")
        .or_else(|| value.get("completion_tokens_details"))
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    Some(Usage {
        input_tokens,
        output_tokens,
        cached_input_tokens,
        reasoning_tokens,
    })
}

fn text_content(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for part in parts {
                if let Some(value) = part.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some(text)
        }
        _ => None,
    }
}

fn output_index(value: &Value) -> Option<usize> {
    value
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn empty_usage() -> Usage {
    Usage {
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: None,
        reasoning_tokens: None,
    }
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
}

struct SseMessage {
    event: Option<String>,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        if self.buffer.len().saturating_add(bytes.len()) > MAX_SSE_EVENT_BYTES {
            return Err(invalid_response("单个 SSE 事件超过大小限制"));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn next_message(&mut self) -> Option<Result<SseMessage, ModelError>> {
        loop {
            let newline = self.buffer.iter().position(|byte| *byte == b'\n')?;
            let mut line = self.buffer.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            match self.process_line(&line) {
                Ok(Some(message)) => return Some(Ok(message)),
                Ok(None) => {}
                Err(error) => return Some(Err(error)),
            }
        }
    }

    fn finish(&mut self) -> Result<Option<SseMessage>, ModelError> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if let Some(message) = self.process_line(line)? {
                return Ok(Some(message));
            }
        }
        Ok(self.take_message())
    }

    fn process_line(&mut self, line: &[u8]) -> Result<Option<SseMessage>, ModelError> {
        if line.is_empty() {
            return Ok(self.take_message());
        }
        if line[0] == b':' {
            return Ok(None);
        }
        let colon = line.iter().position(|byte| *byte == b':');
        let (field, raw_value) = colon.map_or((line, &[][..]), |colon| {
            let value = &line[colon + 1..];
            (&line[..colon], value.strip_prefix(b" ").unwrap_or(value))
        });
        match field {
            b"event" => {
                self.event = Some(decode_sse_text(raw_value)?);
            }
            b"data" => {
                let value = decode_sse_text(raw_value)?;
                if self
                    .data
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(1)
                    > MAX_SSE_EVENT_BYTES
                {
                    return Err(invalid_response("单个 SSE 事件超过大小限制"));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(&value);
            }
            _ => {}
        }
        Ok(None)
    }

    fn take_message(&mut self) -> Option<SseMessage> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        Some(SseMessage {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        })
    }
}

fn decode_sse_text(bytes: &[u8]) -> Result<String, ModelError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|error| invalid_response(format!("SSE 文本不是有效 UTF-8：{error}")))
}

async fn read_response_body(
    response: reqwest::Response,
    cancel: CancellationToken,
) -> Result<Vec<u8>, ModelError> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::select! {
        () = cancel.cancelled() => return Err(cancelled_error()),
        chunk = body.next() => chunk,
    } {
        let chunk = chunk.map_err(map_request_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(invalid_response("模型响应超过大小限制"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_http_error(response: reqwest::Response, cancel: CancellationToken) -> ModelError {
    let status = response.status();
    let body = match read_limited_body(response, cancel).await {
        Ok(body) => body,
        Err(error) => return error,
    };
    let detail = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            String::from_utf8(body)
                .ok()
                .map(|body| truncate_message(&body, MAX_ERROR_BODY_BYTES))
        })
        .filter(|body| !body.trim().is_empty())
        .unwrap_or_else(|| status.to_string());
    let (kind, retryable) = match status {
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
            (ModelErrorKind::Timeout, true)
        }
        StatusCode::TOO_MANY_REQUESTS => (ModelErrorKind::RateLimited, true),
        status if status.is_server_error() => (ModelErrorKind::Unavailable, true),
        _ => (ModelErrorKind::InvalidResponse, false),
    };
    ModelError::new(kind, format!("HTTP {status}: {detail}"), retryable)
}

async fn read_limited_body(
    response: reqwest::Response,
    cancel: CancellationToken,
) -> Result<Vec<u8>, ModelError> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = tokio::select! {
        () = cancel.cancelled() => return Err(cancelled_error()),
        chunk = body.next() => chunk,
    } {
        let chunk = chunk.map_err(map_request_error)?;
        if bytes.len().saturating_add(chunk.len()) > MAX_ERROR_BODY_BYTES {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn provider_event_error(value: &Value, kind: ModelErrorKind, retryable: bool) -> ModelError {
    let response = value.get("response").unwrap_or(value);
    let message = response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or("模型供应商返回未说明的错误");
    ModelError::new(
        kind,
        truncate_message(message, MAX_ERROR_BODY_BYTES),
        retryable,
    )
}

#[allow(clippy::needless_pass_by_value)]
fn map_request_error(error: reqwest::Error) -> ModelError {
    if error.is_timeout() {
        ModelError::new(ModelErrorKind::Timeout, "模型请求超时", true)
    } else if error.is_connect() {
        ModelError::new(
            ModelErrorKind::Unavailable,
            format!("无法连接模型服务：{error}"),
            true,
        )
    } else {
        ModelError::new(ModelErrorKind::Internal, error.to_string(), false)
    }
}

fn cancelled_error() -> ModelError {
    ModelError::new(ModelErrorKind::Cancelled, "模型调用已取消", false)
}

fn invalid_response(message: impl Into<String>) -> ModelError {
    ModelError::new(ModelErrorKind::InvalidResponse, message, false)
}

fn internal_error(message: impl Into<String>) -> ModelError {
    ModelError::new(ModelErrorKind::Internal, message, false)
}

fn truncate_message(message: &str, max_bytes: usize) -> String {
    if message.len() <= max_bytes {
        return message.into();
    }
    let mut end = max_bytes;
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &message[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use koi_core::domain::{ModelGenerationOptions, PermissionLevel};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn request(_protocol: ModelProtocol, stream: bool) -> ModelRequest {
        ModelRequest {
            task_id: TaskId::new(),
            instructions: "你是运维助手。".into(),
            instructions_hash: "test-hash".into(),
            context: vec![ModelContextItem {
                event_id: EventId::new(),
                role: ModelInputRole::User,
                content: "检查服务状态".into(),
                permission: PermissionLevel::User,
            }],
            tools: vec![ModelToolDefinition {
                name: "service_status".into(),
                description: "查询服务状态".into(),
                input_schema: json!({"type": "object", "properties": {}}),
                strict: true,
            }],
            output_contract: ModelOutputContract::Text,
            options: ModelGenerationOptions {
                stream,
                ..ModelGenerationOptions::default()
            },
        }
    }

    #[test]
    fn model_registry_resolves_default_and_explicit_models() {
        let openai = Arc::new(
            OpenAiCompatibleModelProvider::new(OpenAiCompatibleModelConfig::new(
                "openai",
                "http://127.0.0.1:1/v1",
                "test-model",
                None,
            ))
            .unwrap(),
        );
        let deepseek = Arc::new(
            OpenAiCompatibleModelProvider::new(OpenAiCompatibleModelConfig::new(
                "deepseek",
                "http://127.0.0.1:1/v1",
                "test-model",
                None,
            ))
            .unwrap(),
        );
        let openai_selection = ModelSelection::new("openai", "test-model").unwrap();
        let deepseek_selection = ModelSelection::new("deepseek", "test-model").unwrap();
        let mut registry = ModelProviderRegistry::new(deepseek_selection.clone()).unwrap();
        registry
            .register(
                openai_selection.clone(),
                ModelProviderEntry::new(
                    Arc::clone(&openai) as Arc<dyn ModelProvider>,
                    ModelGenerationOptions::default(),
                    32,
                ),
            )
            .unwrap();
        registry
            .register(
                deepseek_selection.clone(),
                ModelProviderEntry::new(
                    deepseek,
                    ModelGenerationOptions {
                        reasoning_effort: Some("low".into()),
                        ..ModelGenerationOptions::default()
                    },
                    16,
                ),
            )
            .unwrap();

        let (default_id, default_entry) = registry.resolve(None).unwrap();
        assert_eq!(default_id, &deepseek_selection);
        assert_eq!(default_entry.context_window_tokens, 16);
        let (selected_id, selected_entry) = registry.resolve(Some(&openai_selection)).unwrap();
        assert_eq!(selected_id, &openai_selection);
        assert_eq!(selected_entry.context_window_tokens, 32);
        assert_eq!(
            registry.model_selections().collect::<Vec<_>>(),
            [&deepseek_selection, &openai_selection]
        );
    }

    #[test]
    fn model_registry_rejects_unknown_and_duplicate_models() {
        let provider = Arc::new(
            OpenAiCompatibleModelProvider::new(OpenAiCompatibleModelConfig::new(
                "test-provider",
                "http://127.0.0.1:1/v1",
                "test-model",
                None,
            ))
            .unwrap(),
        ) as Arc<dyn ModelProvider>;
        let selection = ModelSelection::new("test-provider", "test-model").unwrap();
        let missing = ModelSelection::new("missing", "missing-model").unwrap();
        let mut registry = ModelProviderRegistry::new(selection.clone()).unwrap();
        let entry = ModelProviderEntry::new(provider, ModelGenerationOptions::default(), 1);
        registry.register(selection.clone(), entry.clone()).unwrap();
        assert!(matches!(
            registry.register(selection, entry),
            Err(ModelRegistryError::DuplicateModel(_))
        ));
        assert!(matches!(
            registry.resolve(Some(&missing)),
            Err(ModelRegistryError::UnknownModel(_))
        ));
    }

    #[test]
    fn model_receives_only_eligible_context_ids_and_reserved_tool_field() {
        let mut request = request(ModelProtocol::Responses, false);
        let eligible_id = request.context[0].event_id;
        request.context.push(ModelContextItem {
            event_id: EventId::new(),
            role: ModelInputRole::Tool,
            content: "工具结果".into(),
            permission: PermissionLevel::None,
        });

        let responses = responses_input(&request.context, &[]);
        let response_content = responses[0]["content"][0]["text"].as_str().unwrap();
        assert!(response_content.contains(&format!("KOI_CONTEXT event_id={eligible_id}")));
        assert!(!responses[1].to_string().contains("KOI_CONTEXT"));

        let chat = chat_messages(&request.instructions, &request.context, &[]);
        let chat_content = chat[1]["content"].as_str().unwrap();
        assert!(chat_content.contains(&format!("KOI_CONTEXT event_id={eligible_id}")));

        let history_id = EventId::new();
        let history = ModelContextItem {
            event_id: history_id,
            role: ModelInputRole::Memory,
            content: "更早的历史事实".into(),
            permission: PermissionLevel::None,
        };
        let rendered_history = model_context_content(&history);
        assert!(rendered_history.contains(&format!("KOI_HISTORY event_id={history_id}")));
        assert!(rendered_history.contains("更早的历史事实"));

        for schema in [
            &responses_tool(&request.tools[0])["parameters"],
            &chat_tool(&request.tools[0])["function"]["parameters"],
        ] {
            assert_eq!(
                schema["properties"][AUTHORITY_PARENT_FIELD]["type"],
                json!(["string", "null"])
            );
            assert!(
                schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|field| field.as_str() == Some(AUTHORITY_PARENT_FIELD))
            );
        }
    }

    #[test]
    fn chat_messages_merge_system_context_into_the_single_system_message() {
        let mut request = request(ModelProtocol::ChatCompletions, false);
        request.context.insert(
            0,
            ModelContextItem {
                event_id: EventId::new(),
                role: ModelInputRole::System,
                content: "子任务的系统创建指令".into(),
                permission: PermissionLevel::System,
            },
        );

        let messages = chat_messages(&request.instructions, &request.context, &[]);
        let system_messages = messages
            .iter()
            .filter(|message| message["role"] == "system")
            .collect::<Vec<_>>();
        assert_eq!(system_messages.len(), 1);
        let system_content = system_messages[0]["content"].as_str().unwrap();
        assert!(system_content.contains(&request.instructions));
        assert!(system_content.contains("子任务的系统创建指令"));
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn chat_messages_add_a_user_turn_for_a_system_only_child_task() {
        let request = ModelRequest {
            context: vec![ModelContextItem {
                event_id: EventId::new(),
                role: ModelInputRole::System,
                content: "子任务的系统创建指令".into(),
                permission: PermissionLevel::System,
            }],
            ..request(ModelProtocol::ChatCompletions, false)
        };

        let messages = chat_messages(&request.instructions, &request.context, &[]);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("子任务的系统创建指令")
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], CHAT_COMPLETIONS_START_MESSAGE);
    }

    #[test]
    fn parse_tool_arguments_accepts_string_encoded_null_authority() {
        for value in ["null", "none", "NIL", ""] {
            let raw = format!("{{\"service\":\"demo\",\"{AUTHORITY_PARENT_FIELD}\":\"{value}\"}}");
            let (arguments, authority) = parse_tool_arguments(&raw).unwrap();
            assert_eq!(arguments["service"], "demo");
            assert_eq!(authority, None);
        }
    }

    #[test]
    fn parse_tool_arguments_rejects_non_null_invalid_authority() {
        let raw = format!("{{\"{AUTHORITY_PARENT_FIELD}\":\"not-an-event-id\"}}");
        let error = parse_tool_arguments(&raw).unwrap_err();
        assert!(error.message.contains("授权父事件 ID 无效"));
    }

    async fn test_server(response: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 32 * 1024];
            let _ = socket.read(&mut request).await;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}/v1"), handle)
    }

    #[test]
    fn config_redacts_api_key_and_normalizes_endpoint() {
        let config = OpenAiCompatibleModelConfig::new(
            "test-provider",
            "http://127.0.0.1:1234/v1/",
            "test-model",
            Some("secret".into()),
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("secret"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(config.validate().unwrap().path(), "/v1/");
    }

    #[tokio::test]
    async fn responses_stream_is_normalized_with_text_and_tool_call() {
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: text/event-stream\r\n",
            "Connection: close\r\n\r\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"正在分析\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc-1\",\"call_id\":\"call-1\",\"name\":\"service_status\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"service\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"\\\"koi-demo.service\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"正在分析\"}]},{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"service_status\",\"arguments\":\"{\\\"service\\\":\\\"koi-demo.service\\\"}\"}],\"usage\":{\"input_tokens\":12,\"output_tokens\":8,\"output_tokens_details\":{\"reasoning_tokens\":2}}}}\n\n",
        );
        let (base_url, handle) = test_server(response.into()).await;
        let config =
            OpenAiCompatibleModelConfig::new("test-provider", base_url, "test-model", None);
        // 测试服务绑定在回环地址，不能受开发机 HTTP(S) 代理环境变量影响。
        let provider = OpenAiCompatibleModelProvider::with_client(
            Client::builder().no_proxy().build().unwrap(),
            config,
        )
        .unwrap();
        let events = provider
            .start(
                request(ModelProtocol::Responses, true),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        handle.await.unwrap();

        assert!(events.iter().any(|event| matches!(
            event,
            Ok(ModelStreamEvent::Delta {
                kind: ModelDeltaKind::Text,
                ..
            })
        )));
        let Some(Ok(ModelStreamEvent::Completed(turn))) = events.last() else {
            panic!("expected completed event: {events:?}");
        };
        assert_eq!(turn.provider_response_id.as_deref(), Some("resp-1"));
        assert_eq!(turn.usage.input_tokens, 12);
        assert_eq!(turn.usage.reasoning_tokens, Some(2));
        assert!(matches!(
            &turn.outputs[1],
            ModelOutput::ToolCall(ToolCall {
                name,
                provider_call_id: Some(call_id),
                ..
            }) if name == "service_status" && call_id == "call-1"
        ));
    }

    #[tokio::test]
    async fn chat_json_response_is_normalized_and_request_uses_chat_shape() {
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Type: application/json\r\n",
            "Connection: close\r\n\r\n",
            r#"{"id":"chat-1","choices":[{"index":0,"message":{"role":"assistant","content":"服务正常","tool_calls":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}"#,
        );
        let (base_url, handle) = test_server(response.into()).await;
        let config =
            OpenAiCompatibleModelConfig::new("test-provider", base_url, "test-model", None)
                .with_protocol(ModelProtocol::ChatCompletions);
        // 测试服务绑定在回环地址，不能受开发机 HTTP(S) 代理环境变量影响。
        let provider = OpenAiCompatibleModelProvider::with_client(
            Client::builder().no_proxy().build().unwrap(),
            config,
        )
        .unwrap();
        let events = provider
            .start(
                request(ModelProtocol::ChatCompletions, false),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        handle.await.unwrap();

        let Some(Ok(ModelStreamEvent::Completed(turn))) = events.first() else {
            panic!("expected completed event: {events:?}");
        };
        assert_eq!(turn.provider_response_id.as_deref(), Some("chat-1"));
        assert_eq!(turn.usage.cached_input_tokens, Some(2));
        assert!(matches!(&turn.outputs[0], ModelOutput::Text { text } if text == "服务正常"));
    }
}
