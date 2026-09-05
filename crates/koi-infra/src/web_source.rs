//! Web-source adapter: it translates authenticated HTTP commands into core events.
//!
//! The adapter owns transport-to-domain mapping and local persistence queries. `koi-core` remains
//! the authority for ingress permission assessment, event sequencing, projections, and control
//! state transitions.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::event_store::JsonlEventStore;
use crate::web_identity::WebUserStore;
use async_trait::async_trait;
use chrono::Utc;
use koi_api::{
    AppendContextCommand, ApprovalCommand, ApprovalDto, CancellationRequestCommand,
    CreateTaskCommand, DailyUsageDto, DashboardDto, DeletedTaskDto, ElevationRequestDto, EventDto,
    HealthDto, ModelSelectionDto, NameTaskCommand, ScopeDto, TaskControlAction, TaskControlCommand,
    TaskDto, ToolDto, UsageDto, UsageSummaryDto, WEB_SOURCE_NAME, WebApiError, WebCommandPort,
    WebContextKind, WebEventPort, WebPrincipal, WebQueryPort, WebStreamEvent,
};
use koi_core::agent::{
    ControlExecutionRequest, ControlExecutor, DirectControlAuthority, TaskManager,
    TaskManagerError, TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, ContextEnvelope, ContextKind, ContextOrigin, ContextPayload, ControlEvent,
    EventEnvelope, EventId, EventSource, IngressDraft, IngressEvent, IngressSubject,
    ModelSelection, PermissionLevel, Principal, Scope, SourceName, TaskId, TaskProjection,
    ToolDefinition, ToolEvent,
};
use koi_core::domain::{AuthorizationRequest, AuthorizationRequestResult};
use koi_core::ports::{
    AuthorizationError, EventStore, IngressPermissionResolver, IngressRegistrar,
    IngressRegistrationError, IngressSourceDefinition, IngressSourceRegistry,
    SourceAuthorizationProvider,
};
use tokio::sync::{Mutex, broadcast};

const WEB_INSTANCE: &str = "http-api";

pub struct KoiWebSource {
    store: Arc<JsonlEventStore>,
    sources: IngressSourceRegistry,
    permissions: WebPermissionResolver,
    task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
    write_lock: Mutex<()>,
    events: broadcast::Sender<WebStreamEvent>,
    tools: Vec<ToolDefinition>,
    models: Vec<ModelSelection>,
    default_model: Option<ModelSelection>,
    monthly_budget_usd: f64,
}

impl KoiWebSource {
    /// Creates the server-side Web source adapter.
    ///
    /// `identities` is the authoritative credential directory. It owns the mapping from Web
    /// usernames to core principal subjects and permissions; HTTP JSON never supplies either.
    /// `task_manager` must be the same instance the agent supervisor uses, so Web-created
    /// sessions and model-created sessions share one task registry.
    ///
    /// # Errors
    ///
    /// 当 Web 来源名无法注册或来源目录初始化失败时返回错误。
    pub fn new(
        store: Arc<JsonlEventStore>,
        identities: Arc<WebUserStore>,
        task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
        tools: Vec<ToolDefinition>,
        monthly_budget_usd: f64,
    ) -> Result<Self, WebApiError> {
        let source = SourceName::new(WEB_SOURCE_NAME)
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let mut sources = IngressSourceRegistry::default();
        sources
            .register(IngressSourceDefinition {
                source,
                maximum_permission: PermissionLevel::Admin,
            })
            .map_err(|error| WebApiError::internal(error.to_string()))?;

        let (events, _) = broadcast::channel(256);

        Ok(Self {
            store,
            sources,
            permissions: WebPermissionResolver { identities },
            task_manager,
            write_lock: Mutex::new(()),
            events,
            tools,
            models: Vec::new(),
            default_model: None,
            monthly_budget_usd,
        })
    }

    /// Supplies the provider/model identities configured by the application runtime.
    ///
    /// Keeping this as a builder preserves the adapter constructor used by embedded callers and
    /// tests while allowing the HTTP dashboard to advertise the available selections.
    #[must_use]
    pub fn with_model_catalog(
        mut self,
        models: impl IntoIterator<Item = ModelSelection>,
        default_model: ModelSelection,
    ) -> Self {
        self.models = models.into_iter().collect();
        self.default_model = Some(default_model);
        self
    }

    fn validate_principal(&self, principal: &WebPrincipal) -> Result<(), WebApiError> {
        if !self.permissions.identities.accepts(principal) {
            return Err(WebApiError::Forbidden("Web 身份未通过服务器侧认证".into()));
        }
        Ok(())
    }

    fn domain_principal(principal: &WebPrincipal) -> Principal {
        Principal {
            source: WEB_SOURCE_NAME.into(),
            subject: principal.subject.clone(),
            display_name: principal.display_name.clone(),
        }
    }

    fn publish(&self, event: &EventEnvelope) {
        let _ = self.events.send(WebStreamEvent::EventAppended {
            event: event_dto(event),
        });
    }

    /// 将非 Web 适配器产生的核心事件转发给 Web UI。
    ///
    /// Web 自己写入的输入事件仍由命令方法直接发布；后台 Agent 事件由事件存储订阅器
    /// 调用此方法发布，避免把 UI 推送逻辑耦合进核心循环。
    pub fn publish_event(&self, event: &EventEnvelope) {
        self.publish(event);
    }

    /// Creates the core-facing authorization capability for this source. The provider is
    /// intentionally asynchronous: it announces a pending Web confirmation and waits for the
    /// normal Web approval command to append a bound ingress event.
    #[must_use]
    pub fn authorization_provider(self: &Arc<Self>) -> Arc<dyn SourceAuthorizationProvider> {
        Arc::new(WebAuthorizationProvider {
            source: Arc::clone(self),
        })
    }

    async fn request_elevation(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationRequestResult, AuthorizationError> {
        let _guard = self.write_lock.lock().await;
        let events = self
            .store
            .load_task(request.task_id)
            .await
            .map_err(|error| AuthorizationError::new(error.to_string()))?;
        let valid_request = events.iter().any(|event| {
            event.id == request.approval_request_event_id
                && matches!(event.payload, AgentEvent::Tool(ref tool) if matches!(tool.as_ref(), ToolEvent::ApprovalRequested { proposal_event_id } if *proposal_event_id == request.tool_proposal_event_id))
        });
        if !valid_request {
            return Err(AuthorizationError::new(
                "提权请求未绑定当前任务中已持久化的审批事件",
            ));
        }
        let _ = self.events.send(WebStreamEvent::AuthorizationRequested {
            request: ElevationRequestDto {
                task_id: request.task_id.to_string(),
                approval_request_event_id: request.approval_request_event_id.to_string(),
                tool_proposal_event_id: request.tool_proposal_event_id.to_string(),
                tool_name: request.tool_name,
                arguments_hash: request.arguments_hash,
                required_permission: permission_name(request.required_permission),
                original_evidence_event_ids: request
                    .original_evidence_event_ids
                    .into_iter()
                    .map(|event_id| event_id.to_string())
                    .collect(),
            },
        });
        Ok(AuthorizationRequestResult::Pending)
    }

    async fn task_records(&self) -> Result<Vec<TaskRecord>, WebApiError> {
        let task_ids = JsonlEventStore::list_task_ids(self.store.as_ref())
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let mut records = Vec::with_capacity(task_ids.len());
        for task_id in task_ids {
            let events = self
                .store
                .load_task(task_id)
                .await
                .map_err(|error| WebApiError::internal(error.to_string()))?;
            if events.is_empty() {
                continue;
            }
            let runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
                .await
                .map_err(|error| WebApiError::internal(error.to_string()))?;
            records.push(TaskRecord {
                task_id,
                minimum_control_permission: runtime.projection().minimum_control_permission,
                summary: task_dto(task_id, &events, runtime.projection()),
                events,
            });
        }
        records.sort_by(|left, right| right.summary.updated_at.cmp(&left.summary.updated_at));
        Ok(records)
    }

    fn can_access_record(principal: &WebPrincipal, record: &TaskRecord) -> bool {
        // 会话的最低控制权限同时作为 Web 端的可见性边界：能控制该会话的用户才能
        // 看到它。这样不会因为创建者、任务来源或管理员特判而绕过会话自身的权限门槛。
        principal
            .permission
            .allows(record.minimum_control_permission)
    }

    async fn load_accessible_task(
        &self,
        principal: &WebPrincipal,
        task_id: TaskId,
    ) -> Result<TaskRecord, WebApiError> {
        let events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        if events.is_empty() {
            return Err(WebApiError::not_found("任务不存在"));
        }
        let runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let record = TaskRecord {
            task_id,
            minimum_control_permission: runtime.projection().minimum_control_permission,
            summary: task_dto(task_id, &events, runtime.projection()),
            events,
        };
        if !Self::can_access_record(principal, &record) {
            return Err(WebApiError::not_found("任务不存在"));
        }
        Ok(record)
    }

    /// 恢复主会话运行时。Web 侧所有跨任务管理操作都以主会话事件流为审计骨架。
    async fn recover_main_runtime(&self) -> Result<TaskRuntime<Arc<JsonlEventStore>>, WebApiError> {
        TaskRuntime::recover(Arc::clone(&self.store), TaskId::MAIN)
            .await
            .map_err(|_| WebApiError::unavailable("主会话尚未初始化，无法执行任务管理操作"))
    }

    async fn record_task_input<RT>(
        &self,
        runtime: &mut TaskRuntime<RT>,
        principal: &WebPrincipal,
        command: &CreateTaskCommand,
    ) -> Result<EventEnvelope, WebApiError>
    where
        RT: EventStore,
    {
        let suggested_permission =
            resolve_suggested_permission(principal, command.suggested_permission)?;
        let now = Utc::now();
        let actor = Self::domain_principal(principal);
        let context = ContextEnvelope {
            schema_version: 1,
            kind: ContextKind::UserMessage,
            origin: ContextOrigin {
                source: WEB_SOURCE_NAME.into(),
                source_instance: WEB_INSTANCE.into(),
                native_event_id: EventId::new().to_string(),
            },
            actor: Some(actor),
            scope: Scope::new(command.scope.kind.clone(), command.scope.id.clone()),
            occurred_at: now,
            received_at: now,
            position: None,
            permission: PermissionLevel::None,
            payload: ContextPayload::Text {
                text: command.message.clone(),
                mentions: Vec::new(),
            },
            causation_id: None,
            content_hash: fingerprint(&command.message),
        };
        let registrar = IngressRegistrar::new(&self.sources, &self.permissions);
        registrar
            .register(
                runtime,
                IngressDraft::Context {
                    context: Box::new(context),
                    suggested_permission,
                },
            )
            .await
            .map_err(map_ingress_error)
    }

    async fn record_context<RT>(
        &self,
        runtime: &mut TaskRuntime<RT>,
        principal: &WebPrincipal,
        scope: Scope,
        kind: ContextKind,
        message: String,
        suggested_permission: PermissionLevel,
    ) -> Result<EventEnvelope, WebApiError>
    where
        RT: EventStore,
    {
        let now = Utc::now();
        let context = ContextEnvelope {
            schema_version: 1,
            kind,
            origin: ContextOrigin {
                source: WEB_SOURCE_NAME.into(),
                source_instance: WEB_INSTANCE.into(),
                native_event_id: EventId::new().to_string(),
            },
            actor: Some(Self::domain_principal(principal)),
            scope,
            occurred_at: now,
            received_at: now,
            position: None,
            permission: PermissionLevel::None,
            payload: ContextPayload::Text {
                text: message.clone(),
                mentions: Vec::new(),
            },
            causation_id: None,
            content_hash: fingerprint(&message),
        };
        IngressRegistrar::new(&self.sources, &self.permissions)
            .register(
                runtime,
                IngressDraft::Context {
                    context: Box::new(context),
                    suggested_permission,
                },
            )
            .await
            .map_err(map_ingress_error)
    }

    fn tool_dtos(&self) -> Vec<ToolDto> {
        self.tools
            .iter()
            .map(|tool| ToolDto {
                name: tool.name.clone(),
                description: tool.description.clone(),
                required_permission: permission_name(tool.required_permission),
                side_effect: format!("{:?}", tool.side_effect),
                timeout_ms: tool.timeout_ms,
                model_visible: tool.model_visible,
                main_session_only: tool.main_session_only,
            })
            .collect()
    }

    fn approval_dtos(&self, records: &[TaskRecord]) -> Vec<ApprovalDto> {
        let descriptions = self
            .tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.description.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut approvals = Vec::new();

        for record in records {
            for request_event in &record.events {
                let AgentEvent::Tool(tool_event) = &request_event.payload else {
                    continue;
                };
                let ToolEvent::ApprovalRequested { proposal_event_id } = tool_event.as_ref() else {
                    continue;
                };
                let Some(ToolEvent::Proposed { tool_call }) =
                    record.events.iter().find_map(|event| {
                        if event.id != *proposal_event_id {
                            return None;
                        }
                        let AgentEvent::Tool(tool_event) = &event.payload else {
                            return None;
                        };
                        match tool_event.as_ref() {
                            ToolEvent::Proposed { tool_call } => Some(ToolEvent::Proposed {
                                tool_call: tool_call.clone(),
                            }),
                            _ => None,
                        }
                    })
                else {
                    continue;
                };

                let decision = record.events.iter().rev().find_map(|event| {
                    let AgentEvent::Ingress(ingress) = &event.payload else {
                        return None;
                    };
                    match ingress.as_ref() {
                        IngressEvent::ApprovalSubmitted {
                            approval_request_event_id,
                            approved,
                            ..
                        } if *approval_request_event_id == request_event.id => Some(*approved),
                        _ => None,
                    }
                });
                let arguments = serde_json::to_string(&tool_call.arguments)
                    .unwrap_or_else(|_| "<无法序列化参数>".into());
                approvals.push(ApprovalDto {
                    approval_request_event_id: request_event.id.to_string(),
                    task_id: record.task_id.to_string(),
                    tool_name: tool_call.name.clone(),
                    tool_description: descriptions.get(tool_call.name.as_str()).map_or_else(
                        || "已注册工具调用".into(),
                        |description| (*description).into(),
                    ),
                    required_permission: self
                        .tools
                        .iter()
                        .find(|tool| tool.name == tool_call.name)
                        .map_or_else(
                            || "Operator".into(),
                            |tool| permission_name(tool.required_permission),
                        ),
                    requested_at: request_event.recorded_at.to_rfc3339(),
                    arguments_hash: fingerprint(&arguments),
                    arguments_preview: truncate(&arguments, 160),
                    scope: record.summary.scope.clone(),
                    status: match decision {
                        Some(true) => "Approved",
                        Some(false) => "Denied",
                        None => "Pending",
                    }
                    .into(),
                    requester: request_event.provenance.creator.as_str().into(),
                });
            }
        }
        approvals.sort_by(|left, right| right.requested_at.cmp(&left.requested_at));
        approvals
    }
}

#[async_trait]
impl WebQueryPort for KoiWebSource {
    async fn dashboard(&self, principal: &WebPrincipal) -> Result<DashboardDto, WebApiError> {
        self.validate_principal(principal)?;
        let _guard = self.write_lock.lock().await;
        let records = self
            .task_records()
            .await?
            .into_iter()
            .filter(|record| Self::can_access_record(principal, record))
            .collect::<Vec<_>>();
        let mut recent_events = records
            .iter()
            .flat_map(|record| record.events.iter().map(event_dto))
            .collect::<Vec<_>>();
        recent_events.sort_by(|left, right| right.occurred_at.cmp(&left.occurred_at));
        recent_events.truncate(30);

        let (input_tokens_today, output_tokens_today) =
            records.iter().fold((0_u64, 0_u64), |totals, record| {
                (
                    totals.0.saturating_add(record.summary.usage.input_tokens),
                    totals.1.saturating_add(record.summary.usage.output_tokens),
                )
            });
        Ok(DashboardDto {
            generated_at: Utc::now().to_rfc3339(),
            health: HealthDto {
                api: "healthy".into(),
                event_store: "healthy".into(),
                model_provider: "healthy".into(),
                last_heartbeat_at: Utc::now().to_rfc3339(),
            },
            tasks: records
                .iter()
                .map(|record| record.summary.clone())
                .collect(),
            approvals: self.approval_dtos(&records),
            recent_events,
            tools: self.tool_dtos(),
            models: self.models.iter().map(model_selection_dto).collect(),
            default_model: self.default_model.as_ref().map(model_selection_dto),
            usage: UsageSummaryDto {
                input_tokens_today,
                output_tokens_today,
                month_spent_usd: 0.0,
                monthly_budget_usd: self.monthly_budget_usd,
                daily: vec![DailyUsageDto {
                    label: "今天".into(),
                    input: input_tokens_today,
                    output: output_tokens_today,
                }],
            },
        })
    }

    async fn list_tasks(&self, principal: &WebPrincipal) -> Result<Vec<TaskDto>, WebApiError> {
        self.validate_principal(principal)?;
        let _guard = self.write_lock.lock().await;
        Ok(self
            .task_records()
            .await?
            .into_iter()
            .filter(|record| Self::can_access_record(principal, record))
            .map(|record| record.summary)
            .collect())
    }

    async fn task_events(
        &self,
        principal: &WebPrincipal,
        task_id: TaskId,
    ) -> Result<Vec<EventDto>, WebApiError> {
        self.validate_principal(principal)?;
        let _guard = self.write_lock.lock().await;
        Ok(self
            .load_accessible_task(principal, task_id)
            .await?
            .events
            .iter()
            .map(event_dto)
            .collect())
    }

    async fn can_access_task(&self, principal: &WebPrincipal, task_id: TaskId) -> bool {
        self.validate_principal(principal).is_ok()
            && self.load_accessible_task(principal, task_id).await.is_ok()
    }
}

#[async_trait]
impl WebCommandPort for KoiWebSource {
    /// 创建任务会话。与主会话的 `task.start` 工具走同一管理路径：先在主会话事件流中
    /// 记录跨任务操作请求与结果，再写入子任务生命周期与首条输入。
    async fn create_task(
        &self,
        principal: WebPrincipal,
        command: CreateTaskCommand,
    ) -> Result<TaskDto, WebApiError> {
        self.validate_principal(&principal)?;
        validate_create_command(&command)?;
        // 在创建子任务前先校验建议权限，避免恶意请求留下没有首条输入的孤儿任务。
        resolve_suggested_permission(&principal, command.suggested_permission)?;
        let _guard = self.write_lock.lock().await;

        let mut main = self.recover_main_runtime().await?;
        let mut created = self
            .task_manager
            .request_create_child(&mut main, None)
            .await
            .map_err(map_task_manager_error)?;
        let task_id = created.task_id;
        created
            .runtime
            .record(
                AgentEvent::control(ControlEvent::TaskCreated {
                    trigger_event_id: Some(created.accepted_event_id),
                }),
                Some(created.accepted_event_id),
            )
            .await
            .map_err(|error| WebApiError::conflict(error.to_string()))?;
        created
            .runtime
            .record(AgentEvent::control(ControlEvent::TaskQueued), None)
            .await
            .map_err(|error| WebApiError::conflict(error.to_string()))?;
        let _ingress = self
            .record_task_input(&mut created.runtime, &principal, &command)
            .await?;

        let events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        Ok(task_dto(task_id, &events, created.runtime.projection()))
    }

    async fn append_context(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: AppendContextCommand,
    ) -> Result<EventDto, WebApiError> {
        self.validate_principal(&principal)?;
        validate_context_command(&command)?;
        let suggested_permission =
            resolve_suggested_permission(&principal, command.suggested_permission)?;
        let _guard = self.write_lock.lock().await;
        let record = self.load_accessible_task(&principal, task_id).await?;
        let scope = task_scope(task_id, &record.events);
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let kind = match command.kind {
            WebContextKind::UserMessage => ContextKind::UserMessage,
            WebContextKind::Alert => ContextKind::Alert,
            WebContextKind::AssistantMessage => {
                return Err(WebApiError::validation(
                    "Web 来源不能伪造 assistant 上下文；模型输出由核心事件流产生",
                ));
            }
        };
        let recorded = self
            .record_context(
                &mut runtime,
                &principal,
                Scope::new(scope.kind, scope.id),
                kind,
                command.message,
                suggested_permission,
            )
            .await?;
        let dto = event_dto(&recorded);
        self.publish(&recorded);
        Ok(dto)
    }

    async fn request_cancellation(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: CancellationRequestCommand,
    ) -> Result<EventDto, WebApiError> {
        self.validate_principal(&principal)?;
        validate_reason(&command.reason, "取消原因")?;
        let suggested_permission =
            resolve_suggested_permission(&principal, command.suggested_permission)?;
        let _guard = self.write_lock.lock().await;
        let record = self.load_accessible_task(&principal, task_id).await?;
        let scope = task_scope(task_id, &record.events);
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let recorded = IngressRegistrar::new(&self.sources, &self.permissions)
            .register(
                &mut runtime,
                IngressDraft::Cancellation {
                    principal: Self::domain_principal(&principal),
                    scope: Scope::new(scope.kind, scope.id),
                    suggested_permission,
                    reason: command.reason,
                },
            )
            .await
            .map_err(map_ingress_error)?;
        let dto = event_dto(&recorded);
        self.publish(&recorded);
        Ok(dto)
    }

    async fn submit_approval(
        &self,
        principal: WebPrincipal,
        approval_request_event_id: EventId,
        command: ApprovalCommand,
    ) -> Result<ApprovalDto, WebApiError> {
        self.validate_principal(&principal)?;
        let suggested_permission =
            resolve_suggested_permission(&principal, command.suggested_permission)?;
        let _guard = self.write_lock.lock().await;
        let mut records = self.task_records().await?;
        let Some(record_index) = records.iter().position(|record| {
            record
                .events
                .iter()
                .any(|event| event.id == approval_request_event_id)
        }) else {
            return Err(WebApiError::not_found("授权请求事件不存在"));
        };
        let record = &records[record_index];
        if !Self::can_access_record(&principal, record) {
            return Err(WebApiError::not_found("授权请求事件不存在"));
        }
        let is_approval_request = record.events.iter().any(|event| {
            event.id == approval_request_event_id
                && matches!(event.payload, AgentEvent::Tool(ref tool) if matches!(tool.as_ref(), ToolEvent::ApprovalRequested { .. }))
        });
        if !is_approval_request {
            return Err(WebApiError::validation("该事件不是工具授权请求"));
        }
        if self.approval_dtos(&records).iter().any(|approval| {
            approval.approval_request_event_id == approval_request_event_id.to_string()
                && approval.status != "Pending"
        }) {
            return Err(WebApiError::conflict("该授权请求已经处理"));
        }

        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), record.task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let domain_principal = Self::domain_principal(&principal);
        let registrar = IngressRegistrar::new(&self.sources, &self.permissions);
        let submitted = registrar
            .register(
                &mut runtime,
                IngressDraft::Approval {
                    approval_request_event_id,
                    principal: domain_principal,
                    scope: Scope::new(
                        record.summary.scope.kind.clone(),
                        record.summary.scope.id.clone(),
                    ),
                    suggested_permission,
                    approved: command.approved,
                },
            )
            .await
            .map_err(map_ingress_error)?;
        self.publish(&submitted);

        let mut updated_events = record.events.clone();
        updated_events.push(submitted);
        records[record_index] = TaskRecord {
            task_id: record.task_id,
            minimum_control_permission: runtime.projection().minimum_control_permission,
            summary: task_dto(record.task_id, &updated_events, runtime.projection()),
            events: updated_events,
        };
        self.approval_dtos(&records)
            .into_iter()
            .find(|approval| {
                approval.approval_request_event_id == approval_request_event_id.to_string()
            })
            .ok_or_else(|| WebApiError::internal("写入后无法重建授权请求"))
    }

    async fn control_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: TaskControlCommand,
    ) -> Result<TaskDto, WebApiError> {
        self.validate_principal(&principal)?;
        let _guard = self.write_lock.lock().await;
        self.load_accessible_task(&principal, task_id).await?;
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|_| WebApiError::not_found(format!("任务 {task_id}")))?;
        let event = match command.action {
            TaskControlAction::Pause => {
                let reason = command.reason.unwrap_or_else(|| "由 Web 控制台发起".into());
                validate_reason(&reason, "控制原因")?;
                ControlEvent::TaskPaused { reason }
            }
            TaskControlAction::Resume => ControlEvent::TaskResumed,
            TaskControlAction::Cancel => {
                let reason = command.reason.unwrap_or_else(|| "由 Web 控制台发起".into());
                validate_reason(&reason, "控制原因")?;
                ControlEvent::TaskCancelled { reason }
            }
            TaskControlAction::SetMinimumPermission => {
                let minimum_permission = command.minimum_permission.ok_or_else(|| {
                    WebApiError::validation("调整最低控制权限时必须提供 minimumPermission")
                })?;
                ControlEvent::MinimumControlPermissionChanged { minimum_permission }
            }
            TaskControlAction::SelectModel => {
                let provider = command
                    .provider
                    .ok_or_else(|| WebApiError::validation("选择模型时必须提供 provider"))?;
                let model_id = command
                    .model_id
                    .ok_or_else(|| WebApiError::validation("选择模型时必须提供 modelId"))?;
                let selection = ModelSelection::new(provider, model_id).map_err(|error| {
                    WebApiError::validation(format!("provider/modelId 无效：{error}"))
                })?;
                if !self.models.contains(&selection) {
                    return Err(WebApiError::validation(format!(
                        "供应商模型未配置：{selection}"
                    )));
                }
                ControlEvent::ModelSelected {
                    provider: selection.provider,
                    model_id: selection.model_id,
                }
            }
        };
        let authority = DirectControlAuthority::external(
            SourceName::new(WEB_SOURCE_NAME)
                .map_err(|error| WebApiError::internal(error.to_string()))?,
            Self::domain_principal(&principal),
            principal.permission,
            None,
        )
        .map_err(|error| WebApiError::conflict(error.to_string()))?;
        let recorded = ControlExecutor::execute(
            &mut runtime,
            ControlExecutionRequest {
                event,
                authority,
                causation_id: None,
            },
        )
        .await
        .map_err(|error| WebApiError::conflict(error.to_string()))?;
        self.publish(&recorded);
        let events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        Ok(task_dto(task_id, &events, runtime.projection()))
    }

    /// 命名任务会话。与主会话的 `task.name` 工具走同一管理路径；主会话不可命名。
    async fn name_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
        command: NameTaskCommand,
    ) -> Result<TaskDto, WebApiError> {
        self.validate_principal(&principal)?;
        let name = command.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 128 {
            return Err(WebApiError::validation("任务名称必须为 1 到 128 个字符"));
        }
        let _guard = self.write_lock.lock().await;
        self.load_accessible_task(&principal, task_id).await?;
        let mut main = self.recover_main_runtime().await?;
        self.task_manager
            .request_name_child(&mut main, task_id, &name, None)
            .await
            .map_err(map_task_manager_error)?;
        let events = self
            .store
            .load_task(task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        let runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(|error| WebApiError::internal(error.to_string()))?;
        Ok(task_dto(task_id, &events, runtime.projection()))
    }

    /// 删除已终止的任务会话。与主会话的 `task.delete` 工具走同一管理路径；主会话不可删除。
    async fn delete_task(
        &self,
        principal: WebPrincipal,
        task_id: TaskId,
    ) -> Result<DeletedTaskDto, WebApiError> {
        self.validate_principal(&principal)?;
        if !matches!(
            principal.permission,
            PermissionLevel::Operator | PermissionLevel::Admin | PermissionLevel::System
        ) {
            return Err(WebApiError::Forbidden(
                "删除任务会话需要 Operator 或更高权限".into(),
            ));
        }
        let _guard = self.write_lock.lock().await;
        self.load_accessible_task(&principal, task_id).await?;
        let mut main = self.recover_main_runtime().await?;
        self.task_manager
            .request_delete_child(&mut main, task_id, "由 Web 控制台删除", None)
            .await
            .map_err(map_task_manager_error)?;
        Ok(DeletedTaskDto {
            task_id: task_id.to_string(),
            deleted: true,
        })
    }
}

impl WebEventPort for KoiWebSource {
    fn subscribe(&self) -> broadcast::Receiver<WebStreamEvent> {
        self.events.subscribe()
    }
}

struct WebAuthorizationProvider {
    source: Arc<KoiWebSource>,
}

#[async_trait]
impl SourceAuthorizationProvider for WebAuthorizationProvider {
    fn source(&self) -> &'static str {
        WEB_SOURCE_NAME
    }

    async fn request_authorization(
        &self,
        request: AuthorizationRequest,
    ) -> Result<AuthorizationRequestResult, AuthorizationError> {
        self.source.request_elevation(request).await
    }
}

#[derive(Clone)]
struct TaskRecord {
    task_id: TaskId,
    minimum_control_permission: PermissionLevel,
    summary: TaskDto,
    events: Vec<EventEnvelope>,
}

struct WebPermissionResolver {
    identities: Arc<WebUserStore>,
}

#[async_trait]
impl IngressPermissionResolver for WebPermissionResolver {
    async fn maximum_permission(
        &self,
        subject: IngressSubject,
    ) -> Result<PermissionLevel, IngressRegistrationError> {
        if subject.source != WEB_SOURCE_NAME {
            return Err(IngressRegistrationError::permission_resolution(
                "来源不是 web",
            ));
        }
        let Some(principal) = subject.principal else {
            return Err(IngressRegistrationError::permission_resolution(
                "Web 输入缺少身份",
            ));
        };
        if principal.source != WEB_SOURCE_NAME {
            return Err(IngressRegistrationError::permission_resolution(
                "Web 身份来源不匹配",
            ));
        }
        self.identities
            .permission_for(&principal.subject)
            .ok_or_else(|| IngressRegistrationError::permission_resolution("Web 身份未授权"))
    }
}

fn validate_create_command(command: &CreateTaskCommand) -> Result<(), WebApiError> {
    if command.message.trim().is_empty() || command.message.chars().count() > 8_000 {
        return Err(WebApiError::validation("任务描述必须为 1 到 8000 个字符"));
    }
    if command.scope.kind.trim().is_empty()
        || command.scope.id.trim().is_empty()
        || command.scope.kind.chars().count() > 128
        || command.scope.id.chars().count() > 256
    {
        return Err(WebApiError::validation(
            "scope.kind 与 scope.id 必须在允许长度内",
        ));
    }
    Ok(())
}

/// 校验 Web 请求中携带的建议授权等级。
///
/// 该值来自 HTTP 请求，不能作为事实权限直接信任；它只能在当前认证身份权限以内
/// 选择，之后仍由核心来源注册表和身份解析器再次截断并记录最终权限。
fn resolve_suggested_permission(
    principal: &WebPrincipal,
    suggested_permission: Option<PermissionLevel>,
) -> Result<PermissionLevel, WebApiError> {
    let suggested_permission = suggested_permission.unwrap_or(principal.permission);
    if !suggested_permission.allows(PermissionLevel::User) {
        return Err(WebApiError::validation("建议授权等级必须为 User 或更高"));
    }
    if !principal.permission.allows(suggested_permission) {
        return Err(WebApiError::Forbidden(format!(
            "建议授权等级 {suggested_permission:?} 超过当前身份权限 {:?}",
            principal.permission
        )));
    }
    Ok(suggested_permission)
}

fn validate_context_command(command: &AppendContextCommand) -> Result<(), WebApiError> {
    if command.message.trim().is_empty() || command.message.chars().count() > 8_000 {
        return Err(WebApiError::validation("事件内容必须为 1 到 8000 个字符"));
    }
    Ok(())
}

fn validate_reason(reason: &str, label: &str) -> Result<(), WebApiError> {
    if reason.trim().is_empty() || reason.chars().count() > 512 {
        return Err(WebApiError::validation(format!(
            "{label}必须为 1 到 512 个字符"
        )));
    }
    Ok(())
}

fn map_ingress_error(error: IngressRegistrationError) -> WebApiError {
    match error {
        IngressRegistrationError::UnregisteredSource(_)
        | IngressRegistrationError::PermissionResolution { .. } => {
            WebApiError::Forbidden(error.to_string())
        }
        IngressRegistrationError::Runtime(error) => WebApiError::conflict(error.to_string()),
        IngressRegistrationError::InvalidSourceName(error) => {
            WebApiError::validation(error.to_string())
        }
    }
}

fn map_task_manager_error(error: TaskManagerError) -> WebApiError {
    match error {
        TaskManagerError::OperationRejected(reason) => WebApiError::conflict(reason),
        TaskManagerError::TaskAlreadyRunning(task_id) => {
            WebApiError::conflict(format!("任务已在当前进程运行：{task_id}"))
        }
        TaskManagerError::LockPoisoned => WebApiError::internal("任务管理器状态锁已中毒"),
        TaskManagerError::Recovery(error) => WebApiError::not_found(error.to_string()),
        TaskManagerError::Runtime(error) => WebApiError::conflict(error.to_string()),
        TaskManagerError::EventStore(error) => WebApiError::internal(error.to_string()),
    }
}

fn task_dto(task_id: TaskId, events: &[EventEnvelope], projection: &TaskProjection) -> TaskDto {
    let last_event = events.last().map(event_dto);
    let started_at = events.first().map_or_else(
        || Utc::now().to_rfc3339(),
        |event| event.recorded_at.to_rfc3339(),
    );
    let updated_at = events.last().map_or_else(
        || started_at.clone(),
        |event| event.recorded_at.to_rfc3339(),
    );
    TaskDto {
        task_id: task_id.to_string(),
        is_main: task_id.is_main(),
        title: projection
            .title
            .clone()
            .unwrap_or_else(|| task_title(task_id, events)),
        status: format!("{:?}", projection.status),
        source: task_source(events),
        scope: task_scope(task_id, events),
        started_at,
        updated_at,
        last_event_kind: last_event
            .as_ref()
            .map_or_else(|| "system".into(), |event| event.kind.clone()),
        last_event_summary: last_event
            .map_or_else(|| "任务尚未写入事件".into(), |event| event.summary),
        minimum_control_permission: permission_name(projection.minimum_control_permission),
        selected_model: projection.selected_model.as_ref().map(model_selection_dto),
        usage: UsageDto {
            input_tokens: projection.usage.input_tokens,
            output_tokens: projection.usage.output_tokens,
            cached_input_tokens: projection.usage.cached_input_tokens,
            reasoning_tokens: projection.usage.reasoning_tokens,
        },
        event_count: events.len(),
    }
}

fn model_selection_dto(selection: &ModelSelection) -> ModelSelectionDto {
    ModelSelectionDto {
        provider: selection.provider.clone(),
        model_id: selection.model_id.clone(),
    }
}

fn task_title(task_id: TaskId, events: &[EventEnvelope]) -> String {
    if task_id.is_main() {
        return "主会话".into();
    }
    events
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Ingress(ingress) => match ingress.as_ref() {
                IngressEvent::ContextReceived { context, .. } => match &context.payload {
                    ContextPayload::Text { text, .. } => Some(truncate(text, 42)),
                    ContextPayload::Alert { summary, .. } => Some(truncate(summary, 42)),
                    ContextPayload::Structured(_) => Some("结构化 Web 输入".into()),
                },
                _ => None,
            },
            _ => None,
        })
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| format!("任务 {}", &task_id.to_string()[..8]))
}

fn task_source(events: &[EventEnvelope]) -> String {
    events
        .iter()
        .find_map(|event| match &event.provenance.creator {
            EventSource::External(source) => Some(source.as_str().into()),
            _ => None,
        })
        .unwrap_or_else(|| "system".into())
}

fn task_scope(task_id: TaskId, events: &[EventEnvelope]) -> ScopeDto {
    events
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Ingress(ingress) => match ingress.as_ref() {
                IngressEvent::ContextReceived { context, .. } => Some(ScopeDto {
                    kind: context.scope.kind.clone(),
                    id: context.scope.id.clone(),
                }),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or_else(|| ScopeDto {
            kind: "task".into(),
            id: task_id.to_string(),
        })
}

fn event_dto(event: &EventEnvelope) -> EventDto {
    let (kind, title, summary) = event_description(&event.payload);
    EventDto {
        id: event.id.to_string(),
        task_id: event.task_id.to_string(),
        sequence: event.sequence,
        occurred_at: event.occurred_at.to_rfc3339(),
        source: event.provenance.creator.as_str().into(),
        kind: kind.into(),
        title,
        summary,
        permission: event
            .provenance
            .direct_permission
            .map_or_else(|| "None".into(), permission_name),
    }
}

#[allow(clippy::too_many_lines)]
fn event_description(event: &AgentEvent) -> (&'static str, String, String) {
    match event {
        AgentEvent::Ingress(ingress) => match ingress.as_ref() {
            IngressEvent::ContextReceived { context, .. } => {
                let summary = match &context.payload {
                    ContextPayload::Text { text, .. } => truncate(text, 180),
                    ContextPayload::Alert { summary, .. } => truncate(summary, 180),
                    ContextPayload::Structured(_) => "收到结构化上下文".into(),
                };
                ("ingress", "收到外部上下文".into(), summary)
            }
            IngressEvent::ApprovalSubmitted { approved, .. } => (
                "approval",
                "Web 已提交授权决定".into(),
                if *approved {
                    "已批准工具操作".into()
                } else {
                    "已拒绝工具操作".into()
                },
            ),
            IngressEvent::CancellationRequested { reason, .. } => {
                ("ingress", "收到取消请求".into(), truncate(reason, 180))
            }
        },
        AgentEvent::Control(control) => match control.as_ref() {
            ControlEvent::TaskCreated { .. } => (
                "control",
                "任务已创建".into(),
                "核心已记录任务生命周期起点".into(),
            ),
            ControlEvent::TaskQueued => (
                "control",
                "任务已进入队列".into(),
                "等待 Agent 运行器接管".into(),
            ),
            ControlEvent::TaskPaused { reason } => {
                ("control", "任务已暂停".into(), truncate(reason, 180))
            }
            ControlEvent::TaskResumed => (
                "control",
                "任务已恢复".into(),
                "任务已恢复为运行状态".into(),
            ),
            ControlEvent::TaskNamed { name } => {
                ("control", "任务已命名".into(), truncate(name, 180))
            }
            ControlEvent::ModelSelected { provider, model_id } => (
                "control",
                "会话模型已切换".into(),
                format!("后续模型调用使用 {provider}/{model_id}"),
            ),
            ControlEvent::TaskCancelled { reason } => {
                ("control", "任务已取消".into(), truncate(reason, 180))
            }
            ControlEvent::TaskCompleted { response } => (
                "control",
                "任务已完成".into(),
                response
                    .clone()
                    .unwrap_or_else(|| "任务未返回文本摘要".into()),
            ),
            ControlEvent::TaskFailed { reason } => {
                ("control", "任务失败".into(), truncate(reason, 180))
            }
            ControlEvent::TaskExpired { reason } => {
                ("control", "任务已过期".into(), truncate(reason, 180))
            }
            ControlEvent::MinimumControlPermissionChanged { minimum_permission } => (
                "control",
                "最低控制权限已修改".into(),
                permission_name(*minimum_permission),
            ),
            ControlEvent::TaskOperationRequested { .. } => (
                "control",
                "请求跨任务操作".into(),
                "主会话正在请求任务管理操作".into(),
            ),
            ControlEvent::TaskOperationAccepted { .. } => (
                "control",
                "跨任务操作已接受".into(),
                "核心已接受任务管理操作".into(),
            ),
            ControlEvent::TaskOperationRejected { reason, .. } => {
                ("control", "跨任务操作被拒绝".into(), truncate(reason, 180))
            }
            ControlEvent::BudgetExceeded { budget, consumed } => (
                "control",
                "任务超出预算".into(),
                format!("预算 {budget}，已消耗 {consumed}"),
            ),
            ControlEvent::ContextCompacted { .. } => (
                "control",
                "上下文已压缩".into(),
                "核心已压缩历史上下文".into(),
            ),
        },
        AgentEvent::Model(model) => match model.as_ref() {
            koi_core::domain::ModelEvent::CallStarted {
                provider, model_id, ..
            } => (
                "model",
                "模型调用开始".into(),
                format!("正在调用 {provider}/{model_id}"),
            ),
            koi_core::domain::ModelEvent::Delta { content, .. } => {
                ("model", "模型输出增量".into(), truncate(content, 180))
            }
            koi_core::domain::ModelEvent::Completed { outputs, .. } => {
                let text = outputs
                    .iter()
                    .filter_map(|output| match output {
                        koi_core::domain::ModelOutput::Text { text } => Some(text.as_str()),
                        koi_core::domain::ModelOutput::Refusal { reason } => Some(reason.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    (
                        "model",
                        "模型调用完成".into(),
                        "已记录模型输出与用量".into(),
                    )
                } else {
                    ("model", "Agent 回复".into(), truncate(&text, 4_000))
                }
            }
            koi_core::domain::ModelEvent::Failed { error, .. } => {
                ("model", "模型调用失败".into(), truncate(error, 180))
            }
        },
        AgentEvent::Tool(tool) => match tool.as_ref() {
            ToolEvent::Proposed { tool_call } => {
                ("tool", "模型提出工具调用".into(), tool_call.name.clone())
            }
            ToolEvent::Validated { .. } => (
                "tool",
                "工具参数已校验".into(),
                "工具调用通过结构化参数校验".into(),
            ),
            ToolEvent::AuthorizationChecked { decision, .. } => (
                "tool",
                "工具授权已检查".into(),
                format!("决策：{decision:?}"),
            ),
            ToolEvent::ApprovalRequested { .. } => (
                "approval",
                "需要人工授权".into(),
                "工具调用等待来源方确认".into(),
            ),
            ToolEvent::Started { .. } => ("tool", "工具执行开始".into(), "已获得执行许可".into()),
            ToolEvent::Output { content, .. } => {
                ("tool", "工具输出".into(), truncate(content, 180))
            }
            ToolEvent::Finished { result, .. } => (
                "tool",
                "工具执行完成".into(),
                truncate(&result.summary, 180),
            ),
            ToolEvent::Failed { error, .. } => {
                ("tool", "工具执行失败".into(), truncate(error, 180))
            }
            ToolEvent::Cancelled { reason, .. } => {
                ("tool", "工具执行已取消".into(), truncate(reason, 180))
            }
            ToolEvent::NotificationSent { channel, .. } => {
                ("tool", "已发送通知".into(), format!("通知渠道：{channel}"))
            }
        },
    }
}

fn permission_name(permission: PermissionLevel) -> String {
    format!("{permission:?}")
}

fn fingerprint(input: &str) -> String {
    let mut value = 0xcbf2_9ce4_8422_2325_u64;
    for byte in input.as_bytes() {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{value:016x}")
}

fn truncate(input: &str, limit: usize) -> String {
    let mut characters = input.chars();
    let truncated: String = characters.by_ref().take(limit).collect();
    if characters.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koi_api::{RegisterUserCommand, WebIdentityProvider};

    /// 测试辅助：构造与生产一致的共享任务管理器。
    fn test_task_manager(store: &Arc<JsonlEventStore>) -> Arc<TaskManager<Arc<JsonlEventStore>>> {
        Arc::new(TaskManager::new(Arc::new(Arc::clone(store))))
    }

    /// 测试辅助：初始化主会话事件流（`TaskCreated` + `TaskQueued`）。
    async fn bootstrap_main_session(store: &Arc<JsonlEventStore>) {
        let mut runtime = TaskRuntime::new(Arc::clone(store), TaskId::MAIN);
        runtime
            .record(
                AgentEvent::control(ControlEvent::TaskCreated {
                    trigger_event_id: None,
                }),
                None,
            )
            .await
            .unwrap();
        runtime
            .record(AgentEvent::control(ControlEvent::TaskQueued), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn web_task_is_registered_through_core_ingress() {
        let directory = std::env::temp_dir().join(format!("koi-web-source-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
        bootstrap_main_session(&store).await;
        let identities =
            Arc::new(WebUserStore::open(directory.join("users.json"), "test-admin").unwrap());
        let source = KoiWebSource::new(
            Arc::clone(&store),
            identities,
            test_task_manager(&store),
            Vec::new(),
            10.0,
        )
        .unwrap();

        let task = source
            .create_task(
                WebPrincipal::admin("web-admin", Some("Web Admin".into())),
                CreateTaskCommand {
                    message: "检查 order-api 的连接池状态".into(),
                    scope: ScopeDto {
                        kind: "service".into(),
                        id: "order-api".into(),
                    },
                    suggested_permission: Some(PermissionLevel::User),
                },
            )
            .await
            .unwrap();

        let task_id = uuid::Uuid::parse_str(&task.task_id).map(TaskId).unwrap();
        let events = store.load_task(task_id).await.unwrap();
        assert_eq!(events.len(), 3);
        let AgentEvent::Control(ref created) = events[0].payload else {
            panic!("首事件必须是 TaskCreated");
        };
        let ControlEvent::TaskCreated {
            trigger_event_id: Some(trigger),
        } = created.as_ref()
        else {
            panic!("Web 创建的子任务必须绑定主会话 task.start 审计事件");
        };
        // 主会话流中应有对应的 TaskOperationRequested/Accepted 审计。
        let main_events = store.load_task(TaskId::MAIN).await.unwrap();
        assert!(main_events.iter().any(|event| {
            event.id == *trigger
                && matches!(
                    event.payload,
                    AgentEvent::Control(ref control)
                        if matches!(control.as_ref(), ControlEvent::TaskOperationAccepted { .. })
                )
        }));
        assert!(
            matches!(events[1].payload, AgentEvent::Control(ref event) if matches!(event.as_ref(), ControlEvent::TaskQueued))
        );
        assert!(
            matches!(events[2].payload, AgentEvent::Ingress(ref event) if matches!(event.as_ref(), IngressEvent::ContextReceived { .. }))
        );
        assert_eq!(
            events[2].provenance.creator,
            EventSource::External(SourceName::new(WEB_SOURCE_NAME).unwrap())
        );
        assert_eq!(
            events[2].provenance.direct_permission,
            Some(PermissionLevel::User)
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn web_elevation_provider_announces_only_persisted_approval_requests() {
        let directory = std::env::temp_dir().join(format!("koi-web-elevation-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
        bootstrap_main_session(&store).await;
        let identities =
            Arc::new(WebUserStore::open(directory.join("users.json"), "test-admin").unwrap());
        let source = Arc::new(
            KoiWebSource::new(
                Arc::clone(&store),
                identities,
                test_task_manager(&store),
                Vec::new(),
                10.0,
            )
            .unwrap(),
        );
        let task = source
            .create_task(
                WebPrincipal::admin("web-admin", Some("Web Admin".into())),
                CreateTaskCommand {
                    message: "检查 order-api".into(),
                    scope: ScopeDto {
                        kind: "service".into(),
                        id: "order-api".into(),
                    },
                    suggested_permission: None,
                },
            )
            .await
            .unwrap();
        let task_id = uuid::Uuid::parse_str(&task.task_id).map(TaskId).unwrap();
        let mut runtime = TaskRuntime::recover(Arc::clone(&store), task_id)
            .await
            .unwrap();
        runtime
            .record(AgentEvent::control(ControlEvent::TaskResumed), None)
            .await
            .unwrap();
        let proposed = runtime
            .record(
                AgentEvent::tool(ToolEvent::Proposed {
                    tool_call: koi_core::domain::ToolCall {
                        name: "service.restart".into(),
                        arguments: serde_json::json!({"service": "order-api"}),
                        provider_call_id: None,
                        authority_parent_event_id: None,
                    },
                }),
                None,
            )
            .await
            .unwrap();
        let approval = runtime
            .record(
                AgentEvent::tool(ToolEvent::ApprovalRequested {
                    proposal_event_id: proposed.id,
                }),
                Some(proposed.id),
            )
            .await
            .unwrap();

        let mut events = source.subscribe();
        let outcome = source
            .authorization_provider()
            .request_authorization(AuthorizationRequest {
                task_id,
                approval_request_event_id: approval.id,
                tool_proposal_event_id: proposed.id,
                tool_name: "service.restart".into(),
                arguments_hash: "fnv1a64:example".into(),
                required_permission: PermissionLevel::Operator,
                original_evidence_event_ids: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!(outcome, AuthorizationRequestResult::Pending);
        assert!(matches!(
            events.try_recv().unwrap(),
            WebStreamEvent::AuthorizationRequested { request }
                if request.approval_request_event_id == approval.id.to_string()
                    && request.tool_proposal_event_id == proposed.id.to_string()
        ));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn users_can_read_tasks_when_minimum_permission_allows() {
        let directory = std::env::temp_dir().join(format!("koi-web-visibility-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
        bootstrap_main_session(&store).await;
        let identities =
            Arc::new(WebUserStore::open(directory.join("users.json"), "test-admin").unwrap());
        let alice = identities
            .register(RegisterUserCommand {
                email: "alice@example.test".into(),
                username: "alice_ops".into(),
                password: "correct horse battery staple".into(),
            })
            .unwrap()
            .principal;
        let bob = identities
            .register(RegisterUserCommand {
                email: "bob@example.test".into(),
                username: "bob_ops".into(),
                password: "correct horse battery staple".into(),
            })
            .unwrap()
            .principal;
        let source = KoiWebSource::new(
            Arc::clone(&store),
            identities,
            test_task_manager(&store),
            Vec::new(),
            10.0,
        )
        .unwrap();
        let task = source
            .create_task(
                alice.clone(),
                CreateTaskCommand {
                    message: "检查订单服务".into(),
                    scope: ScopeDto {
                        kind: "service".into(),
                        id: "orders".into(),
                    },
                    suggested_permission: None,
                },
            )
            .await
            .unwrap();
        let task_id = uuid::Uuid::parse_str(&task.task_id).map(TaskId).unwrap();

        assert_eq!(source.list_tasks(&alice).await.unwrap().len(), 2);
        assert_eq!(source.list_tasks(&bob).await.unwrap().len(), 2);
        assert!(source.task_events(&bob, task_id).await.is_ok());
        assert!(source.can_access_task(&alice, task_id).await);
        assert!(source.can_access_task(&bob, task_id).await);

        source
            .control_task(
                WebPrincipal::admin("web-admin", Some("Web Admin".into())),
                task_id,
                TaskControlCommand {
                    action: TaskControlAction::SetMinimumPermission,
                    reason: None,
                    minimum_permission: Some(PermissionLevel::Admin),
                    provider: None,
                    model_id: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(source.list_tasks(&alice).await.unwrap().len(), 1);
        assert_eq!(source.list_tasks(&bob).await.unwrap().len(), 1);
        assert!(source.task_events(&bob, task_id).await.is_err());
        assert!(!source.can_access_task(&alice, task_id).await);
        assert!(
            source
                .can_access_task(
                    &WebPrincipal::admin("web-admin", Some("Web Admin".into())),
                    task_id,
                )
                .await
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn users_can_read_tasks_created_by_the_main_model_from_their_web_input() {
        let directory =
            std::env::temp_dir().join(format!("koi-web-derived-owner-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
        bootstrap_main_session(&store).await;
        let identities =
            Arc::new(WebUserStore::open(directory.join("users.json"), "test-admin").unwrap());
        let alice = identities
            .register(RegisterUserCommand {
                email: "alice@example.test".into(),
                username: "alice_ops".into(),
                password: "correct horse battery staple".into(),
            })
            .unwrap()
            .principal;
        let source = KoiWebSource::new(
            Arc::clone(&store),
            identities,
            test_task_manager(&store),
            Vec::new(),
            10.0,
        )
        .unwrap();

        let mut main = TaskRuntime::recover(Arc::clone(&store), TaskId::MAIN)
            .await
            .unwrap();
        let ingress = source
            .record_task_input(
                &mut main,
                &alice,
                &CreateTaskCommand {
                    message: "请启动一个只读检查子任务".into(),
                    scope: ScopeDto {
                        kind: "service".into(),
                        id: "orders".into(),
                    },
                    suggested_permission: None,
                },
            )
            .await
            .unwrap();
        let proposed = main
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Proposed {
                    tool_call: koi_core::domain::ToolCall {
                        name: "task.start".into(),
                        arguments: serde_json::json!({ "message": "只读检查" }),
                        provider_call_id: Some("call-test".into()),
                        authority_parent_event_id: Some(ingress.id),
                    },
                }),
                None,
                koi_core::domain::EventProvenance::model(Some(ingress.id)),
            )
            .await
            .unwrap();
        let validated = main
            .record(
                AgentEvent::tool(ToolEvent::Validated {
                    proposal_event_id: proposed.id,
                }),
                Some(proposed.id),
            )
            .await
            .unwrap();
        let requested = main
            .record(
                AgentEvent::control(ControlEvent::TaskOperationRequested {
                    operation: koi_core::domain::TaskOperation::CreateChild,
                }),
                Some(validated.id),
            )
            .await
            .unwrap();
        let child_id = TaskId::new();
        let accepted = main
            .record(
                AgentEvent::control(ControlEvent::TaskOperationAccepted {
                    request_event_id: requested.id,
                    target_task_id: child_id,
                }),
                Some(requested.id),
            )
            .await
            .unwrap();
        let started = main
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Started {
                    proposal_event_id: proposed.id,
                }),
                Some(accepted.id),
                koi_core::domain::EventProvenance::tool(),
            )
            .await
            .unwrap();

        let mut child = TaskRuntime::new(Arc::clone(&store), child_id);
        child
            .record(
                AgentEvent::control(ControlEvent::TaskCreated {
                    trigger_event_id: Some(started.id),
                }),
                Some(started.id),
            )
            .await
            .unwrap();
        child
            .record(AgentEvent::control(ControlEvent::TaskQueued), None)
            .await
            .unwrap();

        assert!(source.can_access_task(&alice, child_id).await);
        assert_eq!(source.list_tasks(&alice).await.unwrap().len(), 2);

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn web_source_names_and_deletes_children_through_task_manager() {
        let directory = std::env::temp_dir().join(format!("koi-web-manage-{}", EventId::new()));
        let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
        bootstrap_main_session(&store).await;
        let identities =
            Arc::new(WebUserStore::open(directory.join("users.json"), "test-admin").unwrap());
        let source = Arc::new(
            KoiWebSource::new(
                Arc::clone(&store),
                identities,
                test_task_manager(&store),
                Vec::new(),
                10.0,
            )
            .unwrap()
            .with_model_catalog(
                [
                    ModelSelection::new("openai", "gpt-5-mini").unwrap(),
                    ModelSelection::new("deepseek", "deepseek-chat").unwrap(),
                ],
                ModelSelection::new("openai", "gpt-5-mini").unwrap(),
            ),
        );
        let admin = WebPrincipal::admin("web-admin", Some("Web Admin".into()));
        let task = source
            .create_task(
                admin.clone(),
                CreateTaskCommand {
                    message: "检查 order-api".into(),
                    scope: ScopeDto {
                        kind: "service".into(),
                        id: "order-api".into(),
                    },
                    suggested_permission: None,
                },
            )
            .await
            .unwrap();
        let task_id = uuid::Uuid::parse_str(&task.task_id).map(TaskId).unwrap();

        let selected = source
            .control_task(
                admin.clone(),
                task_id,
                TaskControlCommand {
                    action: TaskControlAction::SelectModel,
                    reason: None,
                    minimum_permission: None,
                    provider: Some("deepseek".into()),
                    model_id: Some("deepseek-chat".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            selected.selected_model,
            Some(ModelSelectionDto {
                provider: "deepseek".into(),
                model_id: "deepseek-chat".into(),
            })
        );
        let task_events = store.load_task(task_id).await.unwrap();
        assert!(task_events.iter().any(|event| {
            matches!(
                &event.payload,
                AgentEvent::Control(control)
                    if matches!(
                        control.as_ref(),
                        ControlEvent::ModelSelected { provider, model_id }
                            if provider == "deepseek" && model_id == "deepseek-chat"
                    )
            )
        }));

        // 命名：返回的 DTO 标题来自 TaskNamed 投影。
        let named = source
            .name_task(
                admin.clone(),
                task_id,
                NameTaskCommand {
                    name: "磁盘巡检".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(named.title, "磁盘巡检");

        // 主会话不可命名、不可删除。
        assert!(
            source
                .name_task(
                    admin.clone(),
                    TaskId::MAIN,
                    NameTaskCommand {
                        name: "主会话".into()
                    },
                )
                .await
                .is_err()
        );
        assert!(
            source
                .delete_task(admin.clone(), TaskId::MAIN)
                .await
                .is_err()
        );

        // 未终止的子任务不能删除。
        assert!(source.delete_task(admin.clone(), task_id).await.is_err());

        // 终止后可以删除，事件流随之移除。
        let mut runtime = TaskRuntime::recover(Arc::clone(&store), task_id)
            .await
            .unwrap();
        runtime
            .record(AgentEvent::control(ControlEvent::TaskResumed), None)
            .await
            .unwrap();
        runtime
            .record(
                AgentEvent::control(ControlEvent::TaskCompleted {
                    response: Some("完成".into()),
                }),
                None,
            )
            .await
            .unwrap();
        let deleted = source.delete_task(admin, task_id).await.unwrap();
        assert!(deleted.deleted);
        assert!(store.load_task(task_id).await.unwrap().is_empty());

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn suggested_permission_cannot_exceed_authenticated_identity() {
        let user = WebPrincipal {
            subject: "alice_ops".into(),
            display_name: Some("alice_ops".into()),
            permission: PermissionLevel::User,
        };
        assert_eq!(
            resolve_suggested_permission(&user, Some(PermissionLevel::User)).unwrap(),
            PermissionLevel::User
        );
        assert!(matches!(
            resolve_suggested_permission(&user, Some(PermissionLevel::Operator)),
            Err(WebApiError::Forbidden(_))
        ));
        assert!(matches!(
            resolve_suggested_permission(&user, Some(PermissionLevel::None)),
            Err(WebApiError::Validation(_))
        ));
    }
}
