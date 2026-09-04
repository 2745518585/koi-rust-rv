use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

uuid_id!(EventId, "单条已持久化事件的全局唯一标识。");
uuid_id!(TaskId, "单个 Agent 任务的标识。");

impl TaskId {
    /// 连续运行的主会话任务，使用全零 UUID 作为稳定保留值。
    pub const MAIN: Self = Self(Uuid::nil());

    #[must_use]
    pub const fn is_main(self) -> bool {
        self.0.is_nil()
    }
}

/// 已注册外部来源的稳定名称，例如 `qq`、`web` 或 `alertmanager`。
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SourceName(String);

impl SourceName {
    /// # Errors
    ///
    /// 当名称为空或包含不允许的字符时返回错误。
    pub fn new(name: impl Into<String>) -> Result<Self, SourceNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(SourceNameError::Empty);
        }
        if !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        }) {
            return Err(SourceNameError::Invalid(name));
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Error)]
pub enum SourceNameError {
    #[error("来源名称不能为空")]
    Empty,
    #[error("来源名称包含不允许的字符：{0}")]
    Invalid(String),
}

/// 事件的实际创建来源。
///
/// 来源描述“谁创建了这条事件”，不直接等于权限。模型与工具来源在没有合法权限父事件
/// 时的有效权限始终为 `None`。所有核心外来源均使用 `External`，其权限由来源注册表
/// 决定。
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum EventSource {
    System,
    Model,
    Tool,
    External(SourceName),
}

impl EventSource {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::System => "system",
            Self::Model => "model",
            Self::Tool => "tool",
            Self::External(name) => name.as_str(),
        }
    }
}

/// 每条事件统一携带的创建来源、权限继承与有效期信息。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventProvenance {
    pub creator: EventSource,
    /// 直接来源经核心核定后的权限。模型与工具来源必须为 `None`。
    pub direct_permission: Option<PermissionLevel>,
    /// 权限继承自的上级事件；与 `causation_id` 的触发关系相互独立。
    pub authority_parent_event_id: Option<EventId>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl EventProvenance {
    #[must_use]
    pub const fn system() -> Self {
        Self {
            creator: EventSource::System,
            direct_permission: Some(PermissionLevel::System),
            authority_parent_event_id: None,
            expires_at: None,
        }
    }

    #[must_use]
    pub const fn model(authority_parent_event_id: Option<EventId>) -> Self {
        Self {
            creator: EventSource::Model,
            direct_permission: None,
            authority_parent_event_id,
            expires_at: None,
        }
    }

    #[must_use]
    pub const fn tool() -> Self {
        Self {
            creator: EventSource::Tool,
            direct_permission: None,
            authority_parent_event_id: None,
            expires_at: None,
        }
    }
}

/// 与来源无关的用户或服务身份。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Principal {
    pub source: String,
    pub subject: String,
    pub display_name: Option<String>,
}

impl Principal {
    #[must_use]
    pub fn new(source: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            subject: subject.into(),
            display_name: None,
        }
    }
}

/// 资源边界，例如 `qq_group:123`、`server:prod-1` 或 `web:session-7`。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Scope {
    pub kind: String,
    pub id: String,
}

impl Scope {
    #[must_use]
    pub fn new(kind: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
        }
    }
}

/// 由来源适配器提供的事件身份。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOrigin {
    pub source: String,
    pub source_instance: String,
    pub native_event_id: String,
}

/// 来源提供的可选顺序信息。
///
/// `local_sequence` 便于重放，但不能证明来源侧的真实顺序。
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StreamPosition {
    pub stream_id: String,
    pub source_sequence: Option<u64>,
    pub local_sequence: u64,
}

/// 输入事件可授予的最高权限。
///
/// 权限只能由来源适配器或核心内部赋予；模型输出、工具结果和外部观测数据必须为
/// `None`，因此只能辅助分析，不能成为工具调用的授权依据。
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PermissionLevel {
    /// 不具备指令或授权能力，只能作为参考资料。
    None,
    /// 普通用户可发起的查询、诊断与通知。
    User,
    /// 受控运维操作。
    Operator,
    /// 高风险操作、配置管理和审批。
    Admin,
    /// 仅限核心内部规则与系统调度，外部来源不得产生。
    System,
}

impl PermissionLevel {
    /// 此权限是否能作为工具调用的授权证据。
    #[must_use]
    pub const fn can_authorize(self) -> bool {
        !matches!(self, Self::None)
    }

    /// 此权限是否满足目标操作的最低要求。
    #[must_use]
    pub fn allows(self, required: Self) -> bool {
        self >= required
    }
}

/// 核心对一次外部权限建议做出的可审计结论。
///
/// `effective_permission` 必须同时不高于外部来源建议、来源注册上限和核心身份解析结果。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionAssessment {
    pub suggested_permission: PermissionLevel,
    pub source_maximum_permission: PermissionLevel,
    pub identity_maximum_permission: PermissionLevel,
    pub effective_permission: PermissionLevel,
}

impl PermissionAssessment {
    #[must_use]
    pub fn new(
        suggested_permission: PermissionLevel,
        source_maximum_permission: PermissionLevel,
        identity_maximum_permission: PermissionLevel,
    ) -> Self {
        let effective_permission = if suggested_permission < source_maximum_permission {
            if suggested_permission < identity_maximum_permission {
                suggested_permission
            } else {
                identity_maximum_permission
            }
        } else if source_maximum_permission < identity_maximum_permission {
            source_maximum_permission
        } else {
            identity_maximum_permission
        };
        Self {
            suggested_permission,
            source_maximum_permission,
            identity_maximum_permission,
            effective_permission,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ContextKind {
    UserMessage,
    Alert,
    Approval,
    Cancellation,
    ToolResult,
    AssistantMessage,
    SystemEvent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ContextPayload {
    Text {
        text: String,
        mentions: Vec<String>,
    },
    Alert {
        name: String,
        severity: String,
        summary: String,
        labels: BTreeMap<String, String>,
    },
    Structured(Value),
}

/// 核心接收的一条不可变、规范化上下文单元。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextEnvelope {
    pub schema_version: u16,
    pub kind: ContextKind,
    pub origin: ContextOrigin,
    pub actor: Option<Principal>,
    pub scope: Scope,
    pub occurred_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub position: Option<StreamPosition>,
    /// 由来源适配器或核心赋予的权限上限，不能由模型或消息内容声明。
    pub permission: PermissionLevel,
    pub payload: ContextPayload,
    pub causation_id: Option<EventId>,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum IngressEvent {
    ContextReceived {
        context: Box<ContextEnvelope>,
        assessment: PermissionAssessment,
    },
    ApprovalSubmitted {
        approval_request_event_id: EventId,
        principal: Principal,
        scope: Scope,
        assessment: PermissionAssessment,
        approved: bool,
    },
    CancellationRequested {
        principal: Principal,
        scope: Scope,
        assessment: PermissionAssessment,
        reason: String,
    },
}

impl IngressEvent {
    #[must_use]
    pub fn context_received(context: ContextEnvelope, assessment: PermissionAssessment) -> Self {
        Self::ContextReceived {
            context: Box::new(context),
            assessment,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    /// 模型供应商返回的函数调用 ID；仅用于向同一供应商回传工具结果。
    pub provider_call_id: Option<String>,
    /// 模型选择的单一权限父事件。核心会验证它属于当前可见上下文并递归审查其来源。
    pub authority_parent_event_id: Option<EventId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum InterventionAction {
    Ignore,
    Investigate,
    AskForClarification,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ModelOutput {
    Text {
        text: String,
    },
    ToolCall(ToolCall),
    InterventionDecision {
        action: InterventionAction,
        confidence: Option<f32>,
    },
    Refusal {
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelDeltaKind {
    Text,
    ToolName,
    ToolArguments,
    Status,
    Summary,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ModelEvent {
    CallStarted {
        context_event_ids: Vec<EventId>,
        context_hash: String,
        provider: String,
        model_id: String,
    },
    Delta {
        call_started_event_id: EventId,
        sequence: u32,
        kind: ModelDeltaKind,
        content: String,
    },
    Completed {
        call_started_event_id: EventId,
        outputs: Vec<ModelOutput>,
        usage: Usage,
    },
    Failed {
        call_started_event_id: EventId,
        error: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PolicyDecision {
    Allow,
    RequireApproval,
    Deny,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolResult {
    pub summary: String,
    pub data: Value,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ToolEvent {
    Proposed {
        tool_call: ToolCall,
    },
    Validated {
        proposal_event_id: EventId,
    },
    AuthorizationChecked {
        proposal_event_id: EventId,
        decision: PolicyDecision,
        effective_permission: PermissionLevel,
        evidence_event_ids: Vec<EventId>,
    },
    ApprovalRequested {
        proposal_event_id: EventId,
    },
    Started {
        proposal_event_id: EventId,
    },
    Output {
        execution_started_event_id: EventId,
        sequence: u32,
        content: String,
    },
    Finished {
        execution_started_event_id: EventId,
        result: ToolResult,
    },
    Failed {
        execution_started_event_id: EventId,
        error: String,
    },
    Cancelled {
        execution_started_event_id: EventId,
        reason: String,
    },
    NotificationSent {
        channel: String,
        message_id: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskOperation {
    CreateChild,
    /// 为子任务设置稳定显示名称。
    NameChild {
        task_id: TaskId,
        name: String,
    },
    /// 删除一个已终止的子任务事件流；运行中的任务必须先取消。
    DeleteChild {
        task_id: TaskId,
        reason: String,
    },
    /// 由主会话向子任务投递一条控制事件。
    ControlChild {
        task_id: TaskId,
        control: Box<ControlEvent>,
    },
    ResumeChild {
        task_id: TaskId,
    },
    CancelChild {
        task_id: TaskId,
        reason: String,
    },
    DeliverChildResult {
        task_id: TaskId,
        completed_event_id: EventId,
    },
}

impl TaskOperation {
    #[must_use]
    pub const fn target_task_id(&self) -> Option<TaskId> {
        match self {
            Self::CreateChild => None,
            Self::NameChild { task_id, .. }
            | Self::DeleteChild { task_id, .. }
            | Self::ControlChild { task_id, .. }
            | Self::ResumeChild { task_id }
            | Self::CancelChild { task_id, .. }
            | Self::DeliverChildResult { task_id, .. } => Some(*task_id),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ControlEvent {
    TaskCreated {
        trigger_event_id: Option<EventId>,
    },
    TaskQueued,
    TaskPaused {
        reason: String,
    },
    TaskResumed,
    /// 为任务设置稳定显示名称；只能由任务管理操作写入。
    TaskNamed {
        name: String,
    },
    /// 选择任务后续模型调用使用的已配置模型。
    ModelSelected {
        provider: String,
        model_id: String,
    },
    /// 修改任务后续控制指令所需的最低权限。
    MinimumControlPermissionChanged {
        minimum_permission: PermissionLevel,
    },
    /// 仅主会话可发起的跨任务管理请求。
    TaskOperationRequested {
        operation: TaskOperation,
    },
    /// 核心已接受主会话的跨任务管理请求。
    TaskOperationAccepted {
        request_event_id: EventId,
        target_task_id: TaskId,
    },
    /// 核心拒绝主会话的跨任务管理请求。
    TaskOperationRejected {
        request_event_id: EventId,
        reason: String,
    },
    TaskCompleted {
        response: Option<String>,
    },
    TaskFailed {
        reason: String,
    },
    TaskCancelled {
        reason: String,
    },
    TaskExpired {
        reason: String,
    },
    BudgetExceeded {
        budget: u64,
        consumed: u64,
    },
    ContextCompacted {
        dropped_context_event_ids: Vec<EventId>,
    },
}

impl ControlEvent {
    #[must_use]
    pub const fn is_task_management_operation(&self) -> bool {
        matches!(
            self,
            Self::TaskOperationRequested { .. }
                | Self::TaskOperationAccepted { .. }
                | Self::TaskOperationRejected { .. }
        )
    }
}

/// Agent 仅追加任务事件日志中的四个顶级类别。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum AgentEvent {
    Ingress(Box<IngressEvent>),
    Control(Box<ControlEvent>),
    Model(Box<ModelEvent>),
    Tool(Box<ToolEvent>),
}

impl AgentEvent {
    #[must_use]
    pub fn ingress(event: IngressEvent) -> Self {
        Self::Ingress(Box::new(event))
    }

    #[must_use]
    pub fn model(event: ModelEvent) -> Self {
        Self::Model(Box::new(event))
    }

    #[must_use]
    pub fn tool(event: ToolEvent) -> Self {
        Self::Tool(Box::new(event))
    }

    #[must_use]
    pub fn control(event: ControlEvent) -> Self {
        Self::Control(Box::new(event))
    }
}

/// 已持久化的任务事件。`sequence` 仅在所属任务内具有权威顺序。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub task_id: TaskId,
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub causation_id: Option<EventId>,
    pub provenance: EventProvenance,
    pub payload: AgentEvent,
}

impl EventEnvelope {
    #[must_use]
    pub fn new(
        task_id: TaskId,
        sequence: u64,
        causation_id: Option<EventId>,
        payload: AgentEvent,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: EventId::new(),
            task_id,
            sequence,
            occurred_at: now,
            recorded_at: now,
            causation_id,
            provenance: EventProvenance::system(),
            payload,
        }
    }
}
