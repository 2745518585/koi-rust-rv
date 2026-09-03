use std::collections::HashSet;

use futures_util::StreamExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{InputInjectionError, InputInjector, RuntimeError, TaskRuntime};
use crate::domain::{
    AgentEvent, AuthorizationRequest, AuthorizationRequestResult, AuthorizedToolInvocation,
    ControlEvent, EventId, MemoryContextBuilder, MemoryQuery, ModelContextItem, ModelError,
    ModelEvent, ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract,
    ModelRequest, ModelStreamEvent, PermissionCheckResult, PermissionChecker, PermissionLevel,
    PolicyDecision, TaskStatus, ToolCall, ToolEvent, ToolResult,
};
use crate::ports::{
    AuthorizationEvidenceResolver, EventStore, MemoryError, MemoryStore, ModelProvider,
    SourceAuthorizationRegistry, ToolRegistry,
};

/// 执行一个新 Agent 任务所需的供应商无关输入。
#[derive(Clone, Debug)]
pub struct AgentRunRequest {
    /// 触发本任务的已持久化输入事件；用于审计任务创建原因。
    pub trigger_event_id: Option<EventId>,
    /// 调用方已持久化并筛选好的当前上下文，模型只能引用其中的事件作为证据。
    pub context: Vec<ModelContextItem>,
    /// 需要由核心检查后注入的已持久化输入事件。
    pub input_events: Vec<crate::domain::EventEnvelope>,
    pub memory_query: Option<MemoryQuery>,
    pub output_contract: ModelOutputContract,
    pub model_options: ModelGenerationOptions,
    pub max_model_turns: u16,
}

impl AgentRunRequest {
    /// # Errors
    ///
    /// 当最大模型轮数为零或记忆查询不合法时返回错误。
    pub fn validate(&self) -> Result<(), AgentRunRequestError> {
        if self.max_model_turns == 0 {
            return Err(AgentRunRequestError::ZeroModelTurns);
        }
        if let Some(query) = &self.memory_query {
            query.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum AgentRunRequestError {
    #[error("最大模型轮数必须大于零")]
    ZeroModelTurns,
    #[error(transparent)]
    InvalidMemoryQuery(#[from] crate::domain::MemoryQueryValidationError),
}

/// 一次主循环的终止原因与可发送给调用方的最终文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentRunOutcome {
    Completed { response: Option<String> },
    AwaitingAuthorization { approval_request_event_id: EventId },
    Cancelled,
}

/// 将模型、工具、权限与记忆端口编排为单任务主循环。
pub struct AgentLoop<'a> {
    model: &'a dyn ModelProvider,
    tools: &'a ToolRegistry,
    evidence_resolver: &'a dyn AuthorizationEvidenceResolver,
    authorization_providers: &'a SourceAuthorizationRegistry,
    memory: Option<&'a dyn MemoryStore>,
    prompts: &'a dyn crate::ports::SystemPromptProvider,
}

impl<'a> AgentLoop<'a> {
    #[must_use]
    pub const fn new(
        model: &'a dyn ModelProvider,
        tools: &'a ToolRegistry,
        evidence_resolver: &'a dyn AuthorizationEvidenceResolver,
        authorization_providers: &'a SourceAuthorizationRegistry,
        memory: Option<&'a dyn MemoryStore>,
        prompts: &'a dyn crate::ports::SystemPromptProvider,
    ) -> Self {
        Self {
            model,
            tools,
            evidence_resolver,
            authorization_providers,
            memory,
            prompts,
        }
    }

    /// 运行一个从 `TaskStatus::New` 开始的新任务。
    ///
    /// 模型产生的工具调用始终先记录，再验证其证据和权限。权限不足时，核心记录授权
    /// 请求并按来源路由提权；来源返回的新输入事件仍会被重新审查。
    ///
    /// # Errors
    ///
    /// 当任务状态、请求、事件持久化、模型、记忆或来源授权基础设施发生错误时返回错误。
    pub async fn run<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        self.run_internal(runtime, request, cancel, false).await
    }

    /// 运行或继续运行连续主会话。
    ///
    /// 主会话在给出一轮回复后保持可继续状态，不记录 `TaskCompleted`；后续群聊输入和子任务
    /// 回传结果可以继续进入同一会话。
    ///
    /// # Errors
    ///
    /// 当运行时不是主会话、请求非法或底层模型/存储失败时返回错误。
    pub async fn run_main<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        if !runtime.projection().task_id.is_main() {
            return Err(AgentLoopError::MainTaskRequired);
        }
        self.run_internal(runtime, request, cancel, true).await
    }

    async fn run_internal<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
        is_main_session: bool,
    ) -> Result<AgentRunOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        request.validate()?;
        let status = runtime.projection().status;
        if status == TaskStatus::New {
            self.prepare_task(runtime, request.trigger_event_id).await?;
        } else if !is_main_session || status.is_terminal() {
            return Err(AgentLoopError::TaskNotNew(runtime.projection().status));
        }
        let mut context = self
            .build_initial_context(runtime.projection().task_id, &request)
            .await?;

        let mut final_text = Vec::new();
        for _ in 0..request.max_model_turns {
            if cancel.is_cancelled() {
                self.record_cancelled(runtime, "任务已取消").await?;
                return Ok(AgentRunOutcome::Cancelled);
            }

            let completed = match self
                .call_model(runtime, &request, &context, cancel.clone())
                .await
            {
                Ok(completed) => completed,
                Err(AgentLoopError::Cancelled) => return Ok(AgentRunOutcome::Cancelled),
                Err(error) => return Err(error),
            };

            let mut tool_calls = Vec::new();
            for output in completed.outputs {
                match output {
                    ModelOutput::Text { text } => final_text.push(text),
                    ModelOutput::Refusal { reason } => final_text.push(reason),
                    ModelOutput::ToolCall(tool_call) => tool_calls.push(tool_call),
                    ModelOutput::InterventionDecision { .. } => {}
                }
            }

            if tool_calls.is_empty() {
                let response = (!final_text.is_empty()).then(|| final_text.join("\n"));
                if !is_main_session {
                    runtime
                        .record(
                            AgentEvent::control(ControlEvent::TaskCompleted {
                                response: response.clone(),
                            }),
                            Some(completed.event_id),
                        )
                        .await?;
                }
                return Ok(AgentRunOutcome::Completed { response });
            }

            let available_evidence = context.iter().map(|item| item.event_id).collect();
            for tool_call in tool_calls {
                match self
                    .handle_tool_call(
                        runtime,
                        tool_call,
                        completed.event_id,
                        &available_evidence,
                        cancel.clone(),
                    )
                    .await?
                {
                    ToolHandlingOutcome::Continue(tool_context) => context.push(tool_context),
                    ToolHandlingOutcome::AwaitingAuthorization(approval_request_event_id) => {
                        return Ok(AgentRunOutcome::AwaitingAuthorization {
                            approval_request_event_id,
                        });
                    }
                }
            }
        }

        self.record_failed(runtime, "达到最大模型轮数").await?;
        Err(AgentLoopError::ModelTurnLimitExceeded)
    }

    fn model_tools(&self) -> Vec<crate::domain::ModelToolDefinition> {
        self.tools
            .list_definitions()
            .into_iter()
            .filter(|definition| definition.model_visible)
            .map(|definition| crate::domain::ModelToolDefinition {
                name: definition.name,
                description: definition.description,
                input_schema: definition.input_schema,
                strict: true,
            })
            .collect()
    }

    async fn prepare_task<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        trigger_event_id: Option<EventId>,
    ) -> Result<(), AgentLoopError>
    where
        S: EventStore,
    {
        runtime
            .record(
                AgentEvent::control(ControlEvent::TaskCreated { trigger_event_id }),
                trigger_event_id,
            )
            .await?;
        runtime
            .record(AgentEvent::control(ControlEvent::TaskQueued), None)
            .await?;
        Ok(())
    }

    async fn build_initial_context(
        &self,
        task_id: crate::domain::TaskId,
        request: &AgentRunRequest,
    ) -> Result<Vec<ModelContextItem>, AgentLoopError> {
        let mut context = request.context.clone();
        context.extend(InputInjector::default().inject_many(task_id, &request.input_events)?);
        if let (Some(memory), Some(query)) = (self.memory, request.memory_query.as_ref()) {
            let results = memory.search(query.clone()).await?;
            context.extend(MemoryContextBuilder::build(query, results));
        }
        Ok(context)
    }

    async fn call_model<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: &AgentRunRequest,
        context: &[ModelContextItem],
        cancel: CancellationToken,
    ) -> Result<CompletedModel, AgentLoopError>
    where
        S: EventStore,
    {
        let prompt = self
            .prompts
            .prompt_for(crate::ports::PromptTaskKind::for_task(
                runtime.projection().task_id,
            ))?;
        prompt.validate()?;
        let model_request = ModelRequest {
            task_id: runtime.projection().task_id,
            instructions_hash: fingerprint(&prompt.content)?,
            instructions: prompt.content,
            context: context.to_vec(),
            tools: self.model_tools(),
            output_contract: request.output_contract.clone(),
            options: request.model_options.clone(),
        };
        model_request.validate()?;

        let descriptor = self.model.descriptor();
        let call_started = runtime
            .record_with_provenance(
                AgentEvent::model(ModelEvent::CallStarted {
                    context_event_ids: context.iter().map(|item| item.event_id).collect(),
                    context_hash: fingerprint(&context)?,
                    provider: descriptor.provider,
                    model: descriptor.model,
                }),
                None,
                crate::domain::EventProvenance::model(None),
            )
            .await?;
        let stream = match self.model.start(model_request, cancel.clone()).await {
            Ok(stream) => stream,
            Err(error) => {
                runtime
                    .record_with_provenance(
                        AgentEvent::model(ModelEvent::Failed {
                            call_started_event_id: call_started.id,
                            error: error.to_string(),
                        }),
                        Some(call_started.id),
                        crate::domain::EventProvenance::model(None),
                    )
                    .await?;
                self.record_failed(runtime, error.to_string()).await?;
                return Err(AgentLoopError::Model(error));
            }
        };
        let turn = self
            .record_model_stream(runtime, stream, call_started.id, cancel)
            .await?;
        let completed = runtime
            .record_with_provenance(
                AgentEvent::model(ModelEvent::Completed {
                    call_started_event_id: call_started.id,
                    outputs: turn.outputs.clone(),
                    usage: turn.usage,
                }),
                Some(call_started.id),
                crate::domain::EventProvenance::model(None),
            )
            .await?;
        Ok(CompletedModel {
            event_id: completed.id,
            outputs: turn.outputs,
        })
    }

    async fn record_model_stream<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        mut stream: crate::ports::ModelEventStream,
        call_started_event_id: EventId,
        cancel: CancellationToken,
    ) -> Result<crate::domain::ModelTurn, AgentLoopError>
    where
        S: EventStore,
    {
        while let Some(item) = stream.next().await {
            if cancel.is_cancelled() {
                self.record_cancelled(runtime, "模型调用已取消").await?;
                return Err(AgentLoopError::Cancelled);
            }
            match item? {
                ModelStreamEvent::Delta {
                    sequence,
                    kind,
                    content,
                } => {
                    runtime
                        .record_with_provenance(
                            AgentEvent::model(ModelEvent::Delta {
                                call_started_event_id,
                                sequence,
                                kind,
                                content,
                            }),
                            Some(call_started_event_id),
                            crate::domain::EventProvenance::model(None),
                        )
                        .await?;
                }
                ModelStreamEvent::Completed(turn) => return Ok(turn),
            }
        }

        Err(AgentLoopError::ModelStreamEndedWithoutCompletion)
    }

    async fn handle_tool_call<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        tool_call: ToolCall,
        model_completed_event_id: EventId,
        available_evidence: &HashSet<EventId>,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        let proposed = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Proposed {
                    tool_call: tool_call.clone(),
                }),
                Some(model_completed_event_id),
                crate::domain::EventProvenance::model(tool_call.authority_parent_event_id),
            )
            .await?;

        let Some(definition) = self.tools.get_definition(&tool_call.name) else {
            return self
                .deny_tool(runtime, proposed.id, "模型请求了未注册工具")
                .await;
        };
        if !tool_call.arguments.is_object()
            || tool_call
                .authority_parent_event_id
                .is_some_and(|event_id| !available_evidence.contains(&event_id))
        {
            return self
                .deny_tool(runtime, proposed.id, "工具参数或授权证据不在当前上下文中")
                .await;
        }

        let validated = runtime
            .record(
                AgentEvent::tool(ToolEvent::Validated {
                    proposal_event_id: proposed.id,
                }),
                Some(proposed.id),
            )
            .await?;

        let evidence = self
            .resolve_evidence(runtime.projection().task_id, proposed.id)
            .await;
        let check = PermissionChecker::check(&definition, &evidence);
        match check {
            PermissionCheckResult::Allowed {
                effective_permission,
                evidence_event_ids,
            } => {
                self.execute_tool(
                    runtime,
                    ExecutionFlow {
                        proposal_event_id: proposed.id,
                        causation_id: validated.id,
                        tool_call,
                        effective_permission,
                        evidence_event_ids,
                    },
                    cancel,
                )
                .await
            }
            PermissionCheckResult::Insufficient {
                effective_permission,
                required_permission,
            } => {
                let checked = runtime
                    .record(
                        AgentEvent::tool(ToolEvent::AuthorizationChecked {
                            proposal_event_id: proposed.id,
                            decision: PolicyDecision::RequireApproval,
                            effective_permission,
                            evidence_event_ids: Vec::new(),
                        }),
                        Some(validated.id),
                    )
                    .await?;
                let approval_requested = runtime
                    .record(
                        AgentEvent::tool(ToolEvent::ApprovalRequested {
                            proposal_event_id: proposed.id,
                        }),
                        Some(checked.id),
                    )
                    .await?;

                self.request_authorization(
                    runtime,
                    AuthorizationFlow {
                        proposal_event_id: proposed.id,
                        approval_request_event_id: approval_requested.id,
                        tool_call,
                        required_permission,
                        original_evidence: evidence,
                    },
                    cancel,
                )
                .await
            }
        }
    }

    async fn resolve_evidence(
        &self,
        task_id: crate::domain::TaskId,
        proposal_event_id: EventId,
    ) -> Vec<crate::domain::AuthorizationEvidence> {
        let mut evidence = Vec::new();
        if let Some(item) = self
            .resolve_authority_chain(task_id, proposal_event_id)
            .await
        {
            evidence.push(item);
        }
        evidence
    }

    async fn resolve_authority_chain(
        &self,
        task_id: crate::domain::TaskId,
        selected_event_id: EventId,
    ) -> Option<crate::domain::AuthorizationEvidence> {
        const MAX_AUTHORITY_DEPTH: usize = 16;

        let mut current_event_id = selected_event_id;
        let mut visited = HashSet::new();
        for _ in 0..MAX_AUTHORITY_DEPTH {
            if !visited.insert(current_event_id) {
                return None;
            }
            let mut event = self
                .evidence_resolver
                .resolve(task_id, current_event_id)
                .await
                .ok()?;
            // 从工具提议事件自身开始逐层审查。控制事件只允许自身接受直接来源权限，不能
            // 被模型、工具或其他事件作为权限父节点借用。
            if !event.event_kind.can_be_authority_parent() {
                return None;
            }
            if event.status != crate::domain::AuthorizationEvidenceStatus::Active
                || event.is_expired()
            {
                return None;
            }
            if let Some(parent_event_id) = event.authority_parent_event_id {
                current_event_id = parent_event_id;
                continue;
            }
            if matches!(
                event.source,
                crate::domain::EventSource::Model | crate::domain::EventSource::Tool
            ) {
                event.permission = PermissionLevel::None;
            }
            event.event_id = selected_event_id;
            return Some(event);
        }
        None
    }

    async fn request_authorization<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        flow: AuthorizationFlow,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        let task_id = runtime.projection().task_id;
        let mut pending = false;
        for source in unique_sources(&flow.original_evidence) {
            let Some(provider) = self.authorization_providers.get(&source) else {
                continue;
            };
            let request = AuthorizationRequest {
                task_id,
                approval_request_event_id: flow.approval_request_event_id,
                tool_proposal_event_id: flow.proposal_event_id,
                tool_name: flow.tool_call.name.clone(),
                arguments_hash: fingerprint(&flow.tool_call.arguments)?,
                required_permission: flow.required_permission,
                original_evidence_event_ids: flow
                    .tool_call
                    .authority_parent_event_id
                    .into_iter()
                    .collect(),
            };

            match provider.request_authorization(request).await {
                Ok(AuthorizationRequestResult::Pending) => pending = true,
                Ok(AuthorizationRequestResult::Denied { .. }) | Err(_) => {}
                Ok(AuthorizationRequestResult::Authorized {
                    authorization_event_id,
                }) => {
                    let Ok(authorization) = self
                        .evidence_resolver
                        .resolve(task_id, authorization_event_id)
                        .await
                    else {
                        continue;
                    };
                    if authorization.approval_request_event_id
                        != Some(flow.approval_request_event_id)
                    {
                        continue;
                    }

                    let mut all_evidence = flow.original_evidence.clone();
                    all_evidence.push(authorization);
                    let definition = self
                        .tools
                        .get_definition(&flow.tool_call.name)
                        .expect("工具定义已在同一调用中查询成功");
                    if let PermissionCheckResult::Allowed {
                        effective_permission,
                        evidence_event_ids,
                    } = PermissionChecker::check(&definition, &all_evidence)
                    {
                        return self
                            .execute_tool(
                                runtime,
                                ExecutionFlow {
                                    proposal_event_id: flow.proposal_event_id,
                                    causation_id: flow.approval_request_event_id,
                                    tool_call: flow.tool_call,
                                    effective_permission,
                                    evidence_event_ids,
                                },
                                cancel,
                            )
                            .await;
                    }
                }
            }
        }

        if pending {
            return Ok(ToolHandlingOutcome::AwaitingAuthorization(
                flow.approval_request_event_id,
            ));
        }
        self.deny_tool(runtime, flow.proposal_event_id, "来源方未提供足够的授权")
            .await
    }

    async fn execute_tool<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        flow: ExecutionFlow,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        let checked = runtime
            .record(
                AgentEvent::tool(ToolEvent::AuthorizationChecked {
                    proposal_event_id: flow.proposal_event_id,
                    decision: PolicyDecision::Allow,
                    effective_permission: flow.effective_permission,
                    evidence_event_ids: flow.evidence_event_ids.clone(),
                }),
                Some(flow.causation_id),
            )
            .await?;
        let started = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Started {
                    proposal_event_id: flow.proposal_event_id,
                }),
                Some(checked.id),
                crate::domain::EventProvenance::tool(),
            )
            .await?;

        let invocation = AuthorizedToolInvocation {
            task_id: runtime.projection().task_id,
            proposal_event_id: flow.proposal_event_id,
            execution_started_event_id: started.id,
            tool_call: flow.tool_call,
            authorization_evidence_event_ids: flow.evidence_event_ids,
        };
        match self.tools.invoke(invocation, cancel).await {
            Ok(result) => {
                let finished = runtime
                    .record_with_provenance(
                        AgentEvent::tool(ToolEvent::Finished {
                            execution_started_event_id: started.id,
                            result: result.clone(),
                        }),
                        Some(started.id),
                        crate::domain::EventProvenance::tool(),
                    )
                    .await?;
                Ok(ToolHandlingOutcome::Continue(tool_result_context(
                    finished.id,
                    result,
                )))
            }
            Err(error) => {
                let failed = runtime
                    .record_with_provenance(
                        AgentEvent::tool(ToolEvent::Failed {
                            execution_started_event_id: started.id,
                            error: error.to_string(),
                        }),
                        Some(started.id),
                        crate::domain::EventProvenance::tool(),
                    )
                    .await?;
                Ok(ToolHandlingOutcome::Continue(ModelContextItem {
                    event_id: failed.id,
                    role: ModelInputRole::Tool,
                    content: format!("工具执行失败：{error}"),
                    permission: PermissionLevel::None,
                }))
            }
        }
    }

    async fn deny_tool<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        reason: &str,
    ) -> Result<ToolHandlingOutcome, AgentLoopError>
    where
        S: EventStore,
    {
        let denied = runtime
            .record(
                AgentEvent::tool(ToolEvent::AuthorizationChecked {
                    proposal_event_id,
                    decision: PolicyDecision::Deny,
                    effective_permission: PermissionLevel::None,
                    evidence_event_ids: Vec::new(),
                }),
                Some(proposal_event_id),
            )
            .await?;
        Ok(ToolHandlingOutcome::Continue(ModelContextItem {
            event_id: denied.id,
            role: ModelInputRole::Tool,
            content: format!("工具调用被拒绝：{reason}"),
            permission: PermissionLevel::None,
        }))
    }

    async fn record_cancelled<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        reason: &str,
    ) -> Result<(), AgentLoopError>
    where
        S: EventStore,
    {
        if !runtime.projection().status.is_terminal() {
            runtime
                .record(
                    AgentEvent::control(ControlEvent::TaskCancelled {
                        reason: reason.into(),
                    }),
                    None,
                )
                .await?;
        }
        Ok(())
    }

    async fn record_failed<S>(
        &self,
        runtime: &mut TaskRuntime<S>,
        reason: impl Into<String>,
    ) -> Result<(), AgentLoopError>
    where
        S: EventStore,
    {
        if !runtime.projection().status.is_terminal() {
            runtime
                .record(
                    AgentEvent::control(ControlEvent::TaskFailed {
                        reason: reason.into(),
                    }),
                    None,
                )
                .await?;
        }
        Ok(())
    }
}

enum ToolHandlingOutcome {
    Continue(ModelContextItem),
    AwaitingAuthorization(EventId),
}

struct CompletedModel {
    event_id: EventId,
    outputs: Vec<ModelOutput>,
}

struct AuthorizationFlow {
    proposal_event_id: EventId,
    approval_request_event_id: EventId,
    tool_call: ToolCall,
    required_permission: PermissionLevel,
    original_evidence: Vec<crate::domain::AuthorizationEvidence>,
}

struct ExecutionFlow {
    proposal_event_id: EventId,
    causation_id: EventId,
    tool_call: ToolCall,
    effective_permission: PermissionLevel,
    evidence_event_ids: Vec<EventId>,
}

fn unique_sources(evidence: &[crate::domain::AuthorizationEvidence]) -> Vec<String> {
    let mut sources = HashSet::new();
    evidence
        .iter()
        .map(|item| item.source.as_str())
        .filter(|source| sources.insert((*source).to_owned()))
        .map(str::to_owned)
        .collect()
}

fn tool_result_context(event_id: EventId, result: ToolResult) -> ModelContextItem {
    ModelContextItem {
        event_id,
        role: ModelInputRole::Tool,
        content: result.summary,
        permission: PermissionLevel::None,
    }
}

fn fingerprint(value: &impl serde::Serialize) -> Result<String, AgentLoopError> {
    let bytes = serde_json::to_vec(value)?;
    // 该指纹仅用于请求展示与关联；核心始终以工具提议事件中保存的真实参数复核绑定关系。
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("fnv1a64:{hash:016x}"))
}

#[derive(Debug, Error)]
pub enum AgentLoopError {
    #[error("该接口仅允许主会话任务调用")]
    MainTaskRequired,
    #[error(transparent)]
    InvalidRequest(#[from] AgentRunRequestError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    ModelRequest(#[from] crate::domain::ModelRequestValidationError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Prompt(#[from] crate::ports::PromptError),
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    InputInjection(#[from] InputInjectionError),
    #[error("任务必须处于 New 状态，当前为 {0:?}")]
    TaskNotNew(TaskStatus),
    #[error("模型流在完成前结束")]
    ModelStreamEndedWithoutCompletion,
    #[error("模型调用被取消")]
    Cancelled,
    #[error("达到最大模型轮数")]
    ModelTurnLimitExceeded,
    #[error(transparent)]
    Model(#[from] ModelError),
}
