//! `koi-server` 的后台 Agent 调度器。
//!
//! Web、QQ 和告警适配器只负责把外部事实写成事件；本模块负责发现排队任务、恢复事件
//! 流并调用 `koi-core::agent::AgentLoop`。它不改变权限结论，也不接受模型返回的权限字段。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use koi_core::agent::{
    AgentLoop, AgentRunOutcome, AgentRunRequest, PersistedAuthorizationEvidenceResolver,
    TaskManager, TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, EventEnvelope, IngressEvent, ModelContextItem, ModelGenerationOptions,
    ModelInputRole, ModelOutputContract, TaskId, TaskStatus, ToolEvent,
};
use koi_core::ports::{
    EventStore, ModelProvider, SourceAuthorizationRegistry, SystemPromptProvider, ToolRegistry,
};
use koi_infra::event_store::JsonlEventStore;
use tokio_util::sync::CancellationToken;

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 进程内的最小 Agent 运行器。
///
/// 当前版本按事件存储轮询任务，适合课程作业和单进程部署。事件存储仍是唯一事实来源，
/// 因此服务重启后可以重新扫描 `Queued` 或等待审批的任务继续运行。主会话由本运行器
/// 调度：收到新输入或子任务回传的工具结果时继续运行；子任务结果通过 `TaskManager`
/// 回传为主会话中的工具事件。
pub struct AgentSupervisor {
    store: Arc<JsonlEventStore>,
    model: Arc<dyn ModelProvider>,
    tools: Arc<ToolRegistry>,
    authorization_providers: Arc<SourceAuthorizationRegistry>,
    prompts: Arc<dyn SystemPromptProvider>,
    task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
    model_options: ModelGenerationOptions,
    max_model_turns: u16,
    max_context_messages: usize,
    max_concurrent_tasks: usize,
    active: Mutex<HashMap<TaskId, CancellationToken>>,
}

impl AgentSupervisor {
    /// 创建后台调度器。所有依赖均由应用层装配，核心循环本身不绑定 Web 或具体模型。
    #[must_use]
    pub fn new(
        store: Arc<JsonlEventStore>,
        model: Arc<dyn ModelProvider>,
        tools: Arc<ToolRegistry>,
        authorization_providers: Arc<SourceAuthorizationRegistry>,
        prompts: Arc<dyn SystemPromptProvider>,
        task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
        model_options: ModelGenerationOptions,
        max_model_turns: u16,
        max_context_messages: usize,
        max_concurrent_tasks: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            model,
            tools,
            authorization_providers,
            prompts,
            task_manager,
            model_options,
            max_model_turns: max_model_turns.max(1),
            max_context_messages: max_context_messages.max(1),
            max_concurrent_tasks: max_concurrent_tasks.max(1),
            active: Mutex::new(HashMap::new()),
        })
    }

    /// 运行任务扫描循环，直到服务关闭。
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => self.tick().await,
            }
        }
    }

    async fn tick(self: &Arc<Self>) {
        let task_ids = match self.store.list_task_ids() {
            Ok(task_ids) => task_ids,
            Err(error) => {
                tracing::error!(%error, "扫描 Agent 任务失败");
                return;
            }
        };

        for task_id in task_ids {
            let Ok(events) = self.store.load_task(task_id).await else {
                tracing::warn!(%task_id, "读取 Agent 任务事件失败");
                continue;
            };
            if events.is_empty() {
                continue;
            }

            if has_cancellation_request(&events) {
                if let Some(token) = self.active_token(task_id) {
                    token.cancel();
                }
            }

            let Ok(runtime) = TaskRuntime::recover(Arc::clone(&self.store), task_id).await else {
                tracing::warn!(%task_id, "恢复 Agent 任务投影失败");
                continue;
            };
            match runtime.projection().status {
                TaskStatus::New | TaskStatus::Queued => {
                    let input_events = context_events(&events, self.max_context_messages);
                    // Web/工具建任务会先写入生命周期事件，再写入首条输入；没有输入时不
                    // 应让模型凭空启动，也避免与仍在提交首条输入的请求竞争事件序号。
                    if !input_events.is_empty() {
                        if task_id.is_main() {
                            self.spawn_main(task_id, input_events, Vec::new());
                        } else {
                            self.spawn_initial(task_id, input_events);
                        }
                    }
                }
                TaskStatus::Running if task_id.is_main() && !self.is_active(task_id) => {
                    // 主会话持续存在：新输入或子任务回传的工具结果都会唤醒一次续跑。
                    let input_events = context_events(&events, self.max_context_messages);
                    let tool_results = pending_tool_results(&events);
                    if !input_events.is_empty() || !tool_results.is_empty() {
                        self.spawn_main(task_id, input_events, tool_results);
                    }
                }
                TaskStatus::WaitingApproval => {
                    if let Some(approval_event_id) = completed_approval(&events) {
                        if task_id.is_main() {
                            self.spawn_main_resume(
                                task_id,
                                approval_event_id,
                                context_events(&events, self.max_context_messages),
                            );
                        } else {
                            self.spawn_resume(
                                task_id,
                                approval_event_id,
                                context_events(&events, self.max_context_messages),
                            );
                        }
                    }
                }
                TaskStatus::Cancelling if !self.is_active(task_id) => {
                    self.finish_cancelling(task_id).await;
                }
                _ => {}
            }

            if !task_id.is_main()
                && runtime.projection().status.is_terminal()
                && !self.is_active(task_id)
            {
                self.deliver_child_result(task_id).await;
            }
        }
    }

    fn spawn_initial(self: &Arc<Self>, task_id: TaskId, input_events: Vec<EventEnvelope>) {
        let Some(cancel) = self.try_start(task_id) else {
            return;
        };
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let result = supervisor
                .execute_initial(task_id, input_events, cancel)
                .await;
            if let Err(error) = result {
                tracing::error!(%task_id, %error, "Agent 任务执行失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish(task_id);
        });
    }

    fn spawn_resume(
        self: &Arc<Self>,
        task_id: TaskId,
        approval_event_id: koi_core::domain::EventId,
        input_events: Vec<EventEnvelope>,
    ) {
        let Some(cancel) = self.try_start(task_id) else {
            return;
        };
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let result = supervisor
                .execute_resume(task_id, approval_event_id, input_events, cancel)
                .await;
            if let Err(error) = result {
                tracing::error!(%task_id, %error, "审批后的 Agent 任务恢复失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish(task_id);
        });
    }

    /// 启动一次主会话运行（初始或续跑）；结果回传触发的续跑携带预构建的工具上下文。
    fn spawn_main(
        self: &Arc<Self>,
        task_id: TaskId,
        input_events: Vec<EventEnvelope>,
        tool_results: Vec<ModelContextItem>,
    ) {
        let Some(cancel) = self.try_start(task_id) else {
            return;
        };
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let result = supervisor
                .execute_main(task_id, input_events, tool_results, cancel)
                .await;
            match result {
                Ok(outcome) => {
                    if let AgentRunOutcome::StartedChildTask { task_id } = outcome {
                        tracing::info!(%task_id, "主会话已启动子任务，等待结果回传");
                    }
                }
                Err(error) => {
                    tracing::error!(%task_id, %error, "主会话运行失败");
                    supervisor
                        .fail_task_if_needed(task_id, error.to_string())
                        .await;
                }
            }
            supervisor.finish(task_id);
        });
    }

    fn spawn_main_resume(
        self: &Arc<Self>,
        task_id: TaskId,
        approval_event_id: koi_core::domain::EventId,
        input_events: Vec<EventEnvelope>,
    ) {
        let Some(cancel) = self.try_start(task_id) else {
            return;
        };
        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let result = supervisor
                .execute_main_resume(task_id, approval_event_id, input_events, cancel)
                .await;
            if let Err(error) = result {
                tracing::error!(%task_id, %error, "主会话审批后的恢复失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish(task_id);
        });
    }

    fn build_agent<'a>(
        &'a self,
        resolver: &'a PersistedAuthorizationEvidenceResolver<'a, JsonlEventStore>,
    ) -> AgentLoop<'a, Arc<JsonlEventStore>> {
        AgentLoop::new(
            self.model.as_ref(),
            self.tools.as_ref(),
            resolver,
            self.authorization_providers.as_ref(),
            None,
            self.prompts.as_ref(),
        )
    }

    fn build_main_agent<'a>(
        &'a self,
        resolver: &'a PersistedAuthorizationEvidenceResolver<'a, JsonlEventStore>,
    ) -> AgentLoop<'a, Arc<JsonlEventStore>> {
        self.build_agent(resolver)
            .with_task_manager(self.task_manager.as_ref())
    }

    async fn execute_initial(
        &self,
        task_id: TaskId,
        input_events: Vec<EventEnvelope>,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, koi_core::agent::AgentLoopError> {
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(koi_core::agent::AgentLoopError::from)?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_agent(&resolver)
            .run(&mut runtime, self.request(input_events, Vec::new()), cancel)
            .await
    }

    async fn execute_resume(
        &self,
        task_id: TaskId,
        approval_event_id: koi_core::domain::EventId,
        input_events: Vec<EventEnvelope>,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, koi_core::agent::AgentLoopError> {
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(koi_core::agent::AgentLoopError::from)?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_agent(&resolver)
            .resume_after_approval(
                &mut runtime,
                self.request(input_events, Vec::new()),
                approval_event_id,
                cancel,
            )
            .await
    }

    async fn execute_main(
        &self,
        task_id: TaskId,
        input_events: Vec<EventEnvelope>,
        tool_results: Vec<ModelContextItem>,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, koi_core::agent::AgentLoopError> {
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(koi_core::agent::AgentLoopError::from)?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_main_agent(&resolver)
            .run_main(&mut runtime, self.request(input_events, tool_results), cancel)
            .await
    }

    async fn execute_main_resume(
        &self,
        task_id: TaskId,
        approval_event_id: koi_core::domain::EventId,
        input_events: Vec<EventEnvelope>,
        cancel: CancellationToken,
    ) -> Result<AgentRunOutcome, koi_core::agent::AgentLoopError> {
        let mut runtime = TaskRuntime::recover(Arc::clone(&self.store), task_id)
            .await
            .map_err(koi_core::agent::AgentLoopError::from)?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_main_agent(&resolver)
            .resume_after_approval(
                &mut runtime,
                self.request(input_events, Vec::new()),
                approval_event_id,
                cancel,
            )
            .await
    }

    /// 将已终止子任务的最终输出回传为主会话事件流中的工具事件。
    ///
    /// 回传只对通过 `task.start` 启动的子任务生效（依据子任务 `TaskCreated` 中的触发
    /// 事件绑定）；方法本身幂等，重复调用不会产生重复的工具结果。
    async fn deliver_child_result(&self, task_id: TaskId) {
        let Ok(mut main) = TaskRuntime::recover(Arc::clone(&self.store), TaskId::MAIN).await else {
            return;
        };
        if main.projection().status.is_terminal() {
            return;
        }
        match self
            .task_manager
            .deliver_child_result(&mut main, task_id)
            .await
        {
            Ok(Some(delivered)) => {
                tracing::info!(
                    task = %task_id,
                    finished_event = %delivered.finished_event_id,
                    "子任务结果已回传为主会话工具事件"
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(task = %task_id, %error, "回传子任务结果失败");
            }
        }
    }

    async fn finish_cancelling(&self, task_id: TaskId) {
        let Ok(mut runtime) = TaskRuntime::recover(Arc::clone(&self.store), task_id).await else {
            return;
        };
        if let Err(error) = runtime
            .record(
                AgentEvent::control(koi_core::domain::ControlEvent::TaskCancelled {
                    reason: "收到取消输入，任务尚未开始执行".into(),
                }),
                None,
            )
            .await
        {
            tracing::warn!(%task_id, %error, "写入任务取消结果失败");
        }
    }

    async fn fail_task_if_needed(&self, task_id: TaskId, reason: String) {
        let Ok(mut runtime) = TaskRuntime::recover(Arc::clone(&self.store), task_id).await else {
            return;
        };
        if runtime.projection().status.is_terminal() {
            return;
        }
        if let Err(error) = runtime
            .record(
                AgentEvent::control(koi_core::domain::ControlEvent::TaskFailed { reason }),
                None,
            )
            .await
        {
            tracing::warn!(%task_id, %error, "写入任务失败结果失败");
        }
    }

    fn request(
        &self,
        input_events: Vec<EventEnvelope>,
        tool_results: Vec<ModelContextItem>,
    ) -> AgentRunRequest {
        AgentRunRequest {
            trigger_event_id: input_events
                .first()
                .map(|event| event.id)
                .or_else(|| tool_results.first().map(|item| item.event_id)),
            context: tool_results,
            input_events,
            memory_query: None,
            output_contract: ModelOutputContract::Text,
            model_options: self.model_options.clone(),
            max_model_turns: self.max_model_turns,
        }
    }

    fn try_start(&self, task_id: TaskId) -> Option<CancellationToken> {
        let Ok(mut active) = self.active.lock() else {
            tracing::error!(%task_id, "Agent 活动任务锁已中毒");
            return None;
        };
        if active.contains_key(&task_id) || active.len() >= self.max_concurrent_tasks {
            return None;
        }
        let cancel = CancellationToken::new();
        active.insert(task_id, cancel.clone());
        Some(cancel)
    }

    fn finish(&self, task_id: TaskId) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&task_id);
        }
    }

    fn is_active(&self, task_id: TaskId) -> bool {
        self.active
            .lock()
            .map(|active| active.contains_key(&task_id))
            .unwrap_or(true)
    }

    fn active_token(&self, task_id: TaskId) -> Option<CancellationToken> {
        self.active
            .lock()
            .ok()
            .and_then(|active| active.get(&task_id).cloned())
    }
}

fn context_events(events: &[EventEnvelope], limit: usize) -> Vec<EventEnvelope> {
    let mut contexts = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                AgentEvent::Ingress(ref ingress)
                    if matches!(ingress.as_ref(), IngressEvent::ContextReceived { .. })
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if contexts.len() > limit {
        let first = contexts.len() - limit;
        contexts.drain(..first);
    }
    contexts
}

/// 主会话续跑时待注入的工具结果上下文。
///
/// 只有最近一次模型完成之后新写入的 `ToolEvent::Finished` 才是模型尚未看到的；同一次
/// 运行内已经继续推理过的工具结果都会被后续的模型完成事件覆盖。
///
/// 工具结果进入会话是一条显式的无权限限制通道：不经过会话最低控制权限审查。安全性
/// 由授权规则保证——工具事件以 `None` 权限持久化且永远不能作为权限父节点，只能被
/// 模型阅读，不能参与提权。
fn pending_tool_results(events: &[EventEnvelope]) -> Vec<ModelContextItem> {
    let last_model_completed = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.payload,
                AgentEvent::Model(ref model)
                    if matches!(model.as_ref(), koi_core::domain::ModelEvent::Completed { .. })
            )
        })
        .map_or(0, |event| event.sequence);
    events
        .iter()
        .filter(|event| event.sequence > last_model_completed)
        .filter_map(|event| match &event.payload {
            AgentEvent::Tool(tool) => match tool.as_ref() {
                ToolEvent::Finished { result, .. } => Some(ModelContextItem {
                    event_id: event.id,
                    role: ModelInputRole::Tool,
                    content: result.summary.clone(),
                    permission: koi_core::domain::PermissionLevel::None,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn has_cancellation_request(events: &[EventEnvelope]) -> bool {
    events.iter().any(|event| {
        matches!(
            event.payload,
            AgentEvent::Ingress(ref ingress)
                if matches!(ingress.as_ref(), IngressEvent::CancellationRequested { .. })
        )
    })
}

fn completed_approval(events: &[EventEnvelope]) -> Option<koi_core::domain::EventId> {
    let (request_event_id, request_sequence) = events.iter().rev().find_map(|event| {
        let AgentEvent::Tool(tool) = &event.payload else {
            return None;
        };
        match tool.as_ref() {
            ToolEvent::ApprovalRequested { .. } => Some((event.id, event.sequence)),
            _ => None,
        }
    })?;

    events.iter().rev().find_map(|event| {
        if event.sequence <= request_sequence {
            return None;
        }
        let AgentEvent::Ingress(ingress) = &event.payload else {
            return None;
        };
        match ingress.as_ref() {
            IngressEvent::ApprovalSubmitted {
                approval_request_event_id,
                ..
            } if *approval_request_event_id == request_event_id => Some(event.id),
            _ => None,
        }
    })
}
