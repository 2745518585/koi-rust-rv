use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use koi_api::{WebApi, WebAuth};
use koi_core::agent::DEFAULT_CONTEXT_WINDOW_TOKENS;
use koi_core::domain::{EventSource, ModelGenerationOptions, ModelProtocol, ModelSelection};
use koi_core::ports::{EventStore, SourceAuthorizationRegistry, ToolRegistry};
use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::{
    ModelProviderEntry, ModelProviderRegistry, OpenAiCompatibleModelConfig,
    OpenAiCompatibleModelProvider,
};
use koi_infra::tools::ToolPolicy;
use koi_infra::web_identity::WebUserStore;
use koi_infra::web_source::KoiWebSource;
use serde::Deserialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

mod agent_runtime;
mod prompts;

#[derive(Debug, Deserialize)]
struct RuntimeConfig {
    server: ServerConfig,
    #[serde(default)]
    security: ToolPolicy,
    models: ModelsConfig,
    #[serde(default)]
    agent: AgentConfig,
    #[serde(default)]
    usage: UsageConfig,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelConfig {
    provider: String,
    base_url: String,
    model_id: String,
    api_key: Option<String>,
    protocol: String,
    request_timeout_secs: u64,
    /// 模型上下文窗口上限；兼容旧配置时可由 `max_context_messages` 推导。
    #[serde(default)]
    context_window_tokens: Option<u32>,
    /// 旧版按消息数量限制上下文的配置，仅用于迁移，不再直接控制上下文。
    #[serde(default)]
    max_context_messages: Option<usize>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
}

/// Configured provider/model pairs. The pair is the model identity; there is no application alias.
#[derive(Clone, Debug, Deserialize)]
struct ModelsConfig {
    default_provider: String,
    default_model_id: String,
    entries: Vec<ModelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct AgentConfig {
    max_steps: u16,
    max_concurrent_tasks: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_concurrent_tasks: 4,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ServerConfig {
    bind_addr: String,
    web_dist_dir: PathBuf,
    event_store_dir: PathBuf,
    user_store_path: PathBuf,
    web_cookie_secure: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct UsageConfig {
    monthly_budget_usd: f64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            monthly_budget_usd: 10.0,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    if let Err(error) = run().await {
        tracing::error!(%error, "koi-server 启动失败");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)]
async fn run() -> Result<(), ServerError> {
    let config = load_runtime_config()?;
    let prompts = prompts::ServerPromptProvider;
    let model_registry = build_model_registry(&config)?;

    let mut registry = ToolRegistry::default();
    let registered = koi_infra::tools::register_builtin_tools(&mut registry, config.security)
        .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;
    let task_tools = koi_core::agent::task_tools::register_task_management_tools(&mut registry)
        .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;
    let tools = Arc::new(registry);
    let tool_definitions = tools.list_definitions();

    let store = Arc::new(
        JsonlEventStore::open(&config.server.event_store_dir)
            .map_err(|error| ServerError::EventStore(error.to_string()))?,
    );
    // 主会话是唯一的跨任务管理入口：启动时初始化其事件流（TaskCreated/TaskQueued），
    // 之后 Web 与 `task.*` 工具共享同一个任务管理器。
    bootstrap_main_session(&store)
        .await
        .map_err(ServerError::EventStore)?;
    let task_manager = Arc::new(koi_core::agent::TaskManager::new(Arc::new(Arc::clone(
        &store,
    ))));
    let identities =
        Arc::new(WebUserStore::open(&config.server.user_store_path).map_err(ServerError::WebApi)?);
    let source = Arc::new(
        KoiWebSource::new(
            Arc::clone(&store),
            Arc::clone(&identities),
            Arc::clone(&task_manager),
            tool_definitions,
            config.usage.monthly_budget_usd,
        )
        .map_err(ServerError::WebApi)?
        .with_model_catalog(
            model_registry.model_selections().cloned(),
            model_registry.default_model().clone(),
        ),
    );
    let auth = WebAuth::new(identities, config.server.web_cookie_secure);
    let mut authorization_providers = SourceAuthorizationRegistry::default();
    authorization_providers
        .register(source.authorization_provider())
        .map_err(|error| ServerError::AuthorizationProvider(error.to_string()))?;
    let authorization_providers = Arc::new(authorization_providers);

    // Web 命令会直接发布自己的输入事件；后台 Agent 产生的模型、工具和系统事件通过
    // 事件存储订阅器转发，保证刷新页面或重连 SSE 后仍可从存储恢复完整历史。
    let mut stored_events = store.subscribe();
    let event_sink = Arc::clone(&source);
    tokio::spawn(async move {
        loop {
            match stored_events.recv().await {
                Ok(event) if !matches!(event.provenance.creator, EventSource::External(_)) => {
                    event_sink.publish_event(&event).await;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let supervisor = agent_runtime::AgentSupervisor::new(
        Arc::clone(&store),
        Arc::clone(&model_registry),
        tools,
        authorization_providers,
        Arc::new(prompts),
        task_manager,
        config.agent.max_steps,
        config.agent.max_concurrent_tasks,
    );
    let shutdown = CancellationToken::new();
    let supervisor_task = tokio::spawn(Arc::clone(&supervisor).run(shutdown.clone()));

    let api: Arc<dyn WebApi> = source;
    let api_router = koi_api::router(api, auth).layer(TraceLayer::new_for_http());

    let app = if config.server.web_dist_dir.is_dir() {
        tracing::info!(path = %config.server.web_dist_dir.display(), "已启用 Web 静态文件托管");
        api_router.fallback_service(
            ServeDir::new(&config.server.web_dist_dir).append_index_html_on_directories(true),
        )
    } else {
        tracing::warn!(path = %config.server.web_dist_dir.display(), "未找到 Web 构建目录，仅提供 API");
        api_router
    };

    let listener = tokio::net::TcpListener::bind(&config.server.bind_addr)
        .await
        .map_err(ServerError::Bind)?;
    tracing::info!(
        app = koi_core::APP_NAME,
        api_crate = koi_api::CRATE_NAME,
        tool_count = registered + task_tools,
        bind_addr = %config.server.bind_addr,
        event_store = %config.server.event_store_dir.display(),
        "koi-server 已启动"
    );
    let result = tokio::select! {
        result = axum::serve(listener, app) => result.map_err(ServerError::Serve),
        result = tokio::signal::ctrl_c() => result.map_err(ServerError::Signal),
    };
    shutdown.cancel();
    let _ = supervisor_task.await;
    result
}

fn parse_model_protocol(raw: &str) -> Result<ModelProtocol, ServerError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "responses" => Ok(ModelProtocol::Responses),
        "chat_completions" | "chat-completions" | "chat" => Ok(ModelProtocol::ChatCompletions),
        other => Err(ServerError::Configuration(format!(
            "不支持的模型协议：{other}，可选 responses 或 chat_completions"
        ))),
    }
}

fn build_model_registry(config: &RuntimeConfig) -> Result<Arc<ModelProviderRegistry>, ServerError> {
    if config.models.entries.is_empty() {
        return Err(ServerError::Configuration(
            "[models] 至少需要配置一个模型条目".into(),
        ));
    }
    let default_model = ModelSelection::new(
        config.models.default_provider.clone(),
        config.models.default_model_id.clone(),
    )
    .map_err(|error| ServerError::Configuration(format!("默认模型无效：{error}")))?;

    let mut registry = ModelProviderRegistry::new(default_model)
        .map_err(|error| ServerError::Configuration(error.to_string()))?;
    for model in config.models.entries.clone() {
        let selection = ModelSelection::new(model.provider.clone(), model.model_id.clone())
            .map_err(|error| ServerError::Configuration(format!("模型条目无效：{error}")))?;
        let protocol = parse_model_protocol(&model.protocol)?;
        let context_window_tokens = configured_context_window_tokens(&model)?;
        let api_key = model.api_key.filter(|value| !value.trim().is_empty());
        let provider_config = OpenAiCompatibleModelConfig::new(
            model.provider.clone(),
            model.base_url,
            model.model_id.clone(),
            api_key,
        )
        .with_protocol(protocol)
        .with_request_timeout_secs(model.request_timeout_secs)
        .with_context_window_tokens(context_window_tokens);
        let provider = Arc::new(
            OpenAiCompatibleModelProvider::new(provider_config)
                .map_err(|error| ServerError::ModelProvider(format!("{selection}：{error}")))?,
        );
        let model_options = ModelGenerationOptions {
            max_output_tokens: model.max_output_tokens,
            reasoning_effort: model
                .reasoning_effort
                .filter(|effort| !effort.trim().is_empty()),
            ..ModelGenerationOptions::default()
        };
        registry
            .register(
                selection,
                ModelProviderEntry::new(provider, model_options, context_window_tokens),
            )
            .map_err(|error| ServerError::Configuration(error.to_string()))?;
    }
    registry
        .resolve(None)
        .map_err(|error| ServerError::Configuration(error.to_string()))?;
    Ok(Arc::new(registry))
}

fn configured_context_window_tokens(model: &ModelConfig) -> Result<u32, ServerError> {
    let configured = model.context_window_tokens.or_else(|| {
        model
            .max_context_messages
            .and_then(|messages| u32::try_from(messages).ok())
            .map(|messages| messages.saturating_mul(1024))
    });
    let tokens = configured.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
    if tokens == 0 {
        return Err(ServerError::Configuration(format!(
            "模型 {}/{} 的 context_window_tokens 必须大于零",
            model.provider, model.model_id
        )));
    }
    Ok(tokens)
}

/// 初始化主会话，或在其上一轮被取消/终止后开启新的工作周期。
///
/// 主会话是固定的跨任务协调入口，不应永久停留在终态；普通子任务仍保持终态不可复活。
async fn bootstrap_main_session(store: &Arc<JsonlEventStore>) -> Result<(), String> {
    let events = store
        .load_task(koi_core::domain::TaskId::MAIN)
        .await
        .map_err(|error| error.to_string())?;
    if events.is_empty() {
        let mut runtime =
            koi_core::agent::TaskRuntime::new(Arc::clone(store), koi_core::domain::TaskId::MAIN);
        runtime
            .record(
                koi_core::domain::AgentEvent::control(
                    koi_core::domain::ControlEvent::TaskCreated {
                        trigger_event_id: None,
                    },
                ),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        runtime
            .record(
                koi_core::domain::AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        tracing::info!("已初始化主会话事件流");
        return Ok(());
    }
    let mut runtime =
        koi_core::agent::TaskRuntime::recover(Arc::clone(store), koi_core::domain::TaskId::MAIN)
            .await
            .map_err(|error| error.to_string())?;
    if runtime.projection().status.is_terminal() {
        runtime
            .record(
                koi_core::domain::AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        tracing::info!("已重新开启终止的主会话工作周期");
    }
    Ok(())
}

const RUNTIME_CONFIG_PATH: &str = "config/agent.toml";

fn load_runtime_config() -> Result<RuntimeConfig, ServerError> {
    let contents = fs::read_to_string(RUNTIME_CONFIG_PATH).map_err(|error| {
        ServerError::Configuration(format!("读取运行配置 {RUNTIME_CONFIG_PATH} 失败：{error}"))
    })?;
    toml::from_str::<RuntimeConfig>(&contents)
        .map_err(|error| ServerError::Configuration(format!("运行配置解析失败：{error}")))
}

#[derive(Debug, Error)]
enum ServerError {
    #[error("配置错误：{0}")]
    Configuration(String),
    #[error("工具注册失败：{0}")]
    ToolRegistry(String),
    #[error("事件存储初始化失败：{0}")]
    EventStore(String),
    #[error(transparent)]
    WebApi(#[from] koi_api::WebApiError),
    #[error("来源授权 Provider 注册失败：{0}")]
    AuthorizationProvider(String),
    #[error("模型 Provider 初始化失败：{0}")]
    ModelProvider(String),
    #[error("监听地址失败：{0}")]
    Bind(#[source] std::io::Error),
    #[error("HTTP 服务异常结束：{0}")]
    Serve(#[source] std::io::Error),
    #[error("接收关闭信号失败：{0}")]
    Signal(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_model_entries_and_builds_registry() {
        let config: RuntimeConfig = toml::from_str(
            r#"
                [server]
                bind_addr = "127.0.0.1:8080"
                web_dist_dir = "./web/dist"
                event_store_dir = "./data/events"
                user_store_path = "./data/users.json"
                web_cookie_secure = false
                [models]
                default_provider = "deepseek"
                default_model_id = "deepseek-chat"

                [[models.entries]]
                provider = "openai"
                base_url = "http://127.0.0.1:1/v1"
                model_id = "gpt-5-mini"
                protocol = "chat_completions"
                request_timeout_secs = 60
                context_window_tokens = 24576

                [[models.entries]]
                provider = "deepseek"
                base_url = "http://127.0.0.1:1/v1"
                model_id = "deepseek-chat"
                protocol = "responses"
                request_timeout_secs = 60
                context_window_tokens = 8192
            "#,
        )
        .unwrap();

        let registry = build_model_registry(&config).unwrap();
        assert_eq!(
            registry.default_model(),
            &ModelSelection::new("deepseek", "deepseek-chat").unwrap()
        );
        assert_eq!(
            registry.model_selections().collect::<Vec<_>>(),
            [
                &ModelSelection::new("deepseek", "deepseek-chat").unwrap(),
                &ModelSelection::new("openai", "gpt-5-mini").unwrap()
            ]
        );
        assert_eq!(
            registry.resolve(None).unwrap().1.context_window_tokens,
            8192
        );
        assert_eq!(
            registry
                .resolve(Some(&ModelSelection::new("openai", "gpt-5-mini").unwrap()))
                .unwrap()
                .0,
            &ModelSelection::new("openai", "gpt-5-mini").unwrap()
        );
    }
}
