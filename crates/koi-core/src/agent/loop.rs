use std::collections::HashSet;

use futures_util::StreamExt;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    ContextAssembler, ContextBudget, InputInjectionError, InputInjector, RuntimeError,
    RuntimeRecoveryError, TASK_CONTROL_TOOL, TASK_DELETE_TOOL, TASK_INPUT_TOOL, TASK_INSPECT_TOOL,
    TASK_LIST_TOOL, TASK_NAME_TOOL, TASK_START_TOOL, TaskInputArguments, TaskInspectArguments,
    TaskListArguments, TaskManager, TaskManagerError, TaskRuntime, TaskStartArguments,
    TaskToolArgumentsError, parse_task_control, parse_task_delete, parse_task_input,
    parse_task_inspect, parse_task_list, parse_task_name, parse_task_start,
};
use crate::domain::{
    AgentEvent, AuthorizationRequest, AuthorizationRequestResult, AuthorizedToolInvocation,
    ControlEvent, EventId, MemoryContextBuilder, MemoryQuery, ModelContextItem, ModelError,
    ModelEvent, ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract,
    ModelRequest, ModelStreamEvent, PermissionCheckResult, PermissionChecker, PermissionLevel,
    PolicyDecision, TaskId, TaskStatus, ToolCall, ToolDefinition, ToolEvent, ToolResult,
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
    Completed {
        response: Option<String>,
    },
    AwaitingAuthorization {
        approval_request_event_id: EventId,
    },
    /// 主会话通过 `task.start` 启动了一个子任务；结果将在子任务结束后异步回传。
    StartedChildTask {
        task_id: TaskId,
    },
    Cancelled,
}

/// `task.*` 工具解析后的调度动作。
enum TaskToolAction {
    Start(TaskStartArguments),
    Input(TaskInputArguments),
    Control {
        task_id: TaskId,
        control: ControlEvent,
    },
    Name {
        task_id: TaskId,
        name: String,
    },
    Delete {
        task_id: TaskId,
        reason: String,
    },
    List(TaskListArguments),
    Inspect(TaskInspectArguments),
}

/// 按工具名解析任务管理工具参数；未知工具名视为参数错误。
fn parse_task_tool_action(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<TaskToolAction, TaskToolArgumentsError> {
    match name {
        TASK_START_TOOL => parse_task_start(arguments).map(TaskToolAction::Start),
        TASK_INPUT_TOOL => parse_task_input(arguments).map(TaskToolAction::Input),
        TASK_CONTROL_TOOL => parse_task_control(arguments).map(|args| TaskToolAction::Control {
            task_id: args.task_id,
            control: args.control,
        }),
        TASK_NAME_TOOL => parse_task_name(arguments).map(|args| TaskToolAction::Name {
            task_id: args.task_id,
            name: args.name,
        }),
        TASK_DELETE_TOOL => parse_task_delete(arguments).map(|args| TaskToolAction::Delete {
            task_id: args.task_id,
            reason: args.reason,
        }),
        TASK_LIST_TOOL => parse_task_list(arguments).map(TaskToolAction::List),
        TASK_INSPECT_TOOL => parse_task_inspect(arguments).map(TaskToolAction::Inspect),
        other => Err(TaskToolArgumentsError(format!("未知任务管理工具：{other}"))),
    }
}

/// 将模型、工具、权限与记忆端口编排为单任务主循环。
pub struct AgentLoop<'a, S: EventStore> {
    model: &'a dyn ModelProvider,
    tools: &'a ToolRegistry,
    evidence_resolver: &'a dyn AuthorizationEvidenceResolver,
    authorization_providers: &'a SourceAuthorizationRegistry,
    memory: Option<&'a dyn MemoryStore>,
    prompts: &'a dyn crate::ports::SystemPromptProvider,
    task_manager: Option<&'a TaskManager<S>>,
}

impl<'a, S: EventStore> AgentLoop<'a, S> {
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
            task_manager: None,
        }
    }

    /// 绑定任务管理器，使主会话可以使用 `task.*` 管理工具。
    #[must_use]
    pub const fn with_task_manager(mut self, task_manager: &'a TaskManager<S>) -> Self {
        self.task_manager = Some(task_manager);
        self
    }

    /// 运行一个从 `TaskStatus::New` 开始的新任务。
    ///
    /// 模型产生的工具调用始终先记录，再验证其证据和权限。权限不足时，核心记录授权
    /// 请求并按来源路由提权；来源返回的新输入事件仍会被重新审查。
    ///
    /// # Errors
    ///
    /// 当任务状态、请求、事件持久化、模型、记忆或来源授权基础设施发生错误时返回错误。
    pub async fn run(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, AgentLoopError> {
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
    pub async fn run_main(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, AgentLoopError> {
        if !runtime.projection().task_id.is_main() {
            return Err(AgentLoopError::MainTaskRequired);
        }
        self.run_internal(runtime, request, cancel, true).await
    }

    /// 在来源方已经写入审批结果后恢复一个等待授权的任务。
    ///
    /// 恢复时只接受任务事件流中已经存在的 `ApprovalSubmitted` 事件，并从该事件反查
    /// `ApprovalRequested -> Tool::Proposed`。调用方不能直接提交工具参数、权限或提议事件
    /// 来替换原始调用；恢复后的工具调用仍会经过完整的权限检查。
    ///
    /// # Errors
    ///
    /// 当任务状态、审批绑定、事件读取或后续 Agent 执行失败时返回错误。
    pub async fn resume_after_approval(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        approval_submission_event_id: EventId,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, AgentLoopError> {
        request.validate()?;
        if runtime.projection().status != TaskStatus::WaitingApproval {
            return Err(AgentLoopError::TaskNotWaitingApproval(
                runtime.projection().status,
            ));
        }

        let task_id = runtime.projection().task_id;
        let events = runtime.load_events().await?;
        let binding = approval_binding(&events, approval_submission_event_id)?;
        let is_main_session = task_id.is_main();

        let mut context = self.build_initial_context(runtime, &request).await?;
        let available_evidence = context
            .iter()
            .map(|item| item.event_id)
            .collect::<HashSet<_>>();
        if binding
            .tool_call
            .authority_parent_event_id
            .is_some_and(|event_id| !available_evidence.contains(&event_id))
        {
            return Err(AgentLoopError::InvalidApproval(
                "原始工具提议引用了当前上下文之外的授权证据".into(),
            ));
        }

        let Some(definition) = self.tools.get_definition(&binding.tool_call.name) else {
            return Err(AgentLoopError::InvalidApproval(
                "原始工具已经不再注册".into(),
            ));
        };
        if !binding.tool_call.arguments.is_object() {
            return Err(AgentLoopError::InvalidApproval(
                "原始工具参数不是 JSON 对象".into(),
            ));
        }

        let original_evidence = self
            .resolve_evidence(task_id, binding.proposal_event_id)
            .await;
        let tool_outcome = if binding.approved {
            let authorization = self
                .resolve_authority_chain(task_id, binding.approval_submission_event_id)
                .await
                .filter(|evidence| {
                    evidence.approval_request_event_id == Some(binding.approval_request_event_id)
                });
            let mut all_evidence = original_evidence;
            if let Some(authorization) = authorization {
                all_evidence.push(authorization);
            }
            match PermissionChecker::check(&definition, &all_evidence) {
                PermissionCheckResult::Allowed {
                    effective_permission,
                    evidence_event_ids,
                } => {
                    self.execute_tool(
                        runtime,
                        ExecutionFlow {
                            proposal_event_id: binding.proposal_event_id,
                            causation_id: binding.approval_submission_event_id,
                            tool_call: binding.tool_call,
                            effective_permission,
                            evidence_event_ids,
                        },
                        cancel.clone(),
                    )
                    .await?
                }
                PermissionCheckResult::Insufficient { .. } => {
                    self.deny_tool(runtime, binding.proposal_event_id, "审批事件未提供足够权限")
                        .await?
                }
            }
        } else {
            self.deny_tool(runtime, binding.proposal_event_id, "来源方拒绝了工具调用")
                .await?
        };

        match tool_outcome {
            ToolHandlingOutcome::Continue(tool_context) => context.push(tool_context),
            ToolHandlingOutcome::AwaitingAuthorization(_) => {
                return Err(AgentLoopError::InvalidApproval(
                    "审批恢复不应再次进入等待授权状态".into(),
                ));
            }
        }
        self.run_turns(runtime, request, cancel, is_main_session, context)
            .await
    }

    async fn run_internal(
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
        } else if status.is_terminal() || (!is_main_session && status != TaskStatus::Queued) {
            return Err(AgentLoopError::TaskNotNew(runtime.projection().status));
        }
        let context = self.build_initial_context(runtime, &request).await?;

        self.run_turns(runtime, request, cancel, is_main_session, context)
            .await
    }

    async fn run_turns(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: AgentRunRequest,
        cancel: CancellationToken,
        is_main_session: bool,
        mut context: Vec<ModelContextItem>,
    ) -> Result<AgentRunOutcome, AgentLoopError> {
        let mut final_text = Vec::new();
        for _ in 0..request.max_model_turns {
            if cancel.is_cancelled() {
                self.record_cancelled(runtime, "任务已取消").await?;
                return Ok(AgentRunOutcome::Cancelled);
            }

            context = self
                .fit_context_to_budget(runtime, &request, context)
                .await?;
            let completed = match self
                .call_model(runtime, &request, &context, cancel.clone())
                .await
            {
                Ok(completed) => completed,
                Err(AgentLoopError::Cancelled) => return Ok(AgentRunOutcome::Cancelled),
                Err(error) => return Err(error),
            };

            let outputs = completed.outputs;
            let mut tool_calls = Vec::new();
            for output in &outputs {
                match output {
                    ModelOutput::Text { text } => final_text.push(text.clone()),
                    ModelOutput::Refusal { reason } => final_text.push(reason.clone()),
                    ModelOutput::ToolCall(tool_call) => tool_calls.push(tool_call.clone()),
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

            // 下一轮模型调用需要看到本轮助手文本。工具调用本身由 Provider 的当前调用
            // 状态编码，`ContextAssembler::model_output_item` 会有意忽略纯工具调用输出，
            // 避免在 Chat Completions 请求中重复构造 assistant tool call。
            if let Some(item) = ContextAssembler::model_output_item(completed.event_id, &outputs) {
                push_unique_context(&mut context, item);
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

    fn model_tools(&self, is_main_session: bool) -> Vec<crate::domain::ModelToolDefinition> {
        self.tools
            .list_definitions()
            .into_iter()
            .filter(|definition| {
                definition.model_visible && (!definition.main_session_only || is_main_session)
            })
            .map(|definition| crate::domain::ModelToolDefinition {
                name: definition.name,
                description: definition.description,
                input_schema: definition.input_schema,
                strict: true,
            })
            .collect()
    }

    async fn prepare_task(
        &self,
        runtime: &mut TaskRuntime<S>,
        trigger_event_id: Option<EventId>,
    ) -> Result<(), AgentLoopError> {
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

    /// 构建模型上下文，并在注入前完成输入事件的权限检查。
    ///
    /// 检查分两层：
    /// 1. 持久化完整性——输入事件必须已存在于事件存储且与持久化内容一致，否则其
    ///    事件 ID 可能被模型当作伪造的授权证据引用（权限提升通道）；
    /// 2. 会话最低控制权限——指令性输入（用户消息与告警）的权限结论必须满足任务
    ///    投影的最低控制权限。不满足的输入被拒绝注入但不中断任务，事件保留在流中
    ///    供审计；其余输入照常注入。内部回声（系统事件、核心回传工具结果）豁免。
    async fn build_initial_context(
        &self,
        runtime: &TaskRuntime<S>,
        request: &AgentRunRequest,
    ) -> Result<Vec<ModelContextItem>, AgentLoopError>
    where
        S: EventStore,
    {
        let task_id = runtime.projection().task_id;
        let stored = runtime.load_events().await?;
        if !request.input_events.is_empty() {
            InputInjector::verify_persisted_events(&request.input_events, &stored)?;
        }

        // 事件存储是完整历史的事实来源。调用方传入的 context 可能是刚刚回传的工具
        // 结果或其他尚未作为普通输入事件投影的上下文，因此只用它们的 ID 去重，随后
        // 仍然把任务事件流中所有可见事件按顺序恢复出来。
        let provided_context_event_ids = request
            .context
            .iter()
            .map(|item| item.event_id)
            .collect::<HashSet<_>>();
        let mut context = ContextAssembler::from_events(
            task_id,
            &stored,
            &provided_context_event_ids,
            runtime.projection().minimum_control_permission,
        )?;
        for item in &request.context {
            push_unique_context(&mut context, item.clone());
        }
        if let (Some(memory), Some(query)) = (self.memory, request.memory_query.as_ref()) {
            let results = memory.search(query.clone()).await?;
            context.extend(MemoryContextBuilder::build(query, results));
        }
        Ok(context)
    }

    /// 在每次模型调用前按模型上下文窗口压缩最早的完整历史。
    ///
    /// 压缩只改变本次模型请求的投影，不删除任何原始事件。每次压缩都会追加一个无权限
    /// 的 `ContextCompacted` 控制事件；下次恢复任务时，`ContextAssembler` 会使用该事件
    /// 的摘要而不会再次展开已经覆盖的历史。
    async fn fit_context_to_budget(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: &AgentRunRequest,
        mut context: Vec<ModelContextItem>,
    ) -> Result<Vec<ModelContextItem>, AgentLoopError> {
        let prompt = self
            .prompts
            .prompt_for(crate::ports::PromptTaskKind::for_task(
                runtime.projection().task_id,
            ))?;
        prompt.validate()?;
        let tools = self.model_tools(runtime.projection().task_id.is_main());
        let fixed_tokens = ContextAssembler::estimate_fixed_tokens(&prompt.content, &tools);
        let budget = ContextBudget::new(
            self.model.context_window_tokens(),
            request.model_options.max_output_tokens,
        );
        let input_budget = budget.available_input_tokens(fixed_tokens);
        if input_budget == 0 {
            return Err(AgentLoopError::ContextBudgetTooSmall {
                context_window_tokens: budget.context_window_tokens,
                fixed_tokens,
            });
        }

        // 一次调用最多连续写入少量压缩检查点。正常情况下第一次压缩即可完成；限制循环
        // 是为了避免异常长的不可压缩工具结果导致无界追加控制事件。
        for _ in 0..8 {
            let Some(plan) = ContextAssembler::compact(&context, input_budget)? else {
                return Ok(context);
            };
            let crate::agent::ContextCompactionPlan {
                summary_position,
                remaining,
                dropped_context_event_ids: plan_dropped_context_event_ids,
                summary,
            } = plan;
            let mut dropped_context_event_ids =
                ContextAssembler::latest_compaction_coverage(&runtime.load_events().await?)
                    .into_iter()
                    .collect::<Vec<_>>();
            dropped_context_event_ids.extend(plan_dropped_context_event_ids);
            dropped_context_event_ids.sort_by_key(ToString::to_string);
            dropped_context_event_ids.dedup();
            let compacted = runtime
                .record(
                    AgentEvent::control(ControlEvent::ContextCompacted {
                        dropped_context_event_ids,
                        summary: summary.clone(),
                    }),
                    None,
                )
                .await?;
            context = remaining;
            context.insert(
                summary_position.min(context.len()),
                ContextAssembler::summary_item(compacted.id, &summary),
            );
        }

        Err(AgentLoopError::ContextCompactionDidNotConverge)
    }

    async fn call_model(
        &self,
        runtime: &mut TaskRuntime<S>,
        request: &AgentRunRequest,
        context: &[ModelContextItem],
        cancel: CancellationToken,
    ) -> Result<CompletedModel, AgentLoopError> {
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
            tools: self.model_tools(runtime.projection().task_id.is_main()),
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
                    model_id: descriptor.model_id,
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
        let turn = self.record_model_stream(runtime, stream, cancel).await?;
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

    async fn record_model_stream(
        &self,
        runtime: &mut TaskRuntime<S>,
        mut stream: crate::ports::ModelEventStream,
        cancel: CancellationToken,
    ) -> Result<crate::domain::ModelTurn, AgentLoopError> {
        while let Some(item) = stream.next().await {
            if cancel.is_cancelled() {
                self.record_cancelled(runtime, "模型调用已取消").await?;
                return Err(AgentLoopError::Cancelled);
            }
            match item? {
                // Delta 仅是供应商传输层的临时片段。当前版本不将其写入事件存储，
                // 避免历史会话被 token 级事件淹没；`Completed` 会持久化本次调用的
                // 完整输出，作为重放和展示的权威记录。
                ModelStreamEvent::Delta { .. } => {}
                ModelStreamEvent::Completed(turn) => return Ok(turn),
            }
        }

        Err(AgentLoopError::ModelStreamEndedWithoutCompletion)
    }

    async fn handle_tool_call(
        &self,
        runtime: &mut TaskRuntime<S>,
        tool_call: ToolCall,
        model_completed_event_id: EventId,
        available_evidence: &HashSet<EventId>,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
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
                if definition.main_session_only {
                    return self
                        .execute_task_tool(
                            runtime,
                            &definition,
                            proposed.id,
                            validated.id,
                            tool_call,
                        )
                        .await;
                }
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
                self.begin_approval_flow(
                    runtime,
                    proposed.id,
                    validated.id,
                    tool_call,
                    evidence,
                    effective_permission,
                    required_permission,
                    cancel,
                )
                .await
            }
        }
    }

    /// 记录 `AuthorizationChecked(RequireApproval)` 与 `ApprovalRequested`，
    /// 然后向来源适配器发起提权请求。
    #[allow(clippy::too_many_arguments)]
    async fn begin_approval_flow(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        validated_event_id: EventId,
        tool_call: ToolCall,
        evidence: Vec<crate::domain::AuthorizationEvidence>,
        effective_permission: PermissionLevel,
        required_permission: PermissionLevel,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        let checked = runtime
            .record(
                AgentEvent::tool(ToolEvent::AuthorizationChecked {
                    proposal_event_id,
                    decision: PolicyDecision::RequireApproval,
                    effective_permission,
                    evidence_event_ids: Vec::new(),
                }),
                Some(validated_event_id),
            )
            .await?;
        let approval_requested = runtime
            .record(
                AgentEvent::tool(ToolEvent::ApprovalRequested { proposal_event_id }),
                Some(checked.id),
            )
            .await?;

        self.request_authorization(
            runtime,
            AuthorizationFlow {
                proposal_event_id,
                approval_request_event_id: approval_requested.id,
                tool_call,
                required_permission,
                original_evidence: evidence,
            },
            cancel,
        )
        .await
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
            let mut event = match self
                .evidence_resolver
                .resolve(task_id, current_event_id)
                .await
            {
                Ok(event) => event,
                Err(_) => self
                    .evidence_resolver
                    .resolve_any(current_event_id)
                    .await
                    .ok()?,
            };
            // 从工具提议事件自身开始逐层审查。控制事件与 System 来源的核心内部事件
            // 只承载核心自身运转所需的权限，不能被模型、工具或其他事件作为权限父节点
            // 借用；工具事件回传可以无条件进入会话，但其权限为 None，同样不能提权。
            if !event.can_be_authority_parent() {
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

    async fn request_authorization(
        &self,
        runtime: &mut TaskRuntime<S>,
        flow: AuthorizationFlow,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
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

    async fn execute_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        flow: ExecutionFlow,
        cancel: CancellationToken,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
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
                    &result,
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

    /// 执行主会话专用的任务管理工具。
    ///
    /// 所有操作经 `TaskManager` 在主会话事件流中留下 `TaskOperationRequested` 与
    /// `Accepted/Rejected` 审计事件。`task.start` 在成功后写入绑定 Accepted 事件的
    /// `ToolEvent::Started`，立即返回子任务 ID，最终结果仍由核心在子任务结束后以
    /// `ToolEvent::Finished` 回传；其余操作同步完成并立即返回工具结果。
    #[allow(clippy::too_many_lines)]
    async fn execute_task_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        definition: &ToolDefinition,
        proposal_event_id: EventId,
        validated_event_id: EventId,
        tool_call: ToolCall,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        let Some(manager) = self.task_manager else {
            return self
                .deny_tool(runtime, proposal_event_id, "任务管理器未配置")
                .await;
        };
        if !runtime.projection().task_id.is_main() {
            return self
                .deny_tool(runtime, proposal_event_id, "任务管理工具仅主会话可用")
                .await;
        }

        let checked = runtime
            .record(
                AgentEvent::tool(ToolEvent::AuthorizationChecked {
                    proposal_event_id,
                    decision: PolicyDecision::Allow,
                    effective_permission: PermissionLevel::None,
                    evidence_event_ids: Vec::new(),
                }),
                Some(validated_event_id),
            )
            .await?;

        let action = match parse_task_tool_action(&definition.name, &tool_call.arguments) {
            Ok(action) => action,
            Err(error) => {
                return self
                    .deny_tool(runtime, proposal_event_id, &error.to_string())
                    .await;
            }
        };

        match action {
            TaskToolAction::Start(_args) => {
                let mut created = match manager
                    .request_create_child(runtime, Some(validated_event_id))
                    .await
                {
                    Ok(created) => created,
                    Err(error) => {
                        return self
                            .fail_task_tool(
                                runtime,
                                proposal_event_id,
                                checked.id,
                                &error.to_string(),
                            )
                            .await;
                    }
                };
                let started = runtime
                    .record_with_provenance(
                        AgentEvent::tool(ToolEvent::Started { proposal_event_id }),
                        Some(created.accepted_event_id),
                        crate::domain::EventProvenance::tool(),
                    )
                    .await?;
                self.bootstrap_child_task(&mut created).await?;
                self.acknowledge_child_started(runtime, started.id, created.task_id)
                    .await
            }
            TaskToolAction::Input(args) => {
                let Some(authority_parent_event_id) = tool_call.authority_parent_event_id else {
                    return self
                        .deny_tool(
                            runtime,
                            proposal_event_id,
                            "task.input 必须引用一个可见的授权父事件",
                        )
                        .await;
                };
                let Some(authority) = self
                    .resolve_authority_chain(
                        runtime.projection().task_id,
                        authority_parent_event_id,
                    )
                    .await
                else {
                    return self
                        .deny_tool(
                            runtime,
                            proposal_event_id,
                            "task.input 的授权父事件无效、已过期或不可作为授权来源",
                        )
                        .await;
                };
                if !authority.permission.can_authorize() {
                    return self
                        .deny_tool(
                            runtime,
                            proposal_event_id,
                            "task.input 的授权父事件不具备有效权限",
                        )
                        .await;
                }
                let outcome = manager
                    .request_input_child(
                        runtime,
                        args.task_id,
                        args.message,
                        authority_parent_event_id,
                        authority.permission,
                        Some(validated_event_id),
                    )
                    .await
                    .map(|event_id| {
                        (
                            format!("已向子任务 {} 投递输入事件 {}", args.task_id, event_id),
                            serde_json::json!({
                                "task_id": args.task_id.to_string(),
                                "input_event_id": event_id.to_string(),
                                "authority_parent_event_id": authority_parent_event_id.to_string(),
                            }),
                        )
                    });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
            TaskToolAction::Control { task_id, control } => {
                let outcome = manager
                    .request_control_child(runtime, task_id, control, Some(validated_event_id))
                    .await
                    .map(|()| {
                        (
                            format!("已向子任务 {task_id} 发送控制事件"),
                            serde_json::json!({ "task_id": task_id.to_string() }),
                        )
                    });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
            TaskToolAction::Name { task_id, name } => {
                let outcome = manager
                    .request_name_child(runtime, task_id, &name, Some(validated_event_id))
                    .await
                    .map(|_| {
                        (
                            format!("已将子任务 {task_id} 命名为「{name}」"),
                            serde_json::json!({ "task_id": task_id.to_string(), "name": name }),
                        )
                    });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
            TaskToolAction::Delete { task_id, reason } => {
                let outcome = manager
                    .request_delete_child(runtime, task_id, reason, Some(validated_event_id))
                    .await
                    .map(|()| {
                        (
                            format!("已删除子任务 {task_id}"),
                            serde_json::json!({ "task_id": task_id.to_string() }),
                        )
                    });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
            TaskToolAction::List(_args) => {
                let outcome = manager.list_child_tasks().await.map(|tasks| {
                    let data = serde_json::json!({
                        "tasks": tasks.iter().map(task_summary_json).collect::<Vec<_>>()
                    });
                    let summary = if tasks.is_empty() {
                        "当前没有已存在的子任务".to_owned()
                    } else {
                        format!("已列出 {} 个已存在的子任务", tasks.len())
                    };
                    (summary, data)
                });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
            TaskToolAction::Inspect(args) => {
                let outcome = manager
                    .inspect_child_task(args.task_id)
                    .await
                    .map(|details| {
                        (
                            format!("已读取子任务 {} 的详情", args.task_id),
                            task_details_json(&details),
                        )
                    });
                self.settle_sync_task_tool(runtime, proposal_event_id, checked.id, outcome)
                    .await
            }
        }
    }

    /// 同步任务管理操作的统一收尾：成功记录 `Finished`，失败记录 `Failed`。
    async fn settle_sync_task_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        checked_event_id: EventId,
        outcome: Result<(String, serde_json::Value), TaskManagerError>,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        match outcome {
            Ok((summary, data)) => {
                self.finish_task_tool(runtime, proposal_event_id, checked_event_id, summary, data)
                    .await
            }
            Err(error) => {
                self.fail_task_tool(
                    runtime,
                    proposal_event_id,
                    checked_event_id,
                    &error.to_string(),
                )
                .await
            }
        }
    }

    /// 为同步完成的任务管理工具记录 `Started` 与 `Finished` 并返回工具结果上下文。
    async fn finish_task_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        checked_event_id: EventId,
        summary: String,
        data: serde_json::Value,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        let started = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Started { proposal_event_id }),
                Some(checked_event_id),
                crate::domain::EventProvenance::tool(),
            )
            .await?;
        let result = ToolResult {
            summary: summary.clone(),
            data,
            truncated: false,
        };
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
            &result,
        )))
    }

    /// 为被拒绝的任务管理操作记录 `Started` 与 `Failed` 并返回失败上下文。
    async fn fail_task_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        checked_event_id: EventId,
        error: &str,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        let started = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Started { proposal_event_id }),
                Some(checked_event_id),
                crate::domain::EventProvenance::tool(),
            )
            .await?;
        let failed = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Failed {
                    execution_started_event_id: started.id,
                    error: error.to_owned(),
                }),
                Some(started.id),
                crate::domain::EventProvenance::tool(),
            )
            .await?;
        Ok(ToolHandlingOutcome::Continue(ModelContextItem {
            event_id: failed.id,
            role: ModelInputRole::Tool,
            content: format!("任务管理操作被拒绝：{error}"),
            permission: PermissionLevel::None,
        }))
    }

    /// 为新子任务写入生命周期事件。
    ///
    /// 具体任务要求不再通过启动命令写入；主会话必须在获得子任务 ID 后通过
    /// `task.input` 投递一条带授权父事件的输入。
    async fn bootstrap_child_task(
        &self,
        created: &mut crate::agent::CreatedChild<S>,
    ) -> Result<(), AgentLoopError> {
        let child = &mut created.runtime;
        child
            .record(
                AgentEvent::control(ControlEvent::TaskCreated {
                    trigger_event_id: Some(created.accepted_event_id),
                }),
                Some(created.accepted_event_id),
            )
            .await?;
        child
            .record(AgentEvent::control(ControlEvent::TaskQueued), None)
            .await?;
        Ok(())
    }

    /// 向主会话返回新子任务的 ID，但保留 `task.start` 的 Started 事件用于最终结果回传。
    async fn acknowledge_child_started(
        &self,
        runtime: &mut TaskRuntime<S>,
        started_event_id: EventId,
        task_id: TaskId,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
        let content = format!(
            "子任务已创建并处于 Queued 状态，task_id={task_id}。请使用 task.input 投递具体任务要求。"
        );
        let output = runtime
            .record_with_provenance(
                AgentEvent::tool(ToolEvent::Output {
                    execution_started_event_id: started_event_id,
                    sequence: 0,
                    content: content.clone(),
                }),
                Some(started_event_id),
                crate::domain::EventProvenance::tool(),
            )
            .await?;
        Ok(ToolHandlingOutcome::Continue(ModelContextItem {
            event_id: output.id,
            role: ModelInputRole::Tool,
            content,
            permission: PermissionLevel::None,
        }))
    }

    async fn deny_tool(
        &self,
        runtime: &mut TaskRuntime<S>,
        proposal_event_id: EventId,
        reason: &str,
    ) -> Result<ToolHandlingOutcome, AgentLoopError> {
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

    async fn record_cancelled(
        &self,
        runtime: &mut TaskRuntime<S>,
        reason: &str,
    ) -> Result<(), AgentLoopError> {
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

    async fn record_failed(
        &self,
        runtime: &mut TaskRuntime<S>,
        reason: impl Into<String>,
    ) -> Result<(), AgentLoopError> {
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

struct ApprovalBinding {
    approval_submission_event_id: EventId,
    approval_request_event_id: EventId,
    proposal_event_id: EventId,
    approved: bool,
    tool_call: ToolCall,
}

fn approval_binding(
    events: &[crate::domain::EventEnvelope],
    approval_submission_event_id: EventId,
) -> Result<ApprovalBinding, AgentLoopError> {
    let approval_request_event_id = approval_submission(events, approval_submission_event_id)?;
    let proposal_event_id = approval_proposal(events, approval_request_event_id)?;
    let tool_call = proposed_tool_call(events, proposal_event_id)?;
    Ok(ApprovalBinding {
        approval_submission_event_id,
        approval_request_event_id,
        proposal_event_id,
        approved: approval_submission_approved(events, approval_submission_event_id)?,
        tool_call,
    })
}

fn approval_submission(
    events: &[crate::domain::EventEnvelope],
    approval_submission_event_id: EventId,
) -> Result<EventId, AgentLoopError> {
    let event = events
        .iter()
        .find(|event| event.id == approval_submission_event_id)
        .ok_or(AgentLoopError::ApprovalEventNotFound(
            approval_submission_event_id,
        ))?;
    let AgentEvent::Ingress(ingress) = &event.payload else {
        return Err(AgentLoopError::InvalidApproval(
            "指定事件不是审批提交事件".into(),
        ));
    };
    match ingress.as_ref() {
        crate::domain::IngressEvent::ApprovalSubmitted {
            approval_request_event_id,
            ..
        } => {
            let latest_approval_request = events.iter().rev().find_map(|event| {
                let AgentEvent::Tool(tool) = &event.payload else {
                    return None;
                };
                match tool.as_ref() {
                    ToolEvent::ApprovalRequested { .. } => Some(event.id),
                    _ => None,
                }
            });
            if latest_approval_request != Some(*approval_request_event_id) {
                return Err(AgentLoopError::InvalidApproval(
                    "审批提交没有绑定当前等待中的授权请求".into(),
                ));
            }
            Ok(*approval_request_event_id)
        }
        _ => Err(AgentLoopError::InvalidApproval(
            "指定事件不是审批提交事件".into(),
        )),
    }
}

fn approval_submission_approved(
    events: &[crate::domain::EventEnvelope],
    approval_submission_event_id: EventId,
) -> Result<bool, AgentLoopError> {
    let event = events
        .iter()
        .find(|event| event.id == approval_submission_event_id)
        .ok_or(AgentLoopError::ApprovalEventNotFound(
            approval_submission_event_id,
        ))?;
    let AgentEvent::Ingress(ingress) = &event.payload else {
        return Err(AgentLoopError::InvalidApproval(
            "指定事件不是审批提交事件".into(),
        ));
    };
    match ingress.as_ref() {
        crate::domain::IngressEvent::ApprovalSubmitted { approved, .. } => Ok(*approved),
        _ => Err(AgentLoopError::InvalidApproval(
            "指定事件不是审批提交事件".into(),
        )),
    }
}

fn approval_proposal(
    events: &[crate::domain::EventEnvelope],
    approval_request_event_id: EventId,
) -> Result<EventId, AgentLoopError> {
    events
        .iter()
        .find_map(|event| {
            if event.id != approval_request_event_id {
                return None;
            }
            let AgentEvent::Tool(tool) = &event.payload else {
                return None;
            };
            match tool.as_ref() {
                ToolEvent::ApprovalRequested { proposal_event_id } => Some(*proposal_event_id),
                _ => None,
            }
        })
        .ok_or(AgentLoopError::InvalidApproval(
            "授权请求事件不存在或类型不正确".into(),
        ))
}

fn proposed_tool_call(
    events: &[crate::domain::EventEnvelope],
    proposal_event_id: EventId,
) -> Result<ToolCall, AgentLoopError> {
    events
        .iter()
        .find_map(|event| {
            if event.id != proposal_event_id {
                return None;
            }
            let AgentEvent::Tool(tool) = &event.payload else {
                return None;
            };
            match tool.as_ref() {
                ToolEvent::Proposed { tool_call } => Some(tool_call.clone()),
                _ => None,
            }
        })
        .ok_or(AgentLoopError::InvalidApproval(
            "授权请求对应的工具提议不存在".into(),
        ))
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

fn tool_result_context(event_id: EventId, result: &ToolResult) -> ModelContextItem {
    ModelContextItem {
        event_id,
        role: ModelInputRole::Tool,
        content: result.model_content(),
        permission: PermissionLevel::None,
    }
}

fn push_unique_context(context: &mut Vec<ModelContextItem>, item: ModelContextItem) {
    if !context
        .iter()
        .any(|existing| existing.event_id == item.event_id)
    {
        context.push(item);
    }
}

fn task_summary_json(summary: &crate::agent::TaskSummary) -> serde_json::Value {
    serde_json::json!({
        "task_id": summary.task_id.to_string(),
        "status": format!("{:?}", summary.status),
        "title": summary.title.clone(),
        "last_sequence": summary.last_sequence,
        "has_result": summary.has_result,
    })
}

fn task_details_json(details: &crate::agent::TaskDetails) -> serde_json::Value {
    let mut value = task_summary_json(&details.summary);
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "latest_result_event_id".into(),
            details
                .latest_result_event_id
                .map_or(serde_json::Value::Null, |event_id| {
                    serde_json::Value::String(event_id.to_string())
                }),
        );
        object.insert(
            "latest_result".into(),
            details
                .latest_result
                .clone()
                .map_or(serde_json::Value::Null, serde_json::Value::String),
        );
    }
    value
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
    RuntimeRecovery(#[from] RuntimeRecoveryError),
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
    #[error(transparent)]
    ContextCompaction(#[from] crate::agent::ContextCompactionError),
    #[error("模型上下文窗口 {context_window_tokens} Token 小于固定请求开销 {fixed_tokens} Token")]
    ContextBudgetTooSmall {
        context_window_tokens: u32,
        fixed_tokens: u32,
    },
    #[error("上下文压缩在多次尝试后仍未达到模型预算")]
    ContextCompactionDidNotConverge,
    #[error("任务必须处于 New 状态，当前为 {0:?}")]
    TaskNotNew(TaskStatus),
    #[error("任务必须处于 WaitingApproval 状态，当前为 {0:?}")]
    TaskNotWaitingApproval(TaskStatus),
    #[error("审批事件不存在：{0}")]
    ApprovalEventNotFound(EventId),
    #[error("审批事件无效：{0}")]
    InvalidApproval(String),
    #[error("模型流在完成前结束")]
    ModelStreamEndedWithoutCompletion,
    #[error("模型调用被取消")]
    Cancelled,
    #[error("达到最大模型轮数")]
    ModelTurnLimitExceeded,
    #[error(transparent)]
    Model(#[from] ModelError),
}
