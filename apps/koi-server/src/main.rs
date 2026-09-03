use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use koi_api::{WebApi, WebAuth};
use koi_core::domain::{EventSource, ModelGenerationOptions, ModelProtocol};
use koi_core::ports::{SourceAuthorizationRegistry, ToolRegistry};
use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::{OpenAiCompatibleModelConfig, OpenAiCompatibleModelProvider};
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
#[serde(default)]
struct RuntimeConfig {
    server: ServerConfig,
    security: ToolPolicy,
    model: ModelConfig,
    agent: AgentConfig,
    usage: UsageConfig,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            security: ToolPolicy::default(),
            model: ModelConfig::default(),
            agent: AgentConfig::default(),
            usage: UsageConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ModelConfig {
    base_url: String,
    name: String,
    protocol: String,
    request_timeout_secs: u64,
    max_context_messages: usize,
    reasoning_effort: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".into(),
            name: "gpt-5-mini".into(),
            protocol: "responses".into(),
            request_timeout_secs: 60,
            max_context_messages: 32,
            reasoning_effort: Some("medium".into()),
        }
    }
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
#[serde(default)]
struct ServerConfig {
    bind_addr: String,
    web_dist_dir: PathBuf,
    event_store_dir: PathBuf,
    user_store_path: PathBuf,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8080".into(),
            web_dist_dir: PathBuf::from("./web/dist"),
            event_store_dir: PathBuf::from("./data/events"),
            user_store_path: PathBuf::from("./data/users.json"),
        }
    }
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

async fn run() -> Result<(), ServerError> {
    let config = load_runtime_config();
    let web_token = load_web_admin_token()?;
    let prompts = prompts::ServerPromptProvider;

    let mut registry = ToolRegistry::default();
    let registered = koi_infra::tools::register_builtin_tools(&mut registry, config.security)
        .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;
    let tools = Arc::new(registry);
    let tool_definitions = tools.list_definitions();

    let protocol = parse_model_protocol(&config.model.protocol)?;
    let model_config =
        OpenAiCompatibleModelConfig::from_env(config.model.base_url, config.model.name)
            .with_protocol(protocol)
            .with_request_timeout_secs(config.model.request_timeout_secs);
    let model = Arc::new(
        OpenAiCompatibleModelProvider::new(model_config)
            .map_err(|error| ServerError::ModelProvider(error.to_string()))?,
    );
    let model_options = ModelGenerationOptions {
        reasoning_effort: config
            .model
            .reasoning_effort
            .filter(|effort| !effort.trim().is_empty()),
        ..ModelGenerationOptions::default()
    };

    let store = Arc::new(
        JsonlEventStore::open(&config.server.event_store_dir)
            .map_err(|error| ServerError::EventStore(error.to_string()))?,
    );
    let identities = Arc::new(
        WebUserStore::open(&config.server.user_store_path, web_token)
            .map_err(ServerError::WebApi)?,
    );
    let source = Arc::new(
        KoiWebSource::new(
            Arc::clone(&store),
            Arc::clone(&identities),
            tool_definitions,
            config.usage.monthly_budget_usd,
        )
        .map_err(ServerError::WebApi)?,
    );
    let auth = WebAuth::new(identities);
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
                    event_sink.publish_event(&event);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let supervisor = agent_runtime::AgentSupervisor::new(
        Arc::clone(&store),
        model,
        tools,
        authorization_providers,
        Arc::new(prompts),
        model_options,
        config.agent.max_steps,
        config.model.max_context_messages,
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
        tool_count = registered,
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

fn load_runtime_config() -> RuntimeConfig {
    let path = std::env::var("KOI_CONFIG_PATH").unwrap_or_else(|_| "config/agent.toml".into());
    let Ok(contents) = fs::read_to_string(&path) else {
        tracing::warn!(path, "未找到运行配置，使用默认的 fail-closed 工具策略");
        return RuntimeConfig::default();
    };
    match toml::from_str::<RuntimeConfig>(&contents) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(path, %error, "运行配置解析失败，使用默认运行配置");
            RuntimeConfig::default()
        }
    }
}

fn load_web_admin_token() -> Result<String, ServerError> {
    let token = std::env::var("KOI_WEB_ADMIN_TOKEN")
        .map_err(|_| ServerError::Configuration("缺少 KOI_WEB_ADMIN_TOKEN".into()))?;
    if token.trim().is_empty() || token == "change-me-to-a-long-random-token" {
        return Err(ServerError::Configuration(
            "KOI_WEB_ADMIN_TOKEN 必须设置为非示例值".into(),
        ));
    }
    Ok(token)
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
