use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use koi_api::{AlertWebhookPort, WebApi, WebAuth};
use koi_core::domain::{EventSource, PermissionLevel};
use koi_core::ports::{
    EventStore, SourceAuthorizationRegistry, StaticPermissionDirectory, ToolRegistry,
};
use koi_infra::billing::BillingPricing;
use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::ModelProviderRegistry;
use koi_infra::model_config::{
    DEFAULT_MODELS_CONFIG_PATH, ModelsConfig, build_direct_model_registry,
};
use koi_infra::model_proxy::{ModelProxyClientConfig, build_proxy_model_registry};
use koi_infra::qq_source::{QqConfig, QqSource};
use koi_infra::service_monitor::{ServiceMonitor, ServiceMonitorConfig};
use koi_infra::web_identity::WebUserStore;
use koi_infra::web_source::KoiWebSource;
use serde::Deserialize;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

mod admin_socket;
mod agent_runtime;
mod console;
mod model_trace;
mod prompts;

#[derive(Debug, Deserialize)]
struct RuntimeConfig {
    server: ServerConfig,
    #[serde(default)]
    models: ModelRuntimeConfig,
    #[serde(default)]
    agent: AgentConfig,
    #[serde(default)]
    usage: UsageConfig,
    #[serde(default)]
    qq: QqConfig,
    #[serde(default)]
    monitor: ServiceMonitorConfig,
    #[serde(default)]
    alerts: AlertConfig,
    #[serde(default)]
    logging: LoggingConfig,
}

/// 服务日志配置。完整模型请求、供应商原始响应和流式中间输出使用 `debug` 级别；
/// 默认文件过滤器会单独放行这些日志，因此开箱即可复盘一次 Agent 调用。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct LoggingConfig {
    directory: PathBuf,
    level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("./data/logs"),
            level: "info,koi.model.wire=debug,koi.model.reasoning=debug,koi.lifecycle=debug".into(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct LoggingFileConfig {
    #[serde(default)]
    logging: LoggingConfig,
}

/// 独立于 Agent 与工具配置的静态权限目录文件。
#[derive(Debug, Deserialize)]
struct AuthorizationConfig {
    #[serde(default)]
    source_defaults: Vec<SourcePermissionConfig>,
    #[serde(default)]
    principals: Vec<PrincipalPermissionConfig>,
}

#[derive(Debug, Deserialize)]
struct SourcePermissionConfig {
    source: String,
    permission: PermissionLevel,
}

#[derive(Debug, Deserialize)]
struct PrincipalPermissionConfig {
    source: String,
    subject: String,
    permission: PermissionLevel,
}

/// 主服务只保留模型配置文件的位置及代理开关；供应商地址和 API Key 位于独立文件。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct ModelRuntimeConfig {
    config_path: PathBuf,
    proxy: ModelProxyRuntimeConfig,
}

impl Default for ModelRuntimeConfig {
    fn default() -> Self {
        Self {
            config_path: PathBuf::from(DEFAULT_MODELS_CONFIG_PATH),
            proxy: ModelProxyRuntimeConfig::default(),
        }
    }
}

/// 启用后，`koi-server` 不再读取 `config_path`，只使用代理公开的脱敏模型目录。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct ModelProxyRuntimeConfig {
    enabled: bool,
    base_url: String,
    token: Option<String>,
    request_timeout_secs: u64,
}

impl Default for ModelProxyRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://127.0.0.1:9510".into(),
            token: None,
            request_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct AgentConfig {
    max_steps: u16,
    max_concurrent_tasks: usize,
    /// 单个任务（会话）累计输入与输出 Token 的硬预算；`None` 表示不限制。
    token_budget_per_task: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_steps: 8,
            max_concurrent_tasks: 4,
            token_budget_per_task: None,
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
    #[serde(default)]
    admin_socket_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
struct AlertConfig {
    /// Secret used by the machine-to-machine alert webhook. It can also be supplied through
    /// `KOI_ALERT_WEBHOOK_TOKEN` so deployments do not need to store it in the TOML file.
    webhook_token: Option<String>,
    /// The registered ingress source assigned to webhook payloads; request bodies cannot change it.
    webhook_source: String,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            webhook_token: None,
            webhook_source: "alertmanager".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct UsageConfig {
    /// 未命中缓存的输入 Token，每百万 Token 的美元价格。
    input_price_per_million_tokens: f64,
    /// 缓存命中的输入 Token，每百万 Token 的美元价格。
    cached_input_price_per_million_tokens: f64,
    /// 输出 Token，每百万 Token 的美元价格。
    output_price_per_million_tokens: f64,
    monthly_budget_usd: f64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            input_price_per_million_tokens: 0.0,
            cached_input_price_per_million_tokens: 0.0,
            output_price_per_million_tokens: 0.0,
            monthly_budget_usd: 10.0,
        }
    }
}

#[tokio::main]
async fn main() {
    let logging = load_logging_config();
    let _logging_guard = init_logging(&logging);
    let console_enabled = match console::console_enabled_from_args(std::env::args().skip(1)) {
        Ok(enabled) => enabled,
        Err(message) => {
            eprintln!("{message}");
            return;
        }
    };
    if let Err(error) = run(console_enabled).await {
        tracing::error!(%error, "koi-server 启动失败");
        std::process::exit(1);
    }
}

/// 初始化控制台与按天滚动的 JSON 文件日志。
///
/// `RUST_LOG` 优先于配置文件中的级别，便于临时提高或降低日志量。文件日志使用
/// 非阻塞写入，避免磁盘抖动拖慢 Agent 主循环；返回的 guard 必须在整个进程期间保持
/// 存活，否则后台日志写入线程会提前停止。
fn init_logging(config: &LoggingConfig) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    if let Err(error) = fs::create_dir_all(&config.directory) {
        eprintln!("创建日志目录 {} 失败：{error}", config.directory.display());
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&config.level))
            .unwrap_or_else(|_| EnvFilter::new("debug"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .init();
        return None;
    }

    let file_appender = tracing_appender::rolling::daily(&config.directory, "koi.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let file_filter = EnvFilter::try_new(&config.level).unwrap_or_else(|_| EnvFilter::new("debug"));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_filter(console_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(file_filter),
        )
        .init();

    tracing::info!(
        target: "koi.lifecycle",
        log_directory = %config.directory.display(),
        level = %config.level,
        "日志系统已启动"
    );
    Some(guard)
}

/// 在完整运行配置加载前读取日志配置；配置不存在或格式不正确时使用安全默认值，
/// 这样配置错误本身仍能进入控制台日志。
fn load_logging_config() -> LoggingConfig {
    let Ok(contents) = fs::read_to_string(RUNTIME_CONFIG_PATH) else {
        return LoggingConfig::default();
    };
    toml::from_str::<LoggingFileConfig>(&contents)
        .map(|config| config.logging)
        .unwrap_or_default()
}

#[allow(clippy::too_many_lines)]
async fn run(console_enabled: bool) -> Result<(), ServerError> {
    let config = load_runtime_config()?;
    let pricing = BillingPricing {
        input_price_per_million_tokens: config.usage.input_price_per_million_tokens,
        cached_input_price_per_million_tokens: config.usage.cached_input_price_per_million_tokens,
        output_price_per_million_tokens: config.usage.output_price_per_million_tokens,
    }
    .validate()
    .map_err(|error| ServerError::Configuration(error.to_string()))?;
    if config.usage.monthly_budget_usd < 0.0 || !config.usage.monthly_budget_usd.is_finite() {
        return Err(ServerError::Configuration(
            "usage.monthly_budget_usd 必须是非负的有限数字".into(),
        ));
    }
    if config.agent.token_budget_per_task == Some(0) {
        return Err(ServerError::Configuration(
            "agent.token_budget_per_task 必须大于零；不限制请删除该字段".into(),
        ));
    }
    tracing::debug!(
        target: "koi.lifecycle",
        log_directory = %config.logging.directory.display(),
        log_level = %config.logging.level,
        "运行配置已加载"
    );
    let permissions = Arc::new(load_authorization_directory()?);
    let prompts = prompts::ServerPromptProvider;
    let model_registry = build_model_registry(&config.models).await?;

    let mut registry = ToolRegistry::default();
    let mut registered = koi_infra::tools::register_builtin_tools(&mut registry)
        .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;
    let task_tools = koi_core::agent::task_tools::register_task_management_tools(&mut registry)
        .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;

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
    let identities = Arc::new(
        WebUserStore::open(&config.server.user_store_path, Arc::clone(&permissions))
            .map_err(ServerError::WebApi)?,
    );
    let qq_config = config.qq.clone().with_environment_credentials();
    let qq_source = if qq_config.has_any_credentials() {
        let qq_source = Arc::new(
            QqSource::new(
                qq_config,
                Arc::clone(&store),
                Arc::clone(&permissions),
                Arc::clone(&task_manager),
            )
            .map_err(|error| ServerError::QqSource(error.to_string()))?,
        );
        tracing::info!("已启用 QQ 来源");
        Some(qq_source)
    } else {
        tracing::info!("未配置 QQ 凭证，跳过 QQ 来源");
        None
    };
    if let Some(qq_source) = qq_source.as_ref() {
        registered += koi_infra::tools::register_qq_tools(&mut registry, Arc::clone(qq_source))
            .map_err(|error| ServerError::ToolRegistry(error.to_string()))?;
    }
    let tools = Arc::new(registry);
    let tool_definitions = tools.list_definitions();
    let source = Arc::new(
        KoiWebSource::new(
            Arc::clone(&store),
            Arc::clone(&identities),
            Arc::clone(&task_manager),
            tool_definitions,
            config.usage.monthly_budget_usd,
        )
        .map_err(ServerError::WebApi)?
        .with_billing(pricing, config.agent.token_budget_per_task)
        .with_model_catalog(
            model_registry
                .entries()
                .map(|(selection, entry)| (selection.clone(), entry.context_window_tokens)),
            model_registry.default_model().clone(),
        ),
    );
    validate_alert_webhook_source(&config.alerts.webhook_source)?;
    let auth = WebAuth::new(identities, config.server.web_cookie_secure);
    let mut authorization_providers = SourceAuthorizationRegistry::default();
    authorization_providers
        .register(source.authorization_provider())
        .map_err(|error| ServerError::AuthorizationProvider(error.to_string()))?;
    if let Some(qq_source) = qq_source.as_ref() {
        authorization_providers
            .register(qq_source.authorization_provider())
            .map_err(|error| ServerError::AuthorizationProvider(error.to_string()))?;
    }
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

    let reasoning_trace = model_trace::ModelTrace::new();
    let supervisor = agent_runtime::AgentSupervisor::new(
        Arc::clone(&store),
        Arc::clone(&model_registry),
        tools,
        authorization_providers,
        Arc::new(prompts),
        task_manager,
        Arc::clone(&reasoning_trace),
        config.agent.max_steps,
        config.agent.max_concurrent_tasks,
        config.agent.token_budget_per_task,
    );
    let monitor = if config.monitor.enabled {
        Some(
            ServiceMonitor::new(config.monitor.clone(), Arc::clone(&source))
                .map_err(|error| ServerError::Monitor(error.to_string()))?,
        )
    } else {
        None
    };
    let shutdown = CancellationToken::new();
    let supervisor_task = tokio::spawn(Arc::clone(&supervisor).run(shutdown.clone()));
    let qq_task = qq_source.map(|qq_source| tokio::spawn(qq_source.run(shutdown.clone())));
    let monitor_task = monitor.map(|monitor| tokio::spawn(monitor.run(shutdown.clone())));
    let admin_socket_task = admin_socket_path(&config.server).map(|path| {
        admin_socket::spawn(
            path,
            Arc::clone(&store),
            Arc::clone(&model_registry),
            Arc::clone(&supervisor),
            Arc::clone(&reasoning_trace),
            shutdown.clone(),
        )
    });
    let console_task = console_enabled.then(|| {
        console::spawn(
            Arc::clone(&store),
            Arc::clone(&model_registry),
            Arc::clone(&supervisor),
            shutdown.clone(),
        )
    });

    let alert_webhook: Arc<dyn AlertWebhookPort> = source.clone();
    let api: Arc<dyn WebApi> = source;
    let webhook_token = config
        .alerts
        .webhook_token
        .clone()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            std::env::var("KOI_ALERT_WEBHOOK_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())
        });
    let api_router = koi_api::router_with_alert_webhook(
        api,
        auth,
        Some(alert_webhook),
        webhook_token,
        config.alerts.webhook_source.clone(),
    )
    .layer(TraceLayer::new_for_http());

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
        result = axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned()) => result.map_err(ServerError::Serve),
        result = tokio::signal::ctrl_c() => result.map_err(ServerError::Signal),
    };
    shutdown.cancel();
    let _ = supervisor_task.await;
    if let Some(qq_task) = qq_task {
        let _ = qq_task.await;
    }
    if let Some(monitor_task) = monitor_task {
        let _ = monitor_task.await;
    }
    if let Some(console_task) = console_task {
        let _ = console_task.await;
    }
    if let Some(admin_socket_task) = admin_socket_task {
        let _ = admin_socket_task.await;
    }
    result
}

fn admin_socket_path(server: &ServerConfig) -> Option<PathBuf> {
    std::env::var_os("KOI_ADMIN_SOCKET_PATH")
        .map(PathBuf::from)
        .or_else(|| server.admin_socket_path.clone())
}

fn validate_alert_webhook_source(source: &str) -> Result<(), ServerError> {
    if matches!(source, "alertmanager" | "webhook") {
        return Ok(());
    }
    Err(ServerError::Configuration(format!(
        "alerts.webhook_source 不支持：{source}，可选 alertmanager 或 webhook"
    )))
}

async fn build_model_registry(
    config: &ModelRuntimeConfig,
) -> Result<Arc<ModelProviderRegistry>, ServerError> {
    if config.proxy.enabled {
        let token = config
            .proxy
            .token
            .clone()
            .filter(|token| !token.trim().is_empty())
            .or_else(|| {
                std::env::var("KOI_MODEL_PROXY_TOKEN")
                    .ok()
                    .filter(|token| !token.trim().is_empty())
            })
            .ok_or_else(|| {
                ServerError::Configuration(
                    "[models.proxy] 已启用，但未配置 token 或 KOI_MODEL_PROXY_TOKEN".into(),
                )
            })?;
        return build_proxy_model_registry(ModelProxyClientConfig {
            base_url: config.proxy.base_url.clone(),
            token,
            request_timeout_secs: config.proxy.request_timeout_secs,
        })
        .await
        .map_err(|error| ServerError::ModelProvider(error.to_string()));
    }

    let models = ModelsConfig::load(&config.config_path)
        .map_err(|error| ServerError::Configuration(error.to_string()))?;
    build_direct_model_registry(&models)
        .map_err(|error| ServerError::ModelProvider(error.to_string()))
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
const AUTHORIZATION_CONFIG_PATH: &str = "config/authorization.toml";

fn load_runtime_config() -> Result<RuntimeConfig, ServerError> {
    let contents = fs::read_to_string(RUNTIME_CONFIG_PATH).map_err(|error| {
        ServerError::Configuration(format!("读取运行配置 {RUNTIME_CONFIG_PATH} 失败：{error}"))
    })?;
    toml::from_str::<RuntimeConfig>(&contents)
        .map_err(|error| ServerError::Configuration(format!("运行配置解析失败：{error}")))
}

fn load_authorization_directory() -> Result<StaticPermissionDirectory, ServerError> {
    let contents = fs::read_to_string(AUTHORIZATION_CONFIG_PATH).map_err(|error| {
        ServerError::Configuration(format!(
            "读取权限配置 {AUTHORIZATION_CONFIG_PATH} 失败：{error}"
        ))
    })?;
    let config: AuthorizationConfig = toml::from_str(&contents)
        .map_err(|error| ServerError::Configuration(format!("权限配置解析失败：{error}")))?;
    Ok(StaticPermissionDirectory::new(
        config
            .source_defaults
            .into_iter()
            .map(|entry| (entry.source, entry.permission)),
        config
            .principals
            .into_iter()
            .map(|entry| (entry.source, entry.subject, entry.permission)),
    ))
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
    #[error("QQ 来源初始化失败：{0}")]
    QqSource(String),
    #[error("服务监测初始化失败：{0}")]
    Monitor(String),
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
    fn parses_separate_model_configuration_and_proxy_switch() {
        let config: RuntimeConfig = toml::from_str(
            r#"
                [server]
                bind_addr = "127.0.0.1:8080"
                web_dist_dir = "./web/dist"
                event_store_dir = "./data/events"
                user_store_path = "./data/users.json"
                web_cookie_secure = false
                [models]
                config_path = "./config/models.toml"

                [models.proxy]
                enabled = true
                base_url = "http://127.0.0.1:9510"
                request_timeout_secs = 180

                [monitor]
                enabled = true
                interval_secs = 15
                timeout_secs = 3
                failure_threshold = 2
                recovery_threshold = 2

                [[monitor.checks]]
                id = "api"
                kind = "http"
                target = "http://127.0.0.1:8080/healthz"

                [alerts]
                webhook_source = "webhook"
            "#,
        )
        .unwrap();

        assert!(config.monitor.enabled);
        assert_eq!(config.monitor.checks.len(), 1);
        assert_eq!(config.alerts.webhook_source, "webhook");
        assert_eq!(
            config.models.config_path,
            PathBuf::from("./config/models.toml")
        );
        assert!(config.models.proxy.enabled);
        assert_eq!(config.models.proxy.request_timeout_secs, 180);
    }
}
