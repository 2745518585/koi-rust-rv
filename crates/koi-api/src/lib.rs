//! HTTP API, server-sent event boundary, and Web-source command contracts.
//!
//! This crate owns transport DTOs and authentication hand-off only. Implementations of the
//! command ports must hand trusted Web identities to `koi-core`; an HTTP request may only carry
//! an untrusted permission suggestion, which the source adapter and `koi-core` must clamp again.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::stream;
use koi_core::domain::{EventId, PermissionLevel, TaskId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;
use uuid::Uuid;

pub const CRATE_NAME: &str = "koi-api";
pub const WEB_SOURCE_NAME: &str = "web";
const WEB_SESSION_COOKIE: &str = "koi_web_session";

/// A principal whose identity and permission were established by the HTTP authentication layer.
///
/// This is deliberately not deserializable from a request body. The Web source adapter uses it to
/// create the core `Principal` and lets `IngressRegistrar` clamp the final effective permission.
#[derive(Clone, Debug)]
pub struct WebPrincipal {
    pub subject: String,
    pub display_name: Option<String>,
    pub permission: PermissionLevel,
}

impl WebPrincipal {
    #[must_use]
    pub fn admin(subject: impl Into<String>, display_name: impl Into<Option<String>>) -> Self {
        Self {
            subject: subject.into(),
            display_name: display_name.into(),
            permission: PermissionLevel::Admin,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterUserCommand {
    pub email: String,
    pub username: String,
    pub password: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginCommand {
    pub email: String,
    pub password: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebUserDto {
    pub user_id: String,
    pub username: String,
    pub email: String,
    pub permission: String,
}

/// Authentication result kept inside the transport boundary. Its opaque session token is emitted
/// only as an `HttpOnly` cookie; event sources and core code receive just `WebPrincipal`.
#[derive(Clone, Debug)]
pub struct WebSession {
    pub token: String,
    pub principal: WebPrincipal,
    pub user: WebUserDto,
}

/// Credential-system port implemented by infrastructure. It keeps password verification and
/// account storage out of HTTP routes and ensures the resulting subject is the core identity.
pub trait WebIdentityProvider: Send + Sync {
    /// 注册一个 Web 用户。
    ///
    /// # Errors
    ///
    /// 当用户信息非法或账户已经存在时返回错误。
    fn register(&self, command: RegisterUserCommand) -> Result<WebSession, WebApiError>;
    /// 登录一个 Web 用户。
    ///
    /// # Errors
    ///
    /// 当凭据无效或账户不可用时返回错误。
    fn login(&self, command: LoginCommand) -> Result<WebSession, WebApiError>;
    /// 使用会话令牌恢复登录状态。
    ///
    /// # Errors
    ///
    /// 当令牌无效、过期或会话不存在时返回错误。
    fn authenticate_session(&self, token: &str) -> Result<WebSession, WebApiError>;
    /// 使当前不透明会话令牌立即失效。
    ///
    /// # Errors
    ///
    /// 当令牌无效或会话不存在时返回错误。
    fn logout(&self, token: &str) -> Result<(), WebApiError>;
}

/// Request body for a Web-originated diagnostic task.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskCommand {
    pub message: String,
    pub scope: ScopeDto,
    /// 用户希望本次输入最多使用的权限；缺省时由来源适配器使用当前身份权限。
    #[serde(default)]
    pub suggested_permission: Option<PermissionLevel>,
}

/// A follow-up Web context event for an existing task. The transport can choose only a bounded
/// external context kind; source, identity, permission, sequence and event ID remain server-side.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendContextCommand {
    pub message: String,
    pub kind: WebContextKind,
    /// 用户希望本次输入最多使用的权限；该值不能超过认证身份权限。
    #[serde(default)]
    pub suggested_permission: Option<PermissionLevel>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebContextKind {
    UserMessage,
    Alert,
    AssistantMessage,
}

/// A request to record user-originated cancellation evidence. This is distinct from a direct
/// lifecycle cancellation control: the core can audit it as ingress and decide how runners react.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancellationRequestCommand {
    pub reason: String,
    /// 取消请求的建议权限；缺省时使用当前身份权限。
    #[serde(default)]
    pub suggested_permission: Option<PermissionLevel>,
}

/// Request body for a user decision on an existing tool approval request.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalCommand {
    pub approved: bool,
    /// 审批输入的建议权限；缺省时使用当前身份权限。
    #[serde(default)]
    pub suggested_permission: Option<PermissionLevel>,
}

/// Direct task control commands. They are turned into core control events by the Web source
/// adapter and do not become model context.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskControlCommand {
    pub action: TaskControlAction,
    pub reason: Option<String>,
    pub minimum_permission: Option<PermissionLevel>,
    pub provider: Option<String>,
    pub model_id: Option<String>,
}

/// Request body for naming an existing (non-main) task session.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameTaskCommand {
    pub name: String,
}

/// Result of deleting a terminal task session and its event stream.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedTaskDto {
    pub task_id: String,
    pub deleted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskControlAction {
    Pause,
    Resume,
    Cancel,
    SelectModel,
    SetMinimumPermission,
}

/// Read-only HTTP query responsibility. It is separate from `WebCommandPort` so query handling
/// cannot accidentally be granted a capability to append core events.
#[async_trait]
pub trait WebQueryPort: Send + Sync {
    async fn dashboard(&self, principal: &WebPrincipal) -> Result<DashboardDto, WebApiError>;
    async fn list_tasks(&self, principal: &WebPrincipal) -> Result<Vec<TaskDto>, WebApiError>;
    async fn task_events(
        &self,
        principal: &WebPrincipal,
        task_id: TaskId,
    ) -> Result<Vec<EventDto>, WebApiError>;
    /// Tests task visibility without revealing the existence of a task to an unauthorized user.
    async fn can_access_task(&self, principal: &WebPrincipal, task_id: TaskId) -> bool;
}

/// The only command surface exposed to the Web transport.
///
/// Implementations own the source adapter boundary: authenticate identity upstream, create an
/// `IngressDraft` or `ControlExecutionRequest`, then delegate event allocation and authorization
/// checks to `koi-core`.
#[async_trait]
pub trait WebCommandPort: Send + Sync {
    async fn create_task(
        &self,
        principal: WebPrincipal,
        command: CreateTaskCommand,
    ) -> Result<TaskDto, WebApiError>;

    async fn append_context(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: AppendContextCommand,
    ) -> Result<EventDto, WebApiError>;

    async fn request_cancellation(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: CancellationRequestCommand,
    ) -> Result<EventDto, WebApiError>;

    async fn submit_approval(
        &self,
        principal: WebPrincipal,
        approval_request_event_id: EventId,
        command: ApprovalCommand,
    ) -> Result<ApprovalDto, WebApiError>;

    async fn control_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: TaskControlCommand,
    ) -> Result<TaskDto, WebApiError>;

    /// Set a stable display name on a task session. The main session cannot be named.
    async fn name_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: NameTaskCommand,
    ) -> Result<TaskDto, WebApiError>;

    /// Delete a terminal task session and its event stream. The main session cannot be deleted.
    async fn delete_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
    ) -> Result<DeletedTaskDto, WebApiError>;
}

/// Event delivery responsibility. Events are immutable projections of core event envelopes.
pub trait WebEventPort: Send + Sync {
    fn subscribe(&self) -> broadcast::Receiver<WebStreamEvent>;
}

/// Full backend capability needed by the Axum routes.
pub trait WebApi: WebQueryPort + WebCommandPort + WebEventPort {}
impl<T> WebApi for T where T: WebQueryPort + WebCommandPort + WebEventPort {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeDto {
    pub kind: String,
    pub id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDto {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelectionDto {
    pub provider: String,
    pub model_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDto {
    pub task_id: String,
    pub is_main: bool,
    pub title: String,
    pub status: String,
    pub source: String,
    pub scope: ScopeDto,
    pub started_at: String,
    pub updated_at: String,
    pub last_event_kind: String,
    pub last_event_summary: String,
    pub minimum_control_permission: String,
    pub selected_model: Option<ModelSelectionDto>,
    pub usage: UsageDto,
    pub event_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventDto {
    pub id: String,
    pub task_id: String,
    pub sequence: u64,
    pub occurred_at: String,
    pub source: String,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub permission: String,
    /// 工具生命周期事件所属的原始 `ToolEvent::Proposed` 事件 ID。
    ///
    /// 非工具事件以及无法安全解析关联关系的事件为 `null`。该字段只用于展示层
    /// 聚合，不参与权限判断或事件处理。
    pub tool_proposal_event_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDto {
    pub approval_request_event_id: String,
    pub task_id: String,
    pub tool_name: String,
    pub tool_description: String,
    pub required_permission: String,
    pub requested_at: String,
    pub arguments_hash: String,
    pub arguments_preview: String,
    pub scope: ScopeDto,
    pub status: String,
    pub requester: String,
}

/// A core-originated request for extra authorization, delivered to the Web source for display.
/// The caller cannot approve it by echoing this payload: approval is submitted separately and is
/// revalidated by `koi-core` against the persisted request event.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ElevationRequestDto {
    pub task_id: String,
    pub approval_request_event_id: String,
    pub tool_proposal_event_id: String,
    pub tool_name: String,
    pub arguments_hash: String,
    pub required_permission: String,
    pub original_evidence_event_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDto {
    pub name: String,
    pub description: String,
    pub required_permission: String,
    pub side_effect: String,
    pub timeout_ms: u64,
    pub model_visible: bool,
    #[serde(default)]
    pub main_session_only: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthDto {
    pub api: String,
    pub event_store: String,
    pub model_provider: String,
    pub last_heartbeat_at: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsageDto {
    pub label: String,
    pub input: u64,
    pub output: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummaryDto {
    pub input_tokens_today: u64,
    pub output_tokens_today: u64,
    pub month_spent_usd: f64,
    pub monthly_budget_usd: f64,
    pub daily: Vec<DailyUsageDto>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardDto {
    pub generated_at: String,
    pub health: HealthDto,
    pub tasks: Vec<TaskDto>,
    pub approvals: Vec<ApprovalDto>,
    pub recent_events: Vec<EventDto>,
    pub tools: Vec<ToolDto>,
    pub models: Vec<ModelSelectionDto>,
    pub default_model: Option<ModelSelectionDto>,
    pub usage: UsageSummaryDto,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WebStreamEvent {
    #[serde(rename = "event.appended")]
    EventAppended { event: EventDto },
    #[serde(rename = "authorization.requested")]
    AuthorizationRequested { request: ElevationRequestDto },
}

impl WebStreamEvent {
    #[must_use]
    pub fn task_id(&self) -> &str {
        match self {
            Self::EventAppended { event } => &event.task_id,
            Self::AuthorizationRequested { request } => &request.task_id,
        }
    }
}

#[derive(Debug, Error)]
pub enum WebApiError {
    #[error("请求参数无效：{0}")]
    Validation(String),
    #[error("资源不存在：{0}")]
    NotFound(String),
    #[error("无权执行此操作：{0}")]
    Forbidden(String),
    #[error("当前状态不允许此操作：{0}")]
    Conflict(String),
    #[error("服务暂不可用：{0}")]
    Unavailable(String),
    #[error("内部错误：{0}")]
    Internal(String),
}

impl WebApiError {
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// 503-style failure for dependencies that exist but are not ready (e.g. the main session
    /// stream has not been bootstrapped yet).
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }
    #[must_use]
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

#[derive(Clone)]
pub struct WebAuth {
    identities: Arc<dyn WebIdentityProvider>,
    secure_cookie: bool,
}

impl WebAuth {
    /// Creates the token-to-principal mapping used by the Web source. A deployed installation can
    /// replace this with a session or reverse-proxy adapter without changing command handling.
    ///
    /// # Errors
    ///
    pub fn new(identities: Arc<dyn WebIdentityProvider>, secure_cookie: bool) -> Self {
        Self {
            identities,
            secure_cookie,
        }
    }

    fn authenticate(&self, headers: &HeaderMap) -> Result<WebPrincipal, ApiError> {
        session_cookie(headers)
            .ok_or_else(|| ApiError::unauthorized("缺少 Web 会话"))
            .and_then(|token| {
                self.identities
                    .authenticate_session(token)
                    .map(|session| session.principal)
                    .map_err(ApiError::from)
            })
    }

    fn current_session(&self, headers: &HeaderMap) -> Result<WebSession, ApiError> {
        session_cookie(headers)
            .ok_or_else(|| ApiError::unauthorized("缺少 Web 会话"))
            .and_then(|token| {
                self.identities
                    .authenticate_session(token)
                    .map_err(ApiError::from)
            })
    }

    fn session_cookie(&self, token: &str) -> String {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        format!(
            "{WEB_SESSION_COOKIE}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age=28800{secure}"
        )
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == WEB_SESSION_COOKIE).then_some(value)
            })
        })
}

#[derive(Clone)]
struct HttpState {
    api: Arc<dyn WebApi>,
    auth: WebAuth,
}

/// Builds the versioned Axum API router. Static file hosting remains the application's concern.
pub fn router(api: Arc<dyn WebApi>, auth: WebAuth) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/api/v1/auth/register", post(register))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/me", get(current_user))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/dashboard", get(dashboard))
        .route("/api/v1/tasks", get(list_tasks).post(create_task))
        .route(
            "/api/v1/tasks/{task_id}/events",
            get(task_events).post(append_context),
        )
        .route(
            "/api/v1/tasks/{task_id}/cancellation-requests",
            post(request_cancellation),
        )
        .route("/api/v1/tasks/{task_id}/controls", post(control_task))
        .route("/api/v1/tasks/{task_id}/name", post(name_task))
        .route("/api/v1/tasks/{task_id}", delete(delete_task))
        .route(
            "/api/v1/approvals/{approval_request_event_id}",
            post(submit_approval),
        )
        .route(
            "/api/v1/authorizations/{approval_request_event_id}",
            post(submit_approval),
        )
        .route("/api/v1/events/stream", get(event_stream))
        .with_state(HttpState { api, auth })
}

#[derive(Serialize)]
struct ApiEnvelope<T> {
    data: T,
}
#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionDto {
    user: WebUserDto,
}

fn session_response(state: &HttpState, session: WebSession) -> Result<Response, ApiError> {
    let cookie = HeaderValue::from_str(&state.auth.session_cookie(&session.token))
        .map_err(|_| ApiError::bad_request("无法创建 Web 会话 Cookie"))?;
    let mut response = Json(ApiEnvelope {
        data: SessionDto { user: session.user },
    })
    .into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

async fn register(
    State(state): State<HttpState>,
    Json(command): Json<RegisterUserCommand>,
) -> Result<Response, ApiError> {
    let session = state
        .auth
        .identities
        .register(command)
        .map_err(ApiError::from)?;
    session_response(&state, session)
}

async fn login(
    State(state): State<HttpState>,
    Json(command): Json<LoginCommand>,
) -> Result<Response, ApiError> {
    let session = state
        .auth
        .identities
        .login(command)
        .map_err(ApiError::from)?;
    session_response(&state, session)
}

async fn current_user(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<ApiEnvelope<SessionDto>>, ApiError> {
    let session = state.auth.current_session(&headers)?;
    Ok(Json(ApiEnvelope {
        data: SessionDto { user: session.user },
    }))
}

async fn logout(State(state): State<HttpState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let token = session_cookie(&headers).ok_or_else(|| ApiError::unauthorized("缺少 Web 会话"))?;
    state
        .auth
        .identities
        .logout(token)
        .map_err(ApiError::from)?;
    let secure = if state.auth.secure_cookie {
        "; Secure"
    } else {
        ""
    };
    let cookie = HeaderValue::from_str(&format!(
        "{WEB_SESSION_COOKIE}=; HttpOnly; Path=/; SameSite=Strict; Max-Age=0{secure}"
    ))
    .map_err(|_| ApiError::bad_request("无法清除 Web 会话 Cookie"))?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

async fn dashboard(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<ApiEnvelope<DashboardDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .dashboard(&principal)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn list_tasks(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<ApiEnvelope<Vec<TaskDto>>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .list_tasks(&principal)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn task_events(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<ApiEnvelope<Vec<EventDto>>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .task_events(&principal, parse_task_id(&task_id)?)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn create_task(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(command): Json<CreateTaskCommand>,
) -> Result<Json<ApiEnvelope<TaskDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .create_task(principal, command)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn append_context(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(command): Json<AppendContextCommand>,
) -> Result<Json<ApiEnvelope<EventDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .append_context(principal, parse_task_id(&task_id)?, command)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn request_cancellation(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(command): Json<CancellationRequestCommand>,
) -> Result<Json<ApiEnvelope<EventDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .request_cancellation(principal, parse_task_id(&task_id)?, command)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn submit_approval(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(approval_request_event_id): Path<String>,
    Json(command): Json<ApprovalCommand>,
) -> Result<Json<ApiEnvelope<ApprovalDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .submit_approval(
                principal,
                parse_event_id(&approval_request_event_id)?,
                command,
            )
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn control_task(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(command): Json<TaskControlCommand>,
) -> Result<Json<ApiEnvelope<TaskDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .control_task(principal, parse_task_id(&task_id)?, command)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn name_task(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(command): Json<NameTaskCommand>,
) -> Result<Json<ApiEnvelope<TaskDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .name_task(principal, parse_task_id(&task_id)?, command)
            .await
            .map_err(ApiError::from)?,
    }))
}

async fn delete_task(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<ApiEnvelope<DeletedTaskDto>>, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    Ok(Json(ApiEnvelope {
        data: state
            .api
            .delete_task(principal, parse_task_id(&task_id)?)
            .await
            .map_err(ApiError::from)?,
    }))
}

#[derive(Deserialize)]
struct StreamQuery {
    task_id: Option<String>,
}

async fn event_stream(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let principal = state.auth.authenticate(&headers)?;
    if let Some(task_id) = query.task_id.as_deref() {
        let task_id = parse_task_id(task_id)?;
        if !state.api.can_access_task(&principal, task_id).await {
            return Err(ApiError::from(WebApiError::not_found("任务不存在")));
        }
    }
    let requested_task_id = query.task_id;
    let receiver = state.api.subscribe();
    let api = Arc::clone(&state.api);
    let events = stream::unfold(receiver, move |mut receiver| {
        let requested_task_id = requested_task_id.clone();
        let principal = principal.clone();
        let api = Arc::clone(&api);
        async move {
            loop {
                match receiver.recv().await {
                    Ok(stream_event) => {
                        let matches_filter = requested_task_id
                            .as_deref()
                            .is_none_or(|task_id| task_id == stream_event.task_id());
                        let visible = match Uuid::parse_str(stream_event.task_id()) {
                            Ok(task_id) => api.can_access_task(&principal, TaskId(task_id)).await,
                            Err(_) => false,
                        };
                        if !matches_filter || !visible {
                            continue;
                        }
                        let Ok(event) =
                            Event::default().event("koi.event").json_data(&stream_event)
                        else {
                            continue;
                        };
                        return Some((Ok::<Event, Infallible>(event), receiver));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    Ok(Sse::new(events)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}

fn parse_task_id(raw: &str) -> Result<TaskId, ApiError> {
    Uuid::parse_str(raw)
        .map(TaskId)
        .map_err(|_| ApiError::bad_request("task_id 必须是 UUID"))
}

fn parse_event_id(raw: &str) -> Result<EventId, ApiError> {
    Uuid::parse_str(raw)
        .map(EventId)
        .map_err(|_| ApiError::bad_request("approval_request_event_id 必须是 UUID"))
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}
impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }
}
impl From<WebApiError> for ApiError {
    fn from(error: WebApiError) -> Self {
        let status = match error {
            WebApiError::Validation(_) => StatusCode::BAD_REQUEST,
            WebApiError::NotFound(_) => StatusCode::NOT_FOUND,
            WebApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            WebApiError::Conflict(_) => StatusCode::CONFLICT,
            WebApiError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            WebApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
