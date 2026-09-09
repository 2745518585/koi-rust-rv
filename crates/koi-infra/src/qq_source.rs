//! QQ 官方机器人来源适配器。
//!
//! 该模块只负责 QQ Open Platform 的传输与消息标准化：App Access Token、Gateway
//! WebSocket、心跳/重连、C2C/群聊/频道消息以及 QQ 侧确认/控制指令。输入仍统一交给
//! `koi-core::ports::IngressRegistrar`，回复和主动投送由 QQ 工具显式触发，因而不会
//! 绕过核心的权限、事件和 Agent 调度链路。

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::event_store::JsonlEventStore;
use async_trait::async_trait;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use koi_core::agent::{
    ControlExecutionRequest, ControlExecutor, DirectControlAuthority, TaskManager, TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, ApprovalGrant, AuthorizationRequest, AuthorizationRequestResult, ContextEnvelope,
    ContextKind, ContextOrigin, ContextPayload, ControlEvent, EventEnvelope, EventId, IngressDraft,
    IngressEvent, PermissionAssessment, PermissionLevel, PolicyDecision, Principal, Scope,
    SourceName, TaskId, ToolEvent,
};
use koi_core::ports::{
    AuthorizationError, EventStore, IngressRegistrar, IngressSourceDefinition,
    IngressSourceRegistry, SourceAuthorizationProvider, StaticPermissionDirectory,
};
use reqwest::{Client, Method, StatusCode, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OnceCell};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// QQ 在核心中的稳定来源名称。
pub const QQ_SOURCE_NAME: &str = "qq";

/// QQ 官方 `GROUP_AND_C2C_EVENT` intent。
pub const INTENT_GROUP_AND_C2C_EVENT: u64 = 1 << 25;
/// QQ 官方 `INTERACTION` intent；当前适配器保留给后续按钮审批扩展。
pub const INTENT_INTERACTION: u64 = 1 << 26;
/// QQ 官方 `DIRECT_MESSAGE` intent，用于频道私信事件。
pub const INTENT_DIRECT_MESSAGE: u64 = 1 << 12;
/// QQ 官方 `PUBLIC_GUILD_MESSAGES` intent，用于频道消息事件。
pub const INTENT_PUBLIC_GUILD_MESSAGES: u64 = 1 << 30;

const DEFAULT_API_BASE_URL: &str = "https://api.sgroup.qq.com";
const APP_ACCESS_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const QQ_GATEWAY_INSTANCE: &str = "qq-gateway";
const DEFAULT_INTENTS: u64 = INTENT_GROUP_AND_C2C_EVENT;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 10;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_RECONNECT_BASE_DELAY_MS: u64 = 1_000;
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
const DEFAULT_MAX_REPLY_CHARS: usize = 1_500;
const MAX_HTTP_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_TEXT_CHARS: usize = 512;
const DEDUP_CACHE_SIZE: usize = 2_048;
const MAX_CONTEXT_LABEL_VALUE_CHARS: usize = 128;
const MAX_CONTEXT_DISPLAY_NAME_CHARS: usize = 80;
pub(crate) const MAX_GROUP_OPENID_CHARS: usize = 256;
pub(crate) const MAX_CONTENT_CHARS: usize = 12_000;
const MAX_CONTROL_REASON_CHARS: usize = 512;

/// QQ 来源的运行配置。
///
/// `app_id` 与 `app_secret` 可以写在本地 TOML，也可以使用
/// `QQ_BOT_APP_ID` / `QQ_BOT_APP_SECRET` 环境变量。两个凭证都为空时，来源被禁用，
/// 这样不配置 QQ 的部署仍可只运行 Web 控制台。
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct QqConfig {
    pub app_id: Option<String>,
    pub app_secret: Option<String>,
    pub api_base_url: String,
    /// Gateway 订阅位图。默认订阅群聊@与 C2C 事件；频道事件需显式加入
    /// `PUBLIC_GUILD_MESSAGES`。
    pub intents: u64,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub reconnect_base_delay_ms: u64,
    pub reconnect_max_delay_ms: u64,
    /// `GROUP_MESSAGE_CREATE` 是否只处理明确 @ 机器人的消息。
    pub mention_only: bool,
    /// 主要汇报群的 `group_openid`。配置后才会注册只向该群投送的 `qq.report` 工具。
    pub report_group_openid: Option<String>,
    /// 保留的兼容配置项。QQ 普通发言固定建议 `User`，明确 @bot 的发言固定建议
    /// `Operator`；因此该字段只能配置为 `User`。
    pub default_permission: PermissionLevel,
    /// QQ 单条文本消息的最大字符数；过长回复会被拆成多条发送。
    pub max_reply_chars: usize,
}

impl Default for QqConfig {
    fn default() -> Self {
        Self {
            app_id: None,
            app_secret: None,
            api_base_url: DEFAULT_API_BASE_URL.into(),
            intents: DEFAULT_INTENTS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            reconnect_base_delay_ms: DEFAULT_RECONNECT_BASE_DELAY_MS,
            reconnect_max_delay_ms: DEFAULT_RECONNECT_MAX_DELAY_MS,
            mention_only: true,
            report_group_openid: None,
            default_permission: PermissionLevel::User,
            max_reply_chars: DEFAULT_MAX_REPLY_CHARS,
        }
    }
}

impl QqConfig {
    /// 用环境变量补齐未填写的敏感凭证。
    #[must_use]
    pub fn with_environment_credentials(mut self) -> Self {
        if is_blank(self.app_id.as_deref()) {
            self.app_id = env::var("QQ_BOT_APP_ID").ok();
        }
        if is_blank(self.app_secret.as_deref()) {
            self.app_secret = env::var("QQ_BOT_APP_SECRET").ok();
        }
        self
    }

    #[must_use]
    pub fn has_any_credentials(&self) -> bool {
        !is_blank(self.app_id.as_deref()) || !is_blank(self.app_secret.as_deref())
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        !is_blank(self.app_id.as_deref()) && !is_blank(self.app_secret.as_deref())
    }

    /// 验证 QQ 传输配置。凭证为空时由应用层决定跳过来源，因此本方法只验证
    /// 已准备启用的配置。
    ///
    /// # Errors
    ///
    /// 当凭证、URL、超时、重试或消息长度配置无效时返回错误。
    pub fn validate(&self) -> Result<(), QqError> {
        if !self.is_configured() {
            return Err(QqError::Configuration(
                "QQ 来源需要同时配置 app_id 和 app_secret（或 QQ_BOT_APP_ID / QQ_BOT_APP_SECRET）"
                    .into(),
            ));
        }
        if self
            .app_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            || self
                .app_secret
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(QqError::Configuration(
                "QQ app_id 和 app_secret 不能为空".into(),
            ));
        }
        let url = Url::parse(&self.api_base_url)
            .map_err(|error| QqError::Configuration(format!("QQ API 地址无效：{error}")))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(QqError::Configuration(
                "QQ API 地址只允许不带用户信息、query 或 fragment 的 http/https URL".into(),
            ));
        }
        if self.intents == 0 {
            return Err(QqError::Configuration("QQ intents 必须大于零".into()));
        }
        if self.request_timeout_secs == 0 {
            return Err(QqError::Configuration(
                "QQ request_timeout_secs 必须大于零".into(),
            ));
        }
        if self.reconnect_base_delay_ms == 0 || self.reconnect_max_delay_ms == 0 {
            return Err(QqError::Configuration("QQ 重连退避时间必须大于零".into()));
        }
        if self.reconnect_base_delay_ms > self.reconnect_max_delay_ms {
            return Err(QqError::Configuration(
                "QQ reconnect_base_delay_ms 不能大于 reconnect_max_delay_ms".into(),
            ));
        }
        if self.max_reply_chars == 0 {
            return Err(QqError::Configuration(
                "QQ max_reply_chars 必须大于零".into(),
            ));
        }
        if let Some(group_openid) = self.report_group_openid.as_deref() {
            if group_openid.trim().is_empty() {
                return Err(QqError::Configuration(
                    "QQ report_group_openid 不能为空；不启用主要汇报群时请省略该字段".into(),
                ));
            }
            if group_openid.chars().count() > MAX_GROUP_OPENID_CHARS {
                return Err(QqError::Configuration(format!(
                    "QQ report_group_openid 超过 {MAX_GROUP_OPENID_CHARS} 个字符"
                )));
            }
        }
        if self.default_permission != PermissionLevel::User {
            return Err(QqError::Configuration(
                "QQ 普通发言建议权限固定为 User；明确 @bot 的发言才建议为 Operator".into(),
            ));
        }
        Ok(())
    }
}

/// QQ 来源运行错误。
#[derive(Debug, Error)]
pub enum QqError {
    #[error("QQ 配置无效：{0}")]
    Configuration(String),
    #[error("QQ 网络请求失败：{0}")]
    Network(String),
    #[error("QQ API 调用失败（HTTP {status}，错误码 {code:?}）：{message}")]
    Api {
        status: u16,
        code: Option<i64>,
        message: String,
    },
    #[error("QQ Gateway 失败：{0}")]
    Gateway(String),
    #[error("QQ 事件处理失败：{0}")]
    Event(String),
    #[error("QQ 核心事件处理失败：{0}")]
    Core(String),
}

struct TokenCache {
    token: String,
    expires_at: Instant,
}

struct QqTokenManager {
    client: Client,
    app_id: String,
    app_secret: String,
    endpoint: String,
    timeout: Duration,
    cached: AsyncMutex<Option<TokenCache>>,
}

impl QqTokenManager {
    fn new(config: &QqConfig, client: Client) -> Result<Self, QqError> {
        let app_id = config
            .app_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| QqError::Configuration("QQ app_id 未配置".into()))?
            .trim()
            .to_owned();
        let app_secret = config
            .app_secret
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| QqError::Configuration("QQ app_secret 未配置".into()))?
            .trim()
            .to_owned();
        Ok(Self {
            client,
            app_id,
            app_secret,
            endpoint: APP_ACCESS_TOKEN_URL.into(),
            timeout: Duration::from_secs(config.request_timeout_secs),
            cached: AsyncMutex::new(None),
        })
    }

    async fn get(&self) -> Result<String, QqError> {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_some_and(|entry| Instant::now() < entry.expires_at)
        {
            return Ok(cached
                .as_ref()
                .expect("cached token checked above")
                .token
                .clone());
        }

        let response = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .header("content-type", "application/json")
            .json(&json!({
                "appId": self.app_id,
                "clientSecret": self.app_secret,
            }))
            .send()
            .await
            .map_err(|error| QqError::Network(format!("获取 Access Token 失败：{error}")))?;
        let status = response.status();
        let body = read_json_body(response).await?;
        if !status.is_success() {
            return Err(QqError::Api {
                status: status.as_u16(),
                code: api_error_code(&body),
                message: response_error_message(&body),
            });
        }
        let token = body
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| QqError::Api {
                status: status.as_u16(),
                code: api_error_code(&body),
                message: "Access Token 响应缺少 access_token".into(),
            })?;
        let expires_in = body
            .get("expires_in")
            .and_then(value_u64)
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| QqError::Api {
                status: status.as_u16(),
                code: api_error_code(&body),
                message: "Access Token 响应缺少有效 expires_in".into(),
            })?;
        // 官方 token 通常有效 7200 秒，提前两分钟刷新；很短的测试 TTL 仍至少留出
        // 一秒，避免拿到刚生成就被视为过期的缓存。
        let refresh_skew = Duration::from_secs(120).min(Duration::from_secs(expires_in / 2));
        let lifetime = Duration::from_secs(expires_in).saturating_sub(refresh_skew);
        cached.replace(TokenCache {
            token: token.to_owned(),
            expires_at: Instant::now() + lifetime.max(Duration::from_secs(1)),
        });
        Ok(token.to_owned())
    }

    async fn invalidate(&self, token: &str) {
        let mut cached = self.cached.lock().await;
        if cached.as_ref().is_some_and(|entry| entry.token == token) {
            cached.take();
        }
    }
}

struct QqApiClient {
    client: Client,
    token_manager: QqTokenManager,
    base_url: String,
    timeout: Duration,
    max_retries: u32,
}

impl QqApiClient {
    fn new(config: &QqConfig) -> Result<Self, QqError> {
        let timeout = Duration::from_secs(config.request_timeout_secs);
        let client = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| QqError::Network(format!("初始化 QQ HTTP 客户端失败：{error}")))?;
        let token_manager = QqTokenManager::new(config, client.clone())?;
        Ok(Self {
            client,
            token_manager,
            base_url: config.api_base_url.trim_end_matches('/').to_owned(),
            timeout,
            max_retries: config.max_retries,
        })
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, QqError> {
        let mut attempt = 0;
        loop {
            let token = self.token_manager.get().await?;
            let mut request = self
                .client
                .request(method.clone(), format!("{}{}", self.base_url, path))
                .timeout(self.timeout)
                .header("authorization", format!("QQBot {token}"))
                .header("x-union-appid", &self.token_manager.app_id);
            if let Some(body) = body.clone() {
                request = request.json(&body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    if attempt >= self.max_retries {
                        return Err(QqError::Network(format!("QQ API 网络请求失败：{error}")));
                    }
                    sleep_retry(attempt, None).await;
                    attempt += 1;
                    continue;
                }
            };
            let status = response.status();
            let retry_after = retry_after(&response);
            let response_body = read_json_body(response).await?;
            let code = api_error_code(&response_body);
            let success = status.is_success() && code.is_none_or(|code| code == 0);
            if success {
                return Ok(response_body);
            }

            if status == StatusCode::UNAUTHORIZED {
                self.token_manager.invalidate(&token).await;
            }
            let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
            if attempt < self.max_retries && (retryable || status == StatusCode::UNAUTHORIZED) {
                sleep_retry(attempt, retry_after).await;
                attempt += 1;
                continue;
            }

            return Err(QqError::Api {
                status: status.as_u16(),
                code,
                message: response_error_message(&response_body),
            });
        }
    }

    async fn gateway_url(&self) -> Result<String, QqError> {
        let body = self.request(Method::GET, "/gateway", None).await?;
        body.get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| QqError::Api {
                status: 200,
                code: None,
                message: "Gateway 响应缺少 url".into(),
            })
    }

    async fn group_bot_state(&self, group_openid: &str) -> Result<String, QqError> {
        let path = format!("/v2/groups/{}/bot_state", encode_path_segment(group_openid));
        let body = self.request(Method::GET, &path, None).await?;
        body.get("member_openid")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| QqError::Api {
                status: 200,
                code: None,
                message: "机器人群状态响应缺少 member_openid".into(),
            })
    }

    async fn send_target(
        &self,
        target: &QqTarget,
        content: &str,
        msg_seq: u32,
    ) -> Result<String, QqError> {
        let (path, mut body) = match target {
            QqTarget::C2c { user_openid, .. } => (
                format!("/v2/users/{}/messages", encode_path_segment(user_openid)),
                json!({"msg_type": 0, "content": content}),
            ),
            QqTarget::Group {
                group_openid,
                msg_id: _,
            } => (
                format!("/v2/groups/{}/messages", encode_path_segment(group_openid)),
                json!({"msg_type": 0, "content": content}),
            ),
            QqTarget::Channel { channel_id, .. } => (
                format!("/channels/{}/messages", encode_path_segment(channel_id)),
                json!({"content": content}),
            ),
            QqTarget::Direct { guild_id, .. } => (
                format!("/dms/{}/messages", encode_path_segment(guild_id)),
                json!({"content": content}),
            ),
        };
        let msg_id = target.message_id();
        if !msg_id.is_empty() {
            body["msg_id"] = Value::String(msg_id.to_owned());
            if matches!(target, QqTarget::C2c { .. } | QqTarget::Group { .. }) {
                body["msg_seq"] = json!(msg_seq);
            }
        }
        let response = self.request(Method::POST, &path, Some(body)).await?;
        Ok(response
            .get("id")
            .or_else(|| response.get("message_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }
}

#[derive(Clone, Debug)]
enum QqTarget {
    C2c {
        user_openid: String,
        msg_id: String,
    },
    Group {
        group_openid: String,
        msg_id: String,
    },
    Channel {
        channel_id: String,
        msg_id: String,
    },
    Direct {
        guild_id: String,
        msg_id: String,
    },
}

impl QqTarget {
    fn context_kind_label(&self) -> &'static str {
        match self {
            Self::C2c { .. } => "私聊（C2C）",
            Self::Group { .. } => "群聊",
            Self::Channel { .. } => "频道",
            Self::Direct { .. } => "频道私信",
        }
    }

    fn conversation_key(&self) -> String {
        match self {
            Self::C2c { user_openid, .. } => format!("c2c:{user_openid}"),
            Self::Group { group_openid, .. } => format!("group:{group_openid}"),
            Self::Channel { channel_id, .. } => format!("channel:{channel_id}"),
            Self::Direct { guild_id, .. } => format!("direct:{guild_id}"),
        }
    }

    fn message_id(&self) -> &str {
        match self {
            Self::C2c { msg_id, .. }
            | Self::Group { msg_id, .. }
            | Self::Channel { msg_id, .. }
            | Self::Direct { msg_id, .. } => msg_id,
        }
    }

    fn reply_sequence_key(&self) -> String {
        match self {
            Self::C2c { msg_id, .. } | Self::Group { msg_id, .. } if !msg_id.trim().is_empty() => {
                format!("reply:{msg_id}")
            }
            _ => self.conversation_key(),
        }
    }

    fn scope(&self) -> Scope {
        match self {
            Self::C2c { user_openid, .. } => Scope::new("qq_c2c", user_openid),
            Self::Group { group_openid, .. } => Scope::new("qq_group", group_openid),
            Self::Channel { channel_id, .. } => Scope::new("qq_channel", channel_id),
            Self::Direct { guild_id, .. } => Scope::new("qq_direct", guild_id),
        }
    }

    fn from_context(context: &ContextEnvelope) -> Option<Self> {
        let msg_id = context.origin.native_event_id.trim();
        if msg_id.is_empty() || context.origin.source != QQ_SOURCE_NAME {
            return None;
        }
        match context.scope.kind.as_str() {
            "qq_c2c" => Some(Self::C2c {
                user_openid: context.scope.id.clone(),
                msg_id: msg_id.into(),
            }),
            "qq_group" => Some(Self::Group {
                group_openid: context.scope.id.clone(),
                msg_id: msg_id.into(),
            }),
            "qq_channel" => Some(Self::Channel {
                channel_id: context.scope.id.clone(),
                msg_id: msg_id.into(),
            }),
            "qq_direct" => Some(Self::Direct {
                guild_id: context.scope.id.clone(),
                msg_id: msg_id.into(),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct NormalizedMessage {
    message_id: String,
    dispatch_id: Option<String>,
    target: QqTarget,
    subject: String,
    display_name: Option<String>,
    text: String,
    mentions: Vec<String>,
    explicit_bot_mention: bool,
}

/// QQ 群聊投送的 API 回执摘要。
#[derive(Clone, Debug)]
pub(crate) struct QqGroupDelivery {
    pub(crate) group_openid: String,
    pub(crate) reply_to_message_id: Option<String>,
    pub(crate) message_ids: Vec<String>,
    pub(crate) chunks: usize,
}

/// QQ 当前消息回复的 API 回执摘要。
#[derive(Clone, Debug)]
pub(crate) struct QqReplyDelivery {
    pub(crate) scope: Scope,
    pub(crate) reply_to_message_id: String,
    pub(crate) message_ids: Vec<String>,
    pub(crate) chunks: usize,
}

#[derive(Clone, Debug)]
struct PendingApproval {
    task_id: TaskId,
    request_event_id: EventId,
    proposal_event_id: EventId,
    original_context: ContextEnvelope,
}

impl NormalizedMessage {
    fn scope(&self) -> Scope {
        self.target.scope()
    }

    fn conversation_key(&self) -> String {
        self.target.conversation_key()
    }
}

#[derive(Default)]
struct DedupCache {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl DedupCache {
    fn insert(&mut self, key: String) -> bool {
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > DEDUP_CACHE_SIZE {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

/// 已接入核心事件存储和任务管理器的 QQ 来源。
pub struct QqSource {
    config: QqConfig,
    api: QqApiClient,
    store: Arc<JsonlEventStore>,
    permissions: Arc<StaticPermissionDirectory>,
    sources: IngressSourceRegistry,
    write_lock: AsyncMutex<()>,
    group_bot_member_openids: AsyncMutex<HashMap<String, Arc<OnceCell<String>>>>,
    outgoing_sequences: AsyncMutex<HashMap<String, u32>>,
    dedup: Mutex<DedupCache>,
}

impl QqSource {
    /// 创建 QQ 来源。凭证必须在调用前由应用层从配置或环境变量补齐；QQ 输入统一
    /// 写入主会话，构造函数中的任务管理器参数仅为兼容既有调用方而保留。
    ///
    /// # Errors
    ///
    /// 当 QQ 配置、HTTP 客户端或核心来源注册失败时返回错误。
    pub fn new(
        config: QqConfig,
        store: Arc<JsonlEventStore>,
        permissions: Arc<StaticPermissionDirectory>,
        _task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
    ) -> Result<Self, QqError> {
        config.validate()?;
        let mut sources = IngressSourceRegistry::default();
        sources
            .register(IngressSourceDefinition {
                source: SourceName::new(QQ_SOURCE_NAME)
                    .map_err(|error| QqError::Configuration(format!("QQ 来源名无效：{error}")))?,
                maximum_permission: PermissionLevel::Operator,
            })
            .map_err(|error| QqError::Configuration(error.to_string()))?;
        Ok(Self {
            api: QqApiClient::new(&config)?,
            config,
            store,
            permissions,
            sources,
            write_lock: AsyncMutex::new(()),
            group_bot_member_openids: AsyncMutex::new(HashMap::new()),
            outgoing_sequences: AsyncMutex::new(HashMap::new()),
            dedup: Mutex::new(DedupCache::default()),
        })
    }

    /// 创建供核心 Agent 使用的 QQ 提权 Provider。
    #[must_use]
    pub fn authorization_provider(self: &Arc<Self>) -> Arc<dyn SourceAuthorizationProvider> {
        Arc::new(QqAuthorizationProvider {
            source: Arc::clone(self),
        })
    }

    /// 返回配置的主要汇报群；未配置时不暴露 `qq.report` 工具。
    #[must_use]
    pub(crate) fn report_group_openid(&self) -> Option<&str> {
        self.config.report_group_openid.as_deref()
    }

    /// 启动 QQ Gateway 和事件回复循环，直到宿主发出关闭信号。
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        if let Err(error) = self.hydrate_message_dedup().await {
            tracing::warn!(%error, "恢复 QQ 消息去重状态失败，可能重复处理历史消息");
        }

        if let Err(error) = self.gateway_loop(shutdown.clone()).await {
            tracing::error!(%error, "QQ 来源已停止");
        }
    }

    async fn gateway_loop(self: &Arc<Self>, shutdown: CancellationToken) -> Result<(), QqError> {
        let mut session = GatewaySession::default();
        let mut delay = self.config.reconnect_base_delay_ms;
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let gateway_url = match self.api.gateway_url().await {
                Ok(url) => url,
                Err(error) => {
                    tracing::warn!(%error, retry_in_ms = delay, "获取 QQ Gateway 地址失败");
                    sleep_or_shutdown(delay, &shutdown).await;
                    delay = doubled_delay(delay, self.config.reconnect_max_delay_ms);
                    continue;
                }
            };

            match self
                .connect_gateway(&gateway_url, &mut session, &shutdown)
                .await
            {
                Ok(GatewayExit::Shutdown) => return Ok(()),
                Ok(GatewayExit::Reconnect {
                    code,
                    clear_session,
                }) => {
                    if clear_session {
                        session.clear();
                    }
                    delay = self.config.reconnect_base_delay_ms;
                    tracing::warn!(?code, retry_in_ms = delay, "QQ Gateway 连接已断开");
                }
                Ok(GatewayExit::Fatal { code, reason }) => {
                    return Err(QqError::Gateway(format!(
                        "Gateway 返回不可恢复关闭码 {code}：{reason}"
                    )));
                }
                Err(error) => {
                    tracing::warn!(%error, retry_in_ms = delay, "QQ Gateway 连接失败");
                }
            }
            sleep_or_shutdown(delay, &shutdown).await;
            delay = doubled_delay(delay, self.config.reconnect_max_delay_ms);
        }
    }

    async fn connect_gateway(
        self: &Arc<Self>,
        gateway_url: &str,
        session: &mut GatewaySession,
        shutdown: &CancellationToken,
    ) -> Result<GatewayExit, QqError> {
        let connection = tokio::time::timeout(
            Duration::from_secs(self.config.request_timeout_secs),
            connect_async(gateway_url),
        )
        .await
        .map_err(|_| QqError::Gateway("建立 WebSocket 连接超时".into()))?
        .map_err(|error| QqError::Gateway(format!("建立 WebSocket 连接失败：{error}")))?;
        let (mut socket, _) = connection;
        tracing::info!("QQ Gateway WebSocket 已连接");

        let initial_heartbeat = Duration::from_secs(3_600);
        let mut heartbeat_interval = tokio::time::interval_at(
            tokio::time::Instant::now() + initial_heartbeat,
            initial_heartbeat,
        );
        let mut heartbeat_enabled = false;
        let mut heartbeat_acknowledged = true;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => {
                    let _ = socket.close(None).await;
                    return Ok(GatewayExit::Shutdown);
                }
                _ = heartbeat_interval.tick(), if heartbeat_enabled => {
                    if !heartbeat_acknowledged {
                        let _ = socket.close(None).await;
                        return Ok(GatewayExit::Reconnect {
                            code: Some(4000),
                            clear_session: false,
                        });
                    }
                    self.send_heartbeat(&mut socket, session, &mut heartbeat_acknowledged)
                        .await?;
                }
                frame = socket.next() => {
                    let Some(frame) = frame else {
                        return Ok(GatewayExit::Reconnect { code: None, clear_session: false });
                    };
                    match frame.map_err(|error| QqError::Gateway(format!("读取 Gateway 数据失败：{error}")))? {
                        Message::Text(text) => {
                            let payload: Value = serde_json::from_str(text.as_ref())
                                .map_err(|error| QqError::Gateway(format!("Gateway JSON 无效：{error}")))?;
                            if let Some(exit) = self.handle_gateway_payload(
                                &mut socket,
                                session,
                                &payload,
                                &mut heartbeat_interval,
                                &mut heartbeat_enabled,
                                &mut heartbeat_acknowledged,
                            ).await? {
                                return Ok(exit);
                            }
                        }
                        Message::Binary(bytes) => {
                            let payload: Value = serde_json::from_slice(&bytes)
                                .map_err(|error| QqError::Gateway(format!("Gateway JSON 无效：{error}")))?;
                            if let Some(exit) = self.handle_gateway_payload(
                                &mut socket,
                                session,
                                &payload,
                                &mut heartbeat_interval,
                                &mut heartbeat_enabled,
                                &mut heartbeat_acknowledged,
                            ).await? {
                                return Ok(exit);
                            }
                        }
                        Message::Ping(bytes) => {
                            socket.send(Message::Pong(bytes)).await
                                .map_err(|error| QqError::Gateway(format!("回复 Gateway Ping 失败：{error}")))?;
                        }
                        Message::Pong(_) | Message::Frame(_) => {}
                        Message::Close(frame) => {
                            let code = frame.map_or(1000, |frame| u16::from(frame.code));
                            let clear_session = matches!(code, 4006 | 4007);
                            if matches!(code, 4914 | 4915) {
                                return Ok(GatewayExit::Fatal {
                                    code,
                                    reason: "机器人账号或订阅权限不可用".into(),
                                });
                            }
                            return Ok(GatewayExit::Reconnect { code: Some(code), clear_session });
                        }
                    }
                }
            }
        }
    }

    async fn handle_gateway_payload<S>(
        self: &Arc<Self>,
        socket: &mut S,
        session: &mut GatewaySession,
        payload: &Value,
        heartbeat_interval: &mut tokio::time::Interval,
        heartbeat_enabled: &mut bool,
        heartbeat_acknowledged: &mut bool,
    ) -> Result<Option<GatewayExit>, QqError>
    where
        S: futures_util::Sink<Message> + Unpin,
        S::Error: std::fmt::Display,
    {
        let op = payload
            .get("op")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if op == 0 {
            session.sequence = payload.get("s").and_then(Value::as_u64);
        }
        match op {
            0 => {
                let event_type = payload.get("t").and_then(Value::as_str).unwrap_or_default();
                match event_type {
                    "READY" => {
                        session.session_id = payload
                            .get("d")
                            .and_then(|data| data.get("session_id"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        tracing::info!("QQ Gateway 鉴权成功");
                    }
                    "RESUMED" => tracing::info!("QQ Gateway 会话恢复成功"),
                    _ => {
                        let source = Arc::clone(self);
                        let payload = payload.clone();
                        tokio::spawn(async move {
                            source.handle_dispatch(&payload).await;
                        });
                    }
                }
            }
            7 => {
                let _ = socket.close().await;
                return Ok(Some(GatewayExit::Reconnect {
                    code: Some(4000),
                    clear_session: false,
                }));
            }
            9 => {
                let resumable = payload.get("d").and_then(Value::as_bool).unwrap_or(false);
                let _ = socket.close().await;
                return Ok(Some(GatewayExit::Reconnect {
                    code: Some(4000),
                    clear_session: !resumable,
                }));
            }
            1 => {
                self.send_heartbeat(socket, session, heartbeat_acknowledged)
                    .await?;
            }
            10 => {
                let interval = payload
                    .get("d")
                    .and_then(|data| data.get("heartbeat_interval"))
                    .and_then(value_u64)
                    .filter(|value| *value > 0)
                    .ok_or_else(|| QqError::Gateway("Hello 缺少有效 heartbeat_interval".into()))?;
                let interval = Duration::from_millis(interval);
                *heartbeat_interval =
                    tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
                *heartbeat_enabled = true;
                *heartbeat_acknowledged = true;
                self.authenticate(socket, session).await?;
            }
            11 => *heartbeat_acknowledged = true,
            _ => tracing::debug!(op, "忽略未处理的 QQ Gateway opcode"),
        }
        Ok(None)
    }

    async fn send_heartbeat<S>(
        &self,
        socket: &mut S,
        session: &GatewaySession,
        heartbeat_acknowledged: &mut bool,
    ) -> Result<(), QqError>
    where
        S: futures_util::Sink<Message> + Unpin,
        S::Error: std::fmt::Display,
    {
        let payload = serde_json::to_string(&json!({"op": 1, "d": session.sequence}))
            .map_err(|error| QqError::Gateway(format!("构造 Gateway 心跳消息失败：{error}")))?;
        socket
            .send(Message::Text(payload.into()))
            .await
            .map_err(|error| QqError::Gateway(format!("发送心跳失败：{error}")))?;
        *heartbeat_acknowledged = false;
        Ok(())
    }

    async fn authenticate<S>(&self, socket: &mut S, session: &GatewaySession) -> Result<(), QqError>
    where
        S: futures_util::Sink<Message> + Unpin,
        S::Error: std::fmt::Display,
    {
        let token = self.api.token_manager.get().await?;
        let payload = if let (Some(session_id), Some(sequence)) =
            (session.session_id.as_deref(), session.sequence)
        {
            json!({
                "op": 6,
                "d": {
                    "token": format!("QQBot {token}"),
                    "session_id": session_id,
                    "seq": sequence,
                }
            })
        } else {
            json!({
                "op": 2,
                "d": {
                    "token": format!("QQBot {token}"),
                    "intents": self.config.intents,
                    "shard": [0, 1],
                    "properties": {
                        "$os": std::env::consts::OS,
                        "$browser": "koi-rust-rv",
                        "$device": "koi-rust-rv",
                    }
                }
            })
        };
        let text = serde_json::to_string(&payload)
            .map_err(|error| QqError::Gateway(format!("构造 Gateway 鉴权消息失败：{error}")))?;
        socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|error| QqError::Gateway(format!("发送 Gateway 鉴权消息失败：{error}")))
    }

    async fn handle_dispatch(&self, payload: &Value) {
        let event_type = payload.get("t").and_then(Value::as_str).unwrap_or_default();
        if event_type == "GROUP_MESSAGE_CREATE" {
            let Some(group_openid) = payload
                .get("d")
                .and_then(|data| data.get("group_openid"))
                .and_then(Value::as_str)
            else {
                return;
            };
            let bot_member_openid = match self.group_bot_member_openid(group_openid).await {
                Ok(value) => Some(value),
                Err(error) => {
                    if self.config.mention_only {
                        tracing::warn!(%error, %group_openid, "无法确认 QQ 机器人群内身份，跳过未标记群消息");
                    } else {
                        tracing::warn!(%error, %group_openid, "无法确认 QQ 机器人群内身份，带机器人标记的权限可能无法识别");
                    }
                    None
                }
            };
            if let Some(message) = normalize_dispatch(
                payload,
                bot_member_openid.as_deref(),
                self.config.mention_only,
            ) {
                self.accept_message(message).await;
            }
            return;
        }
        if let Some(message) = normalize_dispatch(payload, None, self.config.mention_only) {
            self.accept_message(message).await;
        }
    }

    async fn group_bot_member_openid(&self, group_openid: &str) -> Result<String, QqError> {
        let cell = {
            let mut cache = self.group_bot_member_openids.lock().await;
            cache
                .entry(group_openid.into())
                .or_insert_with(|| Arc::new(OnceCell::new()))
                .clone()
        };
        let member_openid = cell
            .get_or_try_init(|| async { self.api.group_bot_state(group_openid).await })
            .await?;
        Ok(member_openid.clone())
    }

    async fn accept_message(&self, message: NormalizedMessage) {
        let is_new = self.dedup.lock().is_ok_and(|mut cache| {
            let message_is_new = cache.insert(format!("message:{}", message.message_id));
            let dispatch_is_new = message
                .dispatch_id
                .as_ref()
                .is_none_or(|dispatch_id| cache.insert(format!("dispatch:{dispatch_id}")));
            message_is_new && dispatch_is_new
        });
        if !is_new {
            return;
        }

        if message.explicit_bot_mention
            && let Some(result) = parse_confirmation_command(&message.text)
        {
            match result {
                Ok(command) => {
                    if let Err(error) = self.handle_confirmation(message.clone(), command).await {
                        tracing::warn!(%error, "处理 QQ 确认指令失败");
                        self.best_effort_notice(
                            &message.target,
                            "这条确认指令无法处理，请确认令牌、群聊和发送权限。",
                        )
                        .await;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "QQ 确认指令格式无效");
                    self.best_effort_notice(
                        &message.target,
                        "确认格式：@bot /confirm <token>，或 @bot /confirm all。",
                    )
                    .await;
                }
            }
            return;
        }

        if message.explicit_bot_mention
            && let Some(result) = parse_control_command(&message.text)
        {
            match result {
                Ok(command) => {
                    if let Err(error) = self.handle_control_command(message.clone(), command).await
                    {
                        tracing::warn!(%error, "处理 QQ 控制指令失败");
                        self.best_effort_notice(
                            &message.target,
                            "控制指令无法执行，请检查权限、命令格式和主会话状态。",
                        )
                        .await;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "QQ 控制指令格式无效");
                    self.best_effort_notice(
                        &message.target,
                        "控制指令格式：@bot /pause [原因]、@bot /resume、@bot /cancel [原因]（/abort、/stop 为别名）。",
                    )
                    .await;
                }
            }
            return;
        }

        if let Err(error) = self.ingest_message(message).await {
            tracing::warn!(%error, "写入 QQ 输入事件失败");
        }
    }

    async fn handle_control_command(
        &self,
        message: NormalizedMessage,
        command: ParsedControlCommand,
    ) -> Result<(), QqError> {
        if !matches!(&message.target, QqTarget::Group { .. }) {
            return Err(QqError::Event("QQ 控制指令只能在群聊中提交".into()));
        }

        let identity_permission = self
            .permissions
            .permission_for(QQ_SOURCE_NAME, &message.subject);
        let effective_permission = PermissionAssessment::new(
            PermissionLevel::Operator,
            self.sources
                .get(QQ_SOURCE_NAME)
                .map_or(PermissionLevel::None, |source| source.maximum_permission),
            identity_permission,
        )
        .effective_permission;
        if !effective_permission.allows(PermissionLevel::User) {
            return Err(QqError::Event(
                "QQ 身份没有执行控制指令所需的 User 权限".into(),
            ));
        }

        let (event, notice) = match command {
            ParsedControlCommand::Pause { reason } => (
                ControlEvent::PauseRequested {
                    reason: format_control_reason(&message, "pause", &reason),
                },
                "已提交暂停请求，当前执行会在安全点停止。",
            ),
            ParsedControlCommand::Resume => (
                ControlEvent::ResumeRequested,
                "已提交恢复请求，主会话将重新排队。",
            ),
            ParsedControlCommand::Cancel { reason } => (
                ControlEvent::TaskCancelled {
                    reason: format_control_reason(&message, "cancel", &reason),
                },
                "已提交中止请求。",
            ),
        };
        let principal = Principal {
            source: QQ_SOURCE_NAME.into(),
            subject: message.subject.clone(),
            display_name: message.display_name.clone(),
        };
        let authority = DirectControlAuthority::external(
            SourceName::new(QQ_SOURCE_NAME)
                .map_err(|error| QqError::Configuration(format!("QQ 来源名无效：{error}")))?,
            principal,
            effective_permission,
            None,
        )
        .map_err(|error| QqError::Event(error.to_string()))?;

        {
            let _guard = self.write_lock.lock().await;
            let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), TaskId::MAIN)
                .await
                .map_err(|error| QqError::Core(error.to_string()))?;
            ControlExecutor::execute(
                &mut runtime,
                ControlExecutionRequest {
                    event,
                    authority,
                    causation_id: None,
                },
            )
            .await
            .map_err(|error| QqError::Event(error.to_string()))?;
        }

        self.best_effort_notice(&message.target, notice).await;
        Ok(())
    }

    async fn ingest_message(&self, message: NormalizedMessage) -> Result<(), QqError> {
        if message.text.trim().is_empty() {
            return Ok(());
        }
        let _guard = self.write_lock.lock().await;
        // QQ 的所有入口共享主会话，避免群聊、C2C 与频道消息被拆到彼此隔离的子任务。
        let task_id = TaskId::MAIN;
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        let now = Utc::now();
        let principal = Principal {
            source: QQ_SOURCE_NAME.into(),
            subject: message.subject.clone(),
            display_name: message.display_name.clone(),
        };
        // scope/actor 是路由和权限判断的结构化事实，但不会自动出现在模型正文中。
        // 把来源头放进 Text payload，主会话汇总不同 QQ 会话时仍能让模型区分上下文。
        let text = format_qq_context_text(&message, &message.text);
        let context = ContextEnvelope {
            schema_version: 1,
            kind: ContextKind::UserMessage,
            origin: ContextOrigin {
                source: QQ_SOURCE_NAME.into(),
                source_instance: QQ_GATEWAY_INSTANCE.into(),
                native_event_id: message.message_id.clone(),
            },
            actor: Some(principal),
            scope: message.scope(),
            occurred_at: now,
            received_at: now,
            position: None,
            permission: PermissionLevel::None,
            payload: ContextPayload::Text {
                text: text.clone(),
                mentions: message.mentions.clone(),
            },
            causation_id: None,
            content_hash: fingerprint(&text),
        };
        IngressRegistrar::new(&self.sources, self.permissions.as_ref())
            .register(
                &mut runtime,
                IngressDraft::Context {
                    context: Box::new(context),
                    suggested_permission: if message.explicit_bot_mention {
                        PermissionLevel::Operator
                    } else {
                        PermissionLevel::User
                    },
                },
            )
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        Ok(())
    }

    async fn hydrate_message_dedup(&self) -> Result<(), QqError> {
        let task_ids = self
            .store
            .list_task_ids()
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        let mut persisted_message_ids = Vec::new();
        for task_id in task_ids {
            let events = self
                .store
                .load_task(task_id)
                .await
                .map_err(|error| QqError::Core(error.to_string()))?;
            for event in events {
                let Some(context) = context_from_event(&event) else {
                    continue;
                };
                if context.origin.source == QQ_SOURCE_NAME
                    && !context.origin.native_event_id.trim().is_empty()
                {
                    persisted_message_ids
                        .push((context.received_at, context.origin.native_event_id.clone()));
                }
            }
        }
        let mut dedup = self
            .dedup
            .lock()
            .map_err(|_| QqError::Core("QQ 去重缓存锁已中毒".into()))?;
        persisted_message_ids.sort_by_key(|(received_at, _)| *received_at);
        for (_, message_id) in persisted_message_ids {
            dedup.insert(format!("message:{message_id}"));
        }
        Ok(())
    }

    async fn handle_confirmation(
        &self,
        message: NormalizedMessage,
        command: ParsedConfirmationCommand,
    ) -> Result<(), QqError> {
        if !message.explicit_bot_mention {
            return Err(QqError::Event("确认指令必须明确 @bot".into()));
        }
        if !matches!(&message.target, QqTarget::Group { .. }) {
            return Err(QqError::Event("QQ 确认指令只能在群聊中提交".into()));
        }
        let identity_permission = self
            .permissions
            .permission_for(QQ_SOURCE_NAME, &message.subject);
        if !identity_permission.allows(PermissionLevel::Operator) {
            return Err(QqError::Event(
                "只有权限目录中的 Operator 或更高权限成员可以确认操作".into(),
            ));
        }

        let conversation_key = message.conversation_key();
        let guard = self.write_lock.lock().await;
        let (pending, approve_all) = match command {
            ParsedConfirmationCommand::One(request_event_id) => {
                let pending = self
                    .find_approval(request_event_id)
                    .await?
                    .ok_or_else(|| QqError::Event("确认令牌不存在或已经处理".into()))?;
                let target = QqTarget::from_context(&pending.original_context)
                    .ok_or_else(|| QqError::Event("确认令牌没有可用的 QQ 群聊上下文".into()))?;
                if target.conversation_key() != conversation_key
                    || !matches!(target, QqTarget::Group { .. })
                {
                    return Err(QqError::Event("确认令牌不属于当前 QQ 群聊".into()));
                }
                (vec![pending], false)
            }
            ParsedConfirmationCommand::All => {
                (self.find_pending_approvals(&conversation_key).await?, true)
            }
        };
        if pending.is_empty() {
            return Err(QqError::Event("当前 QQ 群没有待确认的操作".into()));
        }

        let approved_count = pending.len();
        for approval in pending {
            let grant = if approve_all {
                ApprovalGrant::AnyOperation
            } else {
                ApprovalGrant::CurrentOperation {
                    original_event_payload_id: approval.proposal_event_id,
                }
            };
            self.submit_confirmation(&message, approval, grant).await?;
        }
        drop(guard);

        let notice = if approve_all {
            format!("已确认当前 QQ 群的 {approved_count} 项待处理操作，Agent 将继续处理。")
        } else {
            "已确认该操作，Agent 将继续处理。".into()
        };
        self.best_effort_notice(&message.target, &notice).await;
        Ok(())
    }

    async fn submit_confirmation(
        &self,
        message: &NormalizedMessage,
        approval: PendingApproval,
        grant: ApprovalGrant,
    ) -> Result<(), QqError> {
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), approval.task_id)
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        let principal = Principal {
            source: QQ_SOURCE_NAME.into(),
            subject: message.subject.clone(),
            display_name: message.display_name.clone(),
        };
        IngressRegistrar::new(&self.sources, self.permissions.as_ref())
            .register(
                &mut runtime,
                IngressDraft::Approval {
                    approval_request_event_id: approval.request_event_id,
                    principal,
                    scope: approval.original_context.scope,
                    suggested_permission: PermissionLevel::Operator,
                    approved: true,
                    grant: Some(grant),
                },
            )
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        Ok(())
    }

    async fn find_approval(
        &self,
        request_event_id: EventId,
    ) -> Result<Option<PendingApproval>, QqError> {
        let task_ids = self
            .store
            .list_task_ids()
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        for task_id in task_ids {
            let events = self
                .store
                .load_task(task_id)
                .await
                .map_err(|error| QqError::Core(error.to_string()))?;
            if let Some(approval) = pending_approval_from_events(task_id, &events, request_event_id)
            {
                return Ok(Some(approval));
            }
        }
        Ok(None)
    }

    async fn find_pending_approvals(
        &self,
        conversation_key: &str,
    ) -> Result<Vec<PendingApproval>, QqError> {
        let task_ids = self
            .store
            .list_task_ids()
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        let mut approvals = Vec::new();
        for task_id in task_ids {
            let events = self
                .store
                .load_task(task_id)
                .await
                .map_err(|error| QqError::Core(error.to_string()))?;
            for event in &events {
                if !matches!(
                    &event.payload,
                    AgentEvent::Tool(tool)
                        if matches!(tool.as_ref(), ToolEvent::ApprovalRequested { .. })
                ) {
                    continue;
                }
                let Some(approval) = pending_approval_from_events(task_id, &events, event.id)
                else {
                    continue;
                };
                if QqTarget::from_context(&approval.original_context)
                    .is_some_and(|target| target.conversation_key() == conversation_key)
                {
                    approvals.push(approval);
                }
            }
        }
        Ok(approvals)
    }

    async fn authorization_target(
        &self,
        request: &AuthorizationRequest,
    ) -> Result<QqTarget, AuthorizationError> {
        let events = self
            .store
            .load_task(request.task_id)
            .await
            .map_err(|error| AuthorizationError::new(error.to_string()))?;
        let mut context = request
            .original_evidence_event_ids
            .iter()
            .find_map(|event_id| {
                events
                    .iter()
                    .find(|event| event.id == *event_id)
                    .and_then(context_from_event)
                    .filter(|context| context.origin.source == QQ_SOURCE_NAME)
            });
        if context.is_none() {
            context = events.iter().find_map(|event| {
                if event.id != request.approval_request_event_id {
                    return None;
                }
                let AgentEvent::Tool(tool) = &event.payload else {
                    return None;
                };
                let ToolEvent::ApprovalRequested { proposal_event_id } = tool.as_ref() else {
                    return None;
                };
                let parent = events.iter().find(|event| event.id == *proposal_event_id)?;
                let AgentEvent::Tool(tool) = &parent.payload else {
                    return None;
                };
                let ToolEvent::Proposed { tool_call } = tool.as_ref() else {
                    return None;
                };
                tool_call
                    .authority_parent_event_id
                    .and_then(|parent_id| events.iter().find(|event| event.id == parent_id))
                    .and_then(context_from_event)
                    .filter(|context| context.origin.source == QQ_SOURCE_NAME)
            });
        }
        let context = context
            .ok_or_else(|| AuthorizationError::new("找不到与授权请求绑定的 QQ 输入上下文"))?;
        QqTarget::from_context(context)
            .ok_or_else(|| AuthorizationError::new("QQ 输入上下文的回复目标无效"))
    }

    async fn send_target_with_receipts(
        &self,
        target: &QqTarget,
        text: &str,
    ) -> Result<Vec<String>, QqError> {
        let sequence_key = target.reply_sequence_key();
        let mut message_ids = Vec::new();
        for chunk in split_chars(text, self.config.max_reply_chars) {
            let msg_seq = self.next_outgoing_sequence(&sequence_key).await;
            let message_id = self.api.send_target(target, &chunk, msg_seq).await?;
            if !message_id.trim().is_empty() {
                message_ids.push(message_id);
            }
        }
        Ok(message_ids)
    }

    async fn send_target(&self, target: &QqTarget, text: &str) -> Result<(), QqError> {
        self.send_target_with_receipts(target, text)
            .await
            .map(|_| ())
    }

    /// 通过 QQ 官方群消息接口发送主动群聊消息或回复指定群消息。
    ///
    /// `reply_to_message_id` 为空时发送主动消息；有值时按官方 `msg_id`/`msg_seq`
    /// 规则回复该消息。长文本按来源配置拆分为多条消息。
    ///
    /// # Errors
    ///
    /// 当群标识、正文或回复目标无效，或 QQ API 调用失败时返回错误。
    pub(crate) async fn send_group_message(
        &self,
        group_openid: &str,
        content: &str,
        reply_to_message_id: Option<&str>,
    ) -> Result<QqGroupDelivery, QqError> {
        let group_openid = group_openid.trim();
        if group_openid.is_empty() {
            return Err(QqError::Configuration("QQ 群 group_openid 不能为空".into()));
        }
        if group_openid.chars().count() > MAX_GROUP_OPENID_CHARS {
            return Err(QqError::Configuration(format!(
                "QQ 群 group_openid 超过 {MAX_GROUP_OPENID_CHARS} 个字符"
            )));
        }
        let content = content.trim();
        if content.is_empty() {
            return Err(QqError::Configuration("QQ 群投送内容不能为空".into()));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(QqError::Configuration(format!(
                "QQ 群投送内容超过 {MAX_CONTENT_CHARS} 个字符"
            )));
        }
        let reply_to_message_id = reply_to_message_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if reply_to_message_id
            .as_deref()
            .is_some_and(|value| value.chars().count() > MAX_GROUP_OPENID_CHARS)
        {
            return Err(QqError::Configuration(format!(
                "QQ 回复目标消息 ID 超过 {MAX_GROUP_OPENID_CHARS} 个字符"
            )));
        }
        let target = QqTarget::Group {
            group_openid: group_openid.to_owned(),
            msg_id: reply_to_message_id.clone().unwrap_or_default(),
        };
        let chunks = split_chars(content, self.config.max_reply_chars).len();
        let message_ids = self.send_target_with_receipts(&target, content).await?;
        Ok(QqGroupDelivery {
            group_openid: group_openid.to_owned(),
            reply_to_message_id,
            message_ids,
            chunks,
        })
    }

    /// 向配置的主要汇报群发送一条模型主动汇报。
    ///
    /// 主要汇报群只由配置决定，模型不能在 `qq.report` 调用中改写目标群。
    ///
    /// # Errors
    ///
    /// 当未配置主要汇报群、汇报内容无效或 QQ API 调用失败时返回错误。
    pub(crate) async fn report_message(&self, content: &str) -> Result<QqGroupDelivery, QqError> {
        let group_openid = self.config.report_group_openid.as_deref().ok_or_else(|| {
            QqError::Configuration("QQ 未配置 report_group_openid，无法使用主要汇报群投送".into())
        })?;
        self.send_group_message(group_openid, content, None).await
    }

    /// 回复一个由模型授权父事件选中的 QQ 入站消息。
    ///
    /// 目标从已持久化的 QQ 上下文恢复，调用参数只有正文，避免模型借助普通回复
    /// 工具把消息改投到另一个群聊。核心仍会先校验 `context_event_id` 的权限证据。
    ///
    /// # Errors
    ///
    /// 当授权父事件不是当前任务中已持久化的 QQ 用户消息、正文无效或 QQ API 调用失败
    /// 时返回错误。
    pub(crate) async fn reply_to_context(
        &self,
        task_id: TaskId,
        context_event_id: EventId,
        content: &str,
    ) -> Result<QqReplyDelivery, QqError> {
        if !task_id.is_main() {
            return Err(QqError::Event("QQ 回复工具只能在主会话中使用".into()));
        }
        let content = content.trim();
        if content.is_empty() {
            return Err(QqError::Configuration("QQ 回复内容不能为空".into()));
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(QqError::Configuration(format!(
                "QQ 回复内容超过 {MAX_CONTENT_CHARS} 个字符"
            )));
        }
        let events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| QqError::Core(error.to_string()))?;
        let context = events
            .iter()
            .find(|event| event.id == context_event_id)
            .and_then(context_from_event)
            .filter(|context| {
                context.origin.source == QQ_SOURCE_NAME && context.kind == ContextKind::UserMessage
            })
            .ok_or_else(|| {
                QqError::Event("QQ 回复必须引用当前任务中已持久化的 QQ 用户消息".into())
            })?;
        let target = QqTarget::from_context(context)
            .ok_or_else(|| QqError::Event("QQ 回复上下文的投送目标无效".into()))?;
        let scope = target.scope();
        let reply_to_message_id = target.message_id().to_owned();
        let chunks = split_chars(content, self.config.max_reply_chars).len();
        let message_ids = self.send_target_with_receipts(&target, content).await?;
        Ok(QqReplyDelivery {
            scope,
            reply_to_message_id,
            message_ids,
            chunks,
        })
    }

    async fn best_effort_notice(&self, target: &QqTarget, text: &str) {
        if let Err(error) = self.send_target(target, text).await {
            tracing::warn!(%error, "发送 QQ 提示消息失败");
        }
    }

    async fn next_outgoing_sequence(&self, key: &str) -> u32 {
        let mut sequences = self.outgoing_sequences.lock().await;
        let sequence = sequences.entry(key.into()).or_default();
        *sequence = sequence.saturating_add(1);
        if *sequence == 0 {
            *sequence = 1;
        }
        *sequence
    }
}

struct QqAuthorizationProvider {
    source: Arc<QqSource>,
}

#[async_trait]
impl SourceAuthorizationProvider for QqAuthorizationProvider {
    fn source(&self) -> &'static str {
        QQ_SOURCE_NAME
    }

    async fn request_authorization(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationRequestResult, AuthorizationError> {
        if !PermissionLevel::Operator.allows(request.required_permission) {
            return Ok(AuthorizationRequestResult::Denied {
                reason: "QQ 来源最高只支持 Operator，不能确认 Admin 或 System 操作".into(),
            });
        }
        let target = self.source.authorization_target(&request).await?;
        if !matches!(&target, QqTarget::Group { .. }) {
            return Ok(AuthorizationRequestResult::Denied {
                reason: "QQ 提权确认仅支持群聊中的 @bot /confirm 指令".into(),
            });
        }
        let message = format!(
            "【Koi 提权请求】\n操作需要 {:?} 权限。\n工具：{}\n参数指纹：{}\n确认 token：{}\n请由有权限成员在本群 @bot 后回复：\n/confirm {}（仅确认本操作）\n/confirm all（确认本群全部待处理操作）",
            request.required_permission,
            request.tool_name,
            request.arguments_hash,
            request.approval_request_event_id,
            request.approval_request_event_id,
        );
        self.source
            .send_target(&target, &message)
            .await
            .map_err(|error| AuthorizationError::new(error.to_string()))?;
        Ok(AuthorizationRequestResult::Pending)
    }
}

#[derive(Default)]
struct GatewaySession {
    session_id: Option<String>,
    sequence: Option<u64>,
}

impl GatewaySession {
    fn clear(&mut self) {
        self.session_id = None;
        self.sequence = None;
    }
}

enum GatewayExit {
    Shutdown,
    Reconnect {
        code: Option<u16>,
        clear_session: bool,
    },
    Fatal {
        code: u16,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParsedConfirmationCommand {
    One(EventId),
    All,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParsedControlCommand {
    Pause { reason: String },
    Resume,
    Cancel { reason: String },
}

fn parse_confirmation_command(text: &str) -> Option<Result<ParsedConfirmationCommand, QqError>> {
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if !parts
        .first()
        .is_some_and(|command| command.eq_ignore_ascii_case("/confirm"))
    {
        return None;
    }
    let [_, token] = parts.as_slice() else {
        return Some(Err(QqError::Event(
            "确认格式必须是 /confirm <token> 或 /confirm all".into(),
        )));
    };
    if token.eq_ignore_ascii_case("all") {
        return Some(Ok(ParsedConfirmationCommand::All));
    }
    Some(
        token
            .parse::<uuid::Uuid>()
            .map(EventId)
            .map(ParsedConfirmationCommand::One)
            .map_err(|_| QqError::Event("确认 token 不是有效的事件 ID".into())),
    )
}

fn parse_control_command(text: &str) -> Option<Result<ParsedControlCommand, QqError>> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if !command.starts_with('/') {
        return None;
    }
    let argument = parts.collect::<Vec<_>>().join(" ");
    let argument_too_long = argument.chars().count() > MAX_CONTROL_REASON_CHARS;
    let result = if command.eq_ignore_ascii_case("/pause") {
        if argument_too_long {
            Err(QqError::Event(format!(
                "QQ pause 原因超过 {MAX_CONTROL_REASON_CHARS} 个字符"
            )))
        } else {
            Ok(ParsedControlCommand::Pause { reason: argument })
        }
    } else if command.eq_ignore_ascii_case("/resume") {
        if argument.is_empty() {
            Ok(ParsedControlCommand::Resume)
        } else {
            Err(QqError::Event("QQ resume 不接受额外参数".into()))
        }
    } else if command.eq_ignore_ascii_case("/cancel")
        || command.eq_ignore_ascii_case("/abort")
        || command.eq_ignore_ascii_case("/stop")
    {
        if argument_too_long {
            Err(QqError::Event(format!(
                "QQ cancel 原因超过 {MAX_CONTROL_REASON_CHARS} 个字符"
            )))
        } else {
            Ok(ParsedControlCommand::Cancel { reason: argument })
        }
    } else {
        Err(QqError::Event(format!("未知 QQ 控制指令 {command}")))
    };
    Some(result)
}

fn format_control_reason(message: &NormalizedMessage, command: &str, reason: &str) -> String {
    let scope = message.scope();
    let prefix = format!(
        "QQ {}:{}（qq:{}） @bot /{command}",
        scope.kind, scope.id, message.subject
    );
    let reason = reason.trim();
    let full = if reason.is_empty() {
        prefix
    } else {
        format!("{prefix}：{reason}")
    };
    truncate_chars(&full, MAX_CONTROL_REASON_CHARS)
}

#[allow(clippy::too_many_lines)]
fn normalize_dispatch(
    payload: &Value,
    bot_member_openid: Option<&str>,
    mention_only: bool,
) -> Option<NormalizedMessage> {
    if payload.get("op").and_then(Value::as_u64) != Some(0) {
        return None;
    }
    let event_type = payload.get("t").and_then(Value::as_str)?;
    if !matches!(
        event_type,
        "C2C_MESSAGE_CREATE"
            | "GROUP_AT_MESSAGE_CREATE"
            | "GROUP_MESSAGE_CREATE"
            | "AT_MESSAGE_CREATE"
            | "DIRECT_MESSAGE_CREATE"
    ) {
        return None;
    }
    let message = payload.get("d")?;
    let message_id = string_value(message.get("id"))?;
    let author = message.get("author").unwrap_or(&Value::Null);
    if author.get("bot").and_then(Value::as_bool).unwrap_or(false)
        || author
            .get("is_bot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }

    let mentions = normalize_mentions(message.get("mentions"));
    let reference_index = find_reference_index(message);
    let raw_content = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map_or_else(
            || collect_current_element_text(message, reference_index.as_deref()),
            str::to_owned,
        );
    let cleaned_content = strip_mention_markers(&raw_content);
    let explicit_bot_mention = match event_type {
        "GROUP_AT_MESSAGE_CREATE" | "AT_MESSAGE_CREATE" => true,
        "GROUP_MESSAGE_CREATE" => {
            mentions.iter().any(|mention| {
                mention == "__koi_bot__" || bot_member_openid.is_some_and(|bot| mention == bot)
            }) || bot_member_openid.is_some_and(|bot| contains_mention_marker(&raw_content, bot))
        }
        _ => false,
    };
    if event_type == "GROUP_MESSAGE_CREATE" && mention_only && !explicit_bot_mention {
        return None;
    }

    let (subject, display_name, target) = match event_type {
        "C2C_MESSAGE_CREATE" => {
            let subject = first_string(author, &["user_openid", "openid", "id"])?;
            (
                subject.clone(),
                first_string(author, &["username", "nickname"]),
                QqTarget::C2c {
                    user_openid: subject,
                    msg_id: message_id.clone(),
                },
            )
        }
        "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" => {
            let group_openid = string_value(message.get("group_openid"))?;
            let subject = first_string(author, &["member_openid", "user_openid", "id"])?;
            (
                subject.clone(),
                first_string(author, &["username", "nickname"]),
                QqTarget::Group {
                    group_openid,
                    msg_id: message_id.clone(),
                },
            )
        }
        "AT_MESSAGE_CREATE" => {
            let channel_id = string_value(message.get("channel_id"))?;
            let subject = first_string(author, &["id", "user_openid"])?;
            (
                subject.clone(),
                first_string(author, &["username", "nickname"]),
                QqTarget::Channel {
                    channel_id,
                    msg_id: message_id.clone(),
                },
            )
        }
        "DIRECT_MESSAGE_CREATE" => {
            let guild_id = string_value(message.get("guild_id"))?;
            let subject = first_string(author, &["id", "user_openid"])?;
            (
                subject.clone(),
                first_string(author, &["username", "nickname"]),
                QqTarget::Direct {
                    guild_id,
                    msg_id: message_id.clone(),
                },
            )
        }
        _ => return None,
    };
    let text = if cleaned_content.is_empty() && explicit_bot_mention {
        "（用户提及了机器人）".into()
    } else {
        cleaned_content
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(NormalizedMessage {
        message_id,
        dispatch_id: string_value(payload.get("id")),
        target,
        subject,
        display_name,
        text,
        mentions,
        explicit_bot_mention,
    })
}

fn pending_approval_from_events(
    task_id: TaskId,
    events: &[EventEnvelope],
    request_event_id: EventId,
) -> Option<PendingApproval> {
    let proposal_event_id = events.iter().find_map(|event| {
        if event.id != request_event_id {
            return None;
        }
        let AgentEvent::Tool(tool) = &event.payload else {
            return None;
        };
        match tool.as_ref() {
            ToolEvent::ApprovalRequested { proposal_event_id } => Some(*proposal_event_id),
            _ => None,
        }
    })?;
    if approval_decision(events, request_event_id, proposal_event_id).is_some() {
        return None;
    }

    let authority_parent = events.iter().find_map(|event| {
        if event.id != proposal_event_id {
            return None;
        }
        let AgentEvent::Tool(tool) = &event.payload else {
            return None;
        };
        match tool.as_ref() {
            ToolEvent::Proposed { tool_call } => tool_call.authority_parent_event_id,
            _ => None,
        }
    });
    let context = authority_parent
        .and_then(|parent| events.iter().find(|event| event.id == parent))
        .and_then(context_from_event)
        .cloned()
        .or_else(|| {
            events
                .iter()
                .rev()
                .filter(|event| {
                    event.sequence <= request_event_id_sequence(events, request_event_id)
                })
                .find_map(|event| {
                    context_from_event(event)
                        .filter(|context| context.origin.source == QQ_SOURCE_NAME)
                })
                .cloned()
        })?;
    Some(PendingApproval {
        task_id,
        request_event_id,
        proposal_event_id,
        original_context: context,
    })
}

fn approval_decision(
    events: &[EventEnvelope],
    approval_request_event_id: EventId,
    proposal_event_id: EventId,
) -> Option<bool> {
    events.iter().rev().find_map(|event| match &event.payload {
        AgentEvent::Ingress(ingress) => match ingress.as_ref() {
            IngressEvent::ApprovalSubmitted {
                approval_request_event_id: submitted_request_event_id,
                approved,
                ..
            } if *submitted_request_event_id == approval_request_event_id => Some(*approved),
            _ => None,
        },
        AgentEvent::Tool(tool) => match tool.as_ref() {
            ToolEvent::AuthorizationChecked {
                proposal_event_id: checked_proposal_event_id,
                decision: PolicyDecision::Deny,
                ..
            } if *checked_proposal_event_id == proposal_event_id => Some(false),
            _ => None,
        },
        _ => None,
    })
}

fn context_from_event(event: &EventEnvelope) -> Option<&ContextEnvelope> {
    let AgentEvent::Ingress(ingress) = &event.payload else {
        return None;
    };
    match ingress.as_ref() {
        IngressEvent::ContextReceived { context, .. } => Some(context),
        _ => None,
    }
}

fn request_event_id_sequence(events: &[EventEnvelope], event_id: EventId) -> u64 {
    events
        .iter()
        .find(|event| event.id == event_id)
        .map_or(u64::MAX, |event| event.sequence)
}

fn normalize_mentions(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|mention| {
            if mention
                .get("is_you")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Some("__koi_bot__".into());
            }
            first_string(mention, &["member_openid", "user_openid", "id"])
        })
        .collect()
}

fn find_message_index(message: &Value) -> Option<String> {
    string_value(message.get("msg_idx"))
        .or_else(|| string_value(message.get("msg_seq")))
        .or_else(|| find_scene_value(message, "msg_idx"))
}

fn find_reference_index(message: &Value) -> Option<String> {
    string_value(message.get("ref_msg_idx"))
        .or_else(|| {
            message
                .get("message_reference")
                .and_then(|reference| string_value(reference.get("message_id")))
        })
        .or_else(|| {
            message
                .get("reply")
                .and_then(|reply| string_value(reply.get("message_id")))
        })
        .or_else(|| find_scene_value(message, "ref_msg_idx"))
}

fn find_scene_value(message: &Value, key: &str) -> Option<String> {
    let equal_prefix = format!("{key}=");
    let colon_prefix = format!("{key}:");
    message
        .get("message_scene")
        .and_then(|scene| scene.get("ext"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|entry| {
            entry
                .strip_prefix(&equal_prefix)
                .or_else(|| entry.strip_prefix(&colon_prefix))
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
        })
}

fn find_element_by_index<'a>(elements: Option<&'a Value>, index: &str) -> Option<&'a Value> {
    let elements = elements?.as_array()?;
    for element in elements {
        if string_value(element.get("msg_idx")).as_deref() == Some(index) {
            return Some(element);
        }
        if let Some(nested) = find_element_by_index(element.get("msg_elements"), index) {
            return Some(nested);
        }
    }
    None
}

fn collect_current_element_text(message: &Value, reference_index: Option<&str>) -> String {
    let current_index = find_message_index(message);
    if let Some(current_index) = current_index.as_deref()
        && reference_index != Some(current_index)
        && let Some(current) = find_element_by_index(message.get("msg_elements"), current_index)
    {
        return collect_element_text(Some(current));
    }

    let Some(elements) = message.get("msg_elements").and_then(Value::as_array) else {
        return String::new();
    };
    elements
        .iter()
        .filter(|element| {
            reference_index.is_none_or(|reference| {
                string_value(element.get("msg_idx")).as_deref() != Some(reference)
            })
        })
        .map(|element| collect_element_text(Some(element)))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_element_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(content) = value.get("content").and_then(Value::as_str) {
        return content.to_owned();
    }
    if let Some(elements) = value.get("msg_elements") {
        return collect_element_text(Some(elements));
    }
    if let Some(array) = value.as_array() {
        return array
            .iter()
            .map(|element| {
                element.get("content").and_then(Value::as_str).map_or_else(
                    || collect_element_text(element.get("msg_elements")),
                    str::to_owned,
                )
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn strip_mention_markers(value: &str) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '<' {
            let Some(end_offset) = chars[index..]
                .iter()
                .position(|character| *character == '>')
            else {
                output.push(chars[index]);
                index += 1;
                continue;
            };
            let end = index + end_offset;
            if chars.get(index + 1) == Some(&'@') {
                index = end + 1;
                continue;
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_mention_marker(content: &str, member_openid: &str) -> bool {
    [
        format!("<@{member_openid}>"),
        format!("<@!{member_openid}>"),
    ]
    .iter()
    .any(|marker| content.contains(marker))
}

/// 为注入主会话的 QQ 消息生成模型可见的来源头。
///
/// 这个头只是上下文标记，不参与权限或回复目标判断；后两者始终使用
/// `ContextEnvelope.scope`、`actor` 和 `origin` 中的结构化值。来源字段与昵称都经过
/// 单行清洗，避免外部昵称把下一条消息伪装成来源头；正文仍按 QQ 消息原样保留。
fn format_qq_context_text(message: &NormalizedMessage, body: &str) -> String {
    let scope = message.scope();
    let scope_id = sanitize_context_label(&scope.id, MAX_CONTEXT_LABEL_VALUE_CHARS);
    let subject = sanitize_context_label(&message.subject, MAX_CONTEXT_LABEL_VALUE_CHARS);
    let message_id = sanitize_context_label(&message.message_id, MAX_CONTEXT_LABEL_VALUE_CHARS);
    let display_name = message
        .display_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .map(|name| sanitize_context_label(name, MAX_CONTEXT_DISPLAY_NAME_CHARS))
        .unwrap_or_else(|| "未提供".into());
    let mention_mode = if message.explicit_bot_mention {
        "是"
    } else {
        "否"
    };
    let header = format!(
        "【QQ来源｜类型={}｜会话={}:{}｜发言人={}（qq:{}）｜消息ID={}｜明确@bot={}】\nQQ消息正文：\n",
        message.target.context_kind_label(),
        scope.kind,
        scope_id,
        display_name,
        subject,
        message_id,
        mention_mode,
    );
    let body_budget = MAX_CONTENT_CHARS.saturating_sub(header.chars().count());
    format!("{header}{}", truncate_chars(body, body_budget))
}

fn sanitize_context_label(value: &str, max_chars: usize) -> String {
    let sanitized = value
        .chars()
        .take(max_chars)
        .map(|character| match character {
            '\r' | '\n' | '\t' | '|' | '【' | '】' => ' ',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect::<String>();
    let trimmed = sanitized.trim();
    if trimmed.is_empty() {
        "未提供".into()
    } else {
        trimmed.into()
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|value| string_value(Some(value))))
}

fn string_value(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.to_owned()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn split_chars(value: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }
    let chars = value.chars().collect::<Vec<_>>();
    chars
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn fingerprint(value: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim().is_empty())
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0f));
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        _ => char::from(b'A' + value - 10),
    }
}

async fn read_json_body(response: reqwest::Response) -> Result<Value, QqError> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| QqError::Network(format!("读取 QQ API 响应失败：{error}")))?;
    if bytes.len() > MAX_HTTP_BODY_BYTES {
        return Err(QqError::Api {
            status: status.as_u16(),
            code: None,
            message: "QQ API 响应超过大小限制".into(),
        });
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    match serde_json::from_slice(&bytes) {
        Ok(body) => Ok(body),
        Err(_) => Ok(json!({
            "raw": truncate_chars(&String::from_utf8_lossy(&bytes), MAX_ERROR_TEXT_CHARS),
        })),
    }
}

fn api_error_code(body: &Value) -> Option<i64> {
    body.get("code")
        .or_else(|| body.get("err_code"))
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn response_error_message(body: &Value) -> String {
    let message = body
        .get("message")
        .or_else(|| body.get("msg"))
        .or_else(|| body.get("error"))
        .or_else(|| body.get("raw"))
        .and_then(Value::as_str)
        .unwrap_or("QQ API 返回错误");
    truncate_chars(message, MAX_ERROR_TEXT_CHARS)
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response.headers().get("retry-after")?.to_str().ok()?;
    value.parse::<u64>().ok().map(Duration::from_secs)
}

async fn sleep_retry(attempt: u32, retry_after: Option<Duration>) {
    let delay = retry_after.unwrap_or_else(|| {
        Duration::from_millis((500_u64.saturating_mul(2_u64.saturating_pow(attempt))).min(5_000))
    });
    tokio::time::sleep(delay).await;
}

async fn sleep_or_shutdown(milliseconds: u64, shutdown: &CancellationToken) {
    tokio::select! {
        () = shutdown.cancelled() => {}
        () = tokio::time::sleep(Duration::from_millis(milliseconds)) => {}
    }
}

fn doubled_delay(current: u64, maximum: u64) -> u64 {
    current.saturating_mul(2).min(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(event_type: &str, data: &Value) -> Value {
        json!({"op": 0, "s": 1, "t": event_type, "id": "dispatch-1", "d": data})
    }

    #[test]
    fn config_defaults_to_official_api_and_group_c2c_intent() {
        let config = QqConfig::default();
        assert_eq!(config.api_base_url, "https://api.sgroup.qq.com");
        assert_eq!(config.intents, 1 << 25);
        assert!(config.mention_only);
        assert_eq!(config.report_group_openid, None);
        assert_eq!(config.default_permission, PermissionLevel::User);
        assert!(!config.is_configured());
    }

    #[test]
    fn rejects_non_user_default_permission() {
        let config = QqConfig {
            app_id: Some("app-id".into()),
            app_secret: Some("app-secret".into()),
            default_permission: PermissionLevel::Admin,
            ..QqConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_invalid_report_group_configuration() {
        let empty = QqConfig {
            app_id: Some("app-id".into()),
            app_secret: Some("app-secret".into()),
            report_group_openid: Some("  ".into()),
            ..QqConfig::default()
        };
        assert!(empty.validate().is_err());

        let oversized = QqConfig {
            app_id: Some("app-id".into()),
            app_secret: Some("app-secret".into()),
            report_group_openid: Some("x".repeat(MAX_GROUP_OPENID_CHARS + 1)),
            ..QqConfig::default()
        };
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn normalizes_c2c_message() {
        let message = normalize_dispatch(
            &payload(
                "C2C_MESSAGE_CREATE",
                &json!({
                    "id": "msg-1",
                    "content": "  hello   koi ",
                    "author": {"user_openid": "user-1", "username": "Alice"}
                }),
            ),
            None,
            true,
        )
        .expect("message should normalize");
        assert_eq!(message.subject, "user-1");
        assert_eq!(message.text, "hello koi");
        assert_eq!(message.conversation_key(), "c2c:user-1");
        assert!(!message.explicit_bot_mention);
    }

    #[test]
    fn labels_qq_message_sources_for_cross_conversation_context() {
        let message = NormalizedMessage {
            message_id: "group-message-1".into(),
            dispatch_id: Some("dispatch-1".into()),
            target: QqTarget::Group {
                group_openid: "group-1".into(),
                msg_id: "group-message-1".into(),
            },
            subject: "member-1".into(),
            display_name: Some("Alice".into()),
            text: "请查询状态".into(),
            mentions: Vec::new(),
            explicit_bot_mention: false,
        };
        let labeled = format_qq_context_text(&message, &message.text);
        assert!(labeled.contains("类型=群聊"));
        assert!(labeled.contains("会话=qq_group:group-1"));
        assert!(labeled.contains("发言人=Alice（qq:member-1）"));
        assert!(labeled.contains("消息ID=group-message-1"));
        assert!(labeled.contains("明确@bot=否"));
        assert!(labeled.ends_with("QQ消息正文：\n请查询状态"));

        let private = NormalizedMessage {
            target: QqTarget::C2c {
                user_openid: "user-1".into(),
                msg_id: "private-message-1".into(),
            },
            message_id: "private-message-1".into(),
            subject: "user-1".into(),
            display_name: None,
            text: "私聊内容".into(),
            ..message
        };
        let private_labeled = format_qq_context_text(&private, &private.text);
        assert!(private_labeled.contains("类型=私聊（C2C）"));
        assert!(private_labeled.contains("会话=qq_c2c:user-1"));
        assert!(private_labeled.contains("发言人=未提供（qq:user-1）"));
    }

    #[test]
    fn qq_source_label_keeps_model_content_within_injection_limit() {
        let message = NormalizedMessage {
            message_id: "message-1".into(),
            dispatch_id: None,
            target: QqTarget::Channel {
                channel_id: "channel-1".into(),
                msg_id: "message-1".into(),
            },
            subject: "member-1".into(),
            display_name: Some("A\nmalicious|nickname".into()),
            text: "正文".into(),
            mentions: Vec::new(),
            explicit_bot_mention: true,
        };
        let labeled = format_qq_context_text(&message, &"x".repeat(MAX_CONTENT_CHARS));
        assert!(labeled.chars().count() <= MAX_CONTENT_CHARS);
        assert!(labeled.contains("A malicious nickname"));
        assert!(labeled.contains("类型=频道"));
        assert!(labeled.contains("明确@bot=是"));
    }

    #[test]
    fn group_message_requires_current_bot_mention_when_enabled() {
        let data = json!({
            "id": "msg-1",
            "group_openid": "group-1",
            "content": "hello",
            "author": {"member_openid": "member-1"},
            "mentions": []
        });
        assert!(
            normalize_dispatch(&payload("GROUP_MESSAGE_CREATE", &data), Some("bot-1"), true)
                .is_none()
        );
        let mentioned = normalize_dispatch(
            &payload(
                "GROUP_MESSAGE_CREATE",
                &json!({
                    "id": "msg-2",
                    "group_openid": "group-1",
                    "content": "<@!bot-1> hello",
                    "author": {"member_openid": "member-1"},
                    "mentions": [{"member_openid": "bot-1"}]
                }),
            ),
            Some("bot-1"),
            true,
        )
        .expect("mentioned message should normalize");
        assert_eq!(mentioned.text, "hello");
        assert!(mentioned.explicit_bot_mention);

        let mentioned_when_not_filtering = normalize_dispatch(
            &payload(
                "GROUP_MESSAGE_CREATE",
                &json!({
                    "id": "msg-3",
                    "group_openid": "group-1",
                    "content": "<@!bot-1> operator request",
                    "author": {"member_openid": "member-1"},
                    "mentions": [{"member_openid": "bot-1"}]
                }),
            ),
            Some("bot-1"),
            false,
        )
        .expect("mentioned message should normalize when filtering is disabled");
        assert_eq!(mentioned_when_not_filtering.text, "operator request");
        assert!(mentioned_when_not_filtering.explicit_bot_mention);
    }

    #[test]
    fn extracts_current_message_element_without_quoted_reference() {
        let message = normalize_dispatch(
            &payload(
                "GROUP_AT_MESSAGE_CREATE",
                &json!({
                    "id": "msg-3",
                    "group_openid": "group-1",
                    "msg_idx": "2",
                    "ref_msg_idx": "1",
                    "content": "",
                    "author": {"member_openid": "member-1"},
                    "msg_elements": [
                        {"msg_idx": "1", "content": "quoted text"},
                        {"msg_idx": "2", "content": "current text"}
                    ]
                }),
            ),
            None,
            true,
        )
        .expect("message should normalize");
        assert_eq!(message.text, "current text");
        assert!(message.explicit_bot_mention);
    }

    #[test]
    fn confirmation_commands_require_the_new_token_format() {
        let id = uuid::Uuid::now_v7();
        let command = parse_confirmation_command(&format!("/confirm {id}"))
            .expect("command")
            .expect("valid confirmation");
        assert_eq!(command, ParsedConfirmationCommand::One(EventId(id)));
        assert_eq!(
            parse_confirmation_command("/confirm all")
                .expect("command")
                .expect("valid all confirmation"),
            ParsedConfirmationCommand::All
        );
        assert_eq!(
            parse_confirmation_command("/confirm ALL")
                .expect("command")
                .expect("valid all confirmation"),
            ParsedConfirmationCommand::All
        );
        assert!(
            parse_confirmation_command("/koi approve token").is_none(),
            "legacy commands are not confirmation commands"
        );
        assert!(
            parse_confirmation_command("拒绝 token").is_none(),
            "free-form denial is not a confirmation command"
        );
        assert!(
            parse_confirmation_command("/confirm unknown")
                .expect("command")
                .is_err()
        );
        assert!(
            parse_confirmation_command("/confirm")
                .expect("command")
                .is_err()
        );
        assert!(parse_confirmation_command("普通消息").is_none());
    }

    #[test]
    fn at_message_events_are_operator_candidates_but_direct_messages_are_not() {
        let at_message = normalize_dispatch(
            &payload(
                "AT_MESSAGE_CREATE",
                &json!({
                    "id": "channel-msg-1",
                    "channel_id": "channel-1",
                    "content": "hello",
                    "author": {"id": "member-1"}
                }),
            ),
            None,
            true,
        )
        .expect("channel mention should normalize");
        assert!(at_message.explicit_bot_mention);

        let direct_message = normalize_dispatch(
            &payload(
                "DIRECT_MESSAGE_CREATE",
                &json!({
                    "id": "direct-msg-1",
                    "guild_id": "guild-1",
                    "content": "hello",
                    "author": {"id": "member-1"}
                }),
            ),
            None,
            true,
        )
        .expect("direct message should normalize");
        assert!(!direct_message.explicit_bot_mention);
    }

    #[test]
    fn old_approval_aliases_are_not_parsed() {
        let id = uuid::Uuid::now_v7();
        assert!(parse_confirmation_command(&format!("/koi approve {id}")).is_none());
        assert!(parse_confirmation_command(&format!("拒绝 {id}")).is_none());
    }

    #[test]
    fn confirmation_token_is_case_insensitive_only_for_command_name() {
        let id = uuid::Uuid::now_v7();
        let command = parse_confirmation_command(&format!("/CoNfIrM {id}"))
            .expect("command")
            .expect("valid confirmation");
        assert_eq!(command, ParsedConfirmationCommand::One(EventId(id)));
    }

    #[test]
    fn qq_control_commands_are_explicit_and_not_user_messages() {
        assert_eq!(
            parse_control_command("/pause 等待人工确认")
                .expect("pause command")
                .expect("valid pause"),
            ParsedControlCommand::Pause {
                reason: "等待人工确认".into()
            }
        );
        assert_eq!(
            parse_control_command("/resume")
                .expect("resume command")
                .expect("valid resume"),
            ParsedControlCommand::Resume
        );
        assert_eq!(
            parse_control_command("/abort stop now")
                .expect("abort command")
                .expect("valid abort"),
            ParsedControlCommand::Cancel {
                reason: "stop now".into()
            }
        );
        assert_eq!(
            parse_control_command("/stop")
                .expect("stop command")
                .expect("valid stop"),
            ParsedControlCommand::Cancel {
                reason: String::new()
            }
        );
        assert!(parse_control_command("/resume now").unwrap().is_err());
        assert!(parse_control_command("/unknown").unwrap().is_err());
        assert!(parse_control_command("普通群消息").is_none());
    }

    #[test]
    fn qq_control_reason_keeps_scope_and_is_bounded() {
        let message = NormalizedMessage {
            message_id: "message-1".into(),
            dispatch_id: None,
            target: QqTarget::Group {
                group_openid: "group-1".into(),
                msg_id: "message-1".into(),
            },
            subject: "member-1".into(),
            display_name: Some("Alice".into()),
            text: "/pause 等待".into(),
            mentions: Vec::new(),
            explicit_bot_mention: true,
        };
        let reason = format_control_reason(&message, "pause", &"x".repeat(1_000));
        assert!(reason.chars().count() <= MAX_CONTROL_REASON_CHARS);
        assert!(reason.contains("qq_group:group-1"));
        assert!(reason.contains("@bot /pause"));
    }

    #[test]
    fn splits_reply_without_breaking_utf8_characters() {
        assert_eq!(split_chars("你好世界", 2), vec!["你好", "世界"]);
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(encode_path_segment("a/b c"), "a%2Fb%20c");
    }

    #[tokio::test]
    async fn ingests_all_qq_messages_into_the_main_task() {
        let directory = std::env::temp_dir().join(format!("koi-qq-source-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).expect("event store"));
        let mut main = TaskRuntime::new(Arc::clone(&store), TaskId::MAIN);
        main.record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .expect("main task created");
        main.record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
            None,
        )
        .await
        .expect("main task queued");

        let permissions = Arc::new(StaticPermissionDirectory::new(
            [("qq".to_owned(), PermissionLevel::User)],
            [(
                "qq".to_owned(),
                "operator-1".to_owned(),
                PermissionLevel::Operator,
            )],
        ));
        let task_manager = Arc::new(TaskManager::new(Arc::new(Arc::clone(&store))));
        let source = QqSource::new(
            QqConfig {
                app_id: Some("app-id".into()),
                app_secret: Some("app-secret".into()),
                ..QqConfig::default()
            },
            Arc::clone(&store),
            permissions,
            task_manager,
        )
        .expect("QQ source");
        assert_eq!(
            source
                .sources
                .get(QQ_SOURCE_NAME)
                .expect("QQ source registration")
                .maximum_permission,
            PermissionLevel::Operator
        );
        source
            .ingest_message(NormalizedMessage {
                message_id: "message-1".into(),
                dispatch_id: Some("dispatch-1".into()),
                target: QqTarget::C2c {
                    user_openid: "user-1".into(),
                    msg_id: "message-1".into(),
                },
                subject: "user-1".into(),
                display_name: Some("Alice".into()),
                text: "hello koi".into(),
                mentions: Vec::new(),
                explicit_bot_mention: false,
            })
            .await
            .expect("QQ message should be ingested");

        source
            .ingest_message(NormalizedMessage {
                message_id: "group-message-1".into(),
                dispatch_id: Some("group-dispatch-1".into()),
                target: QqTarget::Group {
                    group_openid: "group-1".into(),
                    msg_id: "group-message-1".into(),
                },
                subject: "operator-1".into(),
                display_name: Some("Operator".into()),
                text: "请执行运维操作".into(),
                mentions: vec!["__koi_bot__".into()],
                explicit_bot_mention: true,
            })
            .await
            .expect("explicitly mentioned QQ message should be ingested");

        source
            .ingest_message(NormalizedMessage {
                message_id: "group-message-2".into(),
                dispatch_id: Some("group-dispatch-2".into()),
                target: QqTarget::Group {
                    group_openid: "group-1".into(),
                    msg_id: "group-message-2".into(),
                },
                subject: "member-1".into(),
                display_name: Some("Member".into()),
                text: "普通群消息".into(),
                mentions: Vec::new(),
                explicit_bot_mention: false,
            })
            .await
            .expect("ordinary QQ message should be ingested");

        let task_ids = JsonlEventStore::list_task_ids(store.as_ref()).expect("task ids");
        assert_eq!(task_ids.len(), 1);
        assert!(task_ids.iter().all(|task_id| task_id.is_main()));
        let events = store
            .load_task(TaskId::MAIN)
            .await
            .expect("main task events");
        assert_eq!(events.len(), 5);
        let context = events.iter().find_map(context_from_event).expect("context");
        assert_eq!(context.origin.source, QQ_SOURCE_NAME);
        assert_eq!(context.origin.native_event_id, "message-1");
        assert_eq!(context.scope, Scope::new("qq_c2c", "user-1"));
        assert_eq!(context.permission, PermissionLevel::User);
        let ContextPayload::Text { text, .. } = &context.payload else {
            panic!("QQ context should contain text payload");
        };
        assert!(text.contains("类型=私聊（C2C）"));
        assert!(text.contains("会话=qq_c2c:user-1"));

        let mut contexts = Vec::new();
        for task_id in JsonlEventStore::list_task_ids(store.as_ref())
            .expect("task ids")
            .into_iter()
        {
            contexts.extend(
                store
                    .load_task(task_id)
                    .await
                    .expect("task events")
                    .iter()
                    .filter_map(context_from_event)
                    .cloned(),
            );
        }
        let explicit_group_context = contexts
            .iter()
            .find(|context| context.origin.native_event_id == "group-message-1")
            .expect("explicit group context");
        assert_eq!(explicit_group_context.permission, PermissionLevel::Operator);
        let ContextPayload::Text { text, .. } = &explicit_group_context.payload else {
            panic!("QQ group context should contain text payload");
        };
        assert!(text.contains("类型=群聊"));
        assert!(text.contains("会话=qq_group:group-1"));
        assert!(text.contains("明确@bot=是"));
        let ordinary_group_context = contexts
            .iter()
            .find(|context| context.origin.native_event_id == "group-message-2")
            .expect("ordinary group context");
        assert_eq!(ordinary_group_context.permission, PermissionLevel::User);
        let ContextPayload::Text { text, .. } = &ordinary_group_context.payload else {
            panic!("QQ group context should contain text payload");
        };
        assert!(text.contains("会话=qq_group:group-1"));
        assert!(text.contains("明确@bot=否"));
    }
}
