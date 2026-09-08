//! `koi-server` 的后台 Agent 调度器。
//!
//! Web、QQ 和告警适配器只负责把外部事实写成事件；本模块负责发现排队任务、恢复事件
//! 流并调用 `koi-core::agent::AgentLoop`。它不改变权限结论，也不接受模型返回的权限字段。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use koi_core::agent::{
    AgentLoop, AgentRunOutcome, AgentRunRequest, PersistedAuthorizationEvidenceResolver,
    TaskManager, TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, ControlEvent, EventEnvelope, IngressEvent, ModelContextItem, ModelError,
    ModelErrorKind, ModelEvent, ModelInputRole, ModelOutputContract, ModelSelection, TaskId,
    TaskStatus, ToolEvent,
};
use koi_core::ports::{
    EventStore, ModelProvider, SourceAuthorizationRegistry, SystemPromptProvider, ToolRegistry,
};
use koi_infra::event_store::JsonlEventStore;
use koi_infra::llm::{ModelProviderEntry, ModelProviderRegistry, ModelRegistryError};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::model_trace::ModelTrace;

/// 进程内的最小 Agent 运行器。
///
/// 当前版本使用事件存储广播唤醒任务调度，适合课程作业和单进程部署。事件存储仍是唯一
/// 事实来源，因此服务重启后会先做一次全量扫描，随后只在有新事件时恢复受影响的任务。
/// 主会话由本运行器调度：收到新输入或子任务回传的工具结果时继续运行；子任务结果通过
/// `TaskManager` 回传为主会话中的工具事件。
pub struct AgentSupervisor {
    store: Arc<JsonlEventStore>,
    models: Arc<ModelProviderRegistry>,
    tools: Arc<ToolRegistry>,
    authorization_providers: Arc<SourceAuthorizationRegistry>,
    prompts: Arc<dyn SystemPromptProvider>,
    task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
    reasoning: Arc<ModelTrace>,
    max_model_turns: u16,
    max_concurrent_tasks: usize,
    active: Mutex<HashMap<TaskId, CancellationToken>>,
    last_models: Mutex<HashMap<TaskId, ModelSelection>>,
    wake: Notify,
}

impl AgentSupervisor {
    /// 创建后台调度器。所有依赖均由应用层装配，核心循环本身不绑定 Web 或具体模型。
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<JsonlEventStore>,
        models: Arc<ModelProviderRegistry>,
        tools: Arc<ToolRegistry>,
        authorization_providers: Arc<SourceAuthorizationRegistry>,
        prompts: Arc<dyn SystemPromptProvider>,
        task_manager: Arc<TaskManager<Arc<JsonlEventStore>>>,
        reasoning: Arc<ModelTrace>,
        max_model_turns: u16,
        max_concurrent_tasks: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            models,
            tools,
            authorization_providers,
            prompts,
            task_manager,
            reasoning,
            max_model_turns: max_model_turns.max(1),
            max_concurrent_tasks: max_concurrent_tasks.max(1),
            active: Mutex::new(HashMap::new()),
            last_models: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        })
    }

    /// 运行任务调度循环，直到服务关闭。
    ///
    /// 启动时的全量扫描负责恢复服务重启前已经排队的任务。运行中直接订阅事件存储，
    /// 并在一次唤醒前清空已经积压的通知，让一批连续写入的模型、工具事件只触发一次
    /// 全量状态检查。这样空闲时不会反复读取和解析所有任务文件，也不会因为一次检查
    /// 超过固定轮询周期而进入 Tokio 忙等。
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let mut notifications = self.store.subscribe();
        self.tick().await;

        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = self.wake.notified() => self.tick().await,
                notification = notifications.recv() => {
                    match notification {
                        Ok(_) => {
                            while notifications.try_recv().is_ok() {}
                            self.tick().await;
                        }
                        Err(RecvError::Lagged(_)) => self.tick().await,
                        Err(RecvError::Closed) => break,
                    }
                }
            }
        }
    }

    /// Reports whether a task currently owns an active model execution slot.
    #[must_use]
    pub fn is_task_active(&self, task_id: TaskId) -> bool {
        self.is_active(task_id)
    }

    /// Drops provider-specific continuation state for one task.
    ///
    /// Local maintenance uses this after an explicit context clear so the next model call cannot
    /// resume a provider-side conversation that no longer matches the persisted context.
    pub fn reset_model_state(&self, task_id: TaskId) {
        self.models.reset_task(task_id);
    }

    async fn tick(self: &Arc<Self>) {
        let task_ids = match JsonlEventStore::list_task_ids(self.store.as_ref()) {
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

            // 暂停与取消都会向活跃循环发出取消信号。二者的差别在于：取消最终写入
            // `TaskCancelled`，暂停则等待循环真正退出后再确认 `TaskPaused`。
            if has_cancellation_request(&events) || has_pause_request(&events) {
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
                    let input_events = context_events(&events);
                    // Web/工具建任务会先写入生命周期事件，再写入首条输入；没有输入时不
                    // 应让模型凭空启动，也避免与仍在提交首条输入的请求竞争事件序号。
                    // 恢复后的上下文已经在事件存储中，不需要再伪造一条输入；但仍需
                    // 主动启动一次模型循环。普通新建任务则仍要求至少有一条输入。
                    if !input_events.is_empty() || was_resumed(&events) {
                        if task_id.is_main() {
                            self.spawn_main(task_id, input_events, Vec::new());
                        } else {
                            self.spawn_initial(task_id, input_events);
                        }
                    }
                }
                TaskStatus::Running if task_id.is_main() && !self.is_active(task_id) => {
                    // 主会话持续存在：新输入或子任务回传的工具结果都会唤醒一次续跑。
                    // 已经出现在某次模型调用 context_event_ids 中的上下文不会重复注入；
                    // 这里只收集新的外部输入和仍未注入的工具结果，避免主会话在空闲时
                    // 每 250ms 重复执行同一条请求。
                    let input_events = context_events_since_last_model(&events);
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
                                context_events(&events),
                            );
                        } else {
                            self.spawn_resume(task_id, approval_event_id, context_events(&events));
                        }
                    }
                }
                TaskStatus::Pausing if !self.is_active(task_id) => {
                    self.finish_pausing(task_id, &events).await;
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
            if let Err(error) = &result {
                tracing::error!(%task_id, %error, "Agent 任务执行失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish_run(task_id, &result);
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
            if let Err(error) = &result {
                tracing::error!(%task_id, %error, "审批后的 Agent 任务恢复失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish_run(task_id, &result);
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
            match &result {
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
            supervisor.finish_run(task_id, &result);
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
            if let Err(error) = &result {
                tracing::error!(%task_id, %error, "主会话审批后的恢复失败");
                supervisor
                    .fail_task_if_needed(task_id, error.to_string())
                    .await;
            }
            supervisor.finish_run(task_id, &result);
        });
    }

    fn build_agent<'a>(
        &'a self,
        model: &'a dyn ModelProvider,
        resolver: &'a PersistedAuthorizationEvidenceResolver<'a, JsonlEventStore>,
    ) -> AgentLoop<'a, Arc<JsonlEventStore>> {
        AgentLoop::new(
            model,
            self.tools.as_ref(),
            resolver,
            self.authorization_providers.as_ref(),
            None,
            self.prompts.as_ref(),
        )
        .with_reasoning_sink(self.reasoning.as_ref())
    }

    fn build_main_agent<'a>(
        &'a self,
        model: &'a dyn ModelProvider,
        resolver: &'a PersistedAuthorizationEvidenceResolver<'a, JsonlEventStore>,
    ) -> AgentLoop<'a, Arc<JsonlEventStore>> {
        self.build_agent(model, resolver)
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
        let model = self.resolve_model(task_id, runtime.projection().selected_model.as_ref())?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_agent(model.provider.as_ref(), &resolver)
            .run(
                &mut runtime,
                self.request(model, input_events, Vec::new()),
                cancel,
            )
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
        let model = self.resolve_model(task_id, runtime.projection().selected_model.as_ref())?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_agent(model.provider.as_ref(), &resolver)
            .resume_after_approval(
                &mut runtime,
                self.request(model, input_events, Vec::new()),
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
        let model = self.resolve_model(task_id, runtime.projection().selected_model.as_ref())?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_main_agent(model.provider.as_ref(), &resolver)
            .run_main(
                &mut runtime,
                self.request(model, input_events, tool_results),
                cancel,
            )
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
        let model = self.resolve_model(task_id, runtime.projection().selected_model.as_ref())?;
        let resolver = PersistedAuthorizationEvidenceResolver::new(self.store.as_ref());
        self.build_main_agent(model.provider.as_ref(), &resolver)
            .resume_after_approval(
                &mut runtime,
                self.request(model, input_events, Vec::new()),
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
        self.models.reset_task(task_id);
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

    /// 在活跃循环退出后确认暂停。此处而非控制入口写入 `TaskPaused`，保证前端看到
    /// “已暂停”时不会再有本任务的模型循环占用执行槽位。
    async fn finish_pausing(&self, task_id: TaskId, events: &[EventEnvelope]) {
        let Some(reason) = pause_reason(events) else {
            tracing::warn!(%task_id, "暂停中的任务缺少暂停请求事件");
            return;
        };
        let Ok(mut runtime) = TaskRuntime::recover(Arc::clone(&self.store), task_id).await else {
            return;
        };
        if runtime.projection().status != TaskStatus::Pausing {
            return;
        }
        if let Err(error) = runtime
            .record(
                AgentEvent::control(ControlEvent::TaskPaused { reason }),
                None,
            )
            .await
        {
            tracing::warn!(%task_id, %error, "写入任务暂停确认失败");
        }
    }

    async fn fail_task_if_needed(&self, task_id: TaskId, reason: String) {
        let Ok(mut runtime) = TaskRuntime::recover(Arc::clone(&self.store), task_id).await else {
            return;
        };
        if runtime.projection().status.is_terminal()
            || matches!(
                runtime.projection().status,
                TaskStatus::Pausing | TaskStatus::Paused
            )
        {
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
        model: &ModelProviderEntry,
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
            model_options: model.model_options.clone(),
            max_model_turns: self.max_model_turns,
        }
    }

    fn resolve_model(
        &self,
        task_id: TaskId,
        selected_model: Option<&ModelSelection>,
    ) -> Result<&ModelProviderEntry, koi_core::agent::AgentLoopError> {
        let (selection, entry) = self
            .models
            .resolve(selected_model)
            .map_err(model_selection_error)?;
        let changed = self.last_models.lock().is_ok_and(|mut last_models| {
            let changed = last_models
                .get(&task_id)
                .is_some_and(|previous| previous != selection);
            last_models.insert(task_id, selection.clone());
            changed
        });
        if changed {
            self.models.reset_task(task_id);
        }
        Ok(entry)
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
        // 输入或工具结果可能在本轮运行期间到达；当本轮释放执行槽位后重新扫描，
        // 避免对应的事件通知已经被“活跃任务”分支消费后留下未处理事件。
        self.wake.notify_one();
    }

    /// 执行周期结束后的统一清理。
    ///
    /// 等待审批和正常完成都可能需要保留 Provider 的工具续接状态；只有取消或异常
    /// 结束才可以清理它。这样下一次同一会话输入会获得全新的模型调用边界。
    fn finish_run(
        &self,
        task_id: TaskId,
        result: &Result<AgentRunOutcome, koi_core::agent::AgentLoopError>,
    ) {
        if result.is_err() || matches!(result.as_ref(), Ok(AgentRunOutcome::Cancelled)) {
            self.models.reset_task(task_id);
        }
        self.finish(task_id);
    }

    fn is_active(&self, task_id: TaskId) -> bool {
        self.active
            .lock()
            .map_or(true, |active| active.contains_key(&task_id))
    }

    fn active_token(&self, task_id: TaskId) -> Option<CancellationToken> {
        self.active
            .lock()
            .ok()
            .and_then(|active| active.get(&task_id).cloned())
    }
}

#[allow(clippy::needless_pass_by_value)]
fn model_selection_error(error: ModelRegistryError) -> koi_core::agent::AgentLoopError {
    ModelError::new(
        ModelErrorKind::InvalidResponse,
        format!("会话模型选择无效：{error}"),
        false,
    )
    .into()
}

fn context_events(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    let events = current_cycle_events(events);
    let rejected_input_event_ids = events
        .iter()
        .filter_map(|event| match &event.payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::InputRejected { input_event_id, .. } => Some(*input_event_id),
                _ => None,
            },
            _ => None,
        })
        .collect::<HashSet<_>>();
    events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                AgentEvent::Ingress(ref ingress)
                    if matches!(ingress.as_ref(), IngressEvent::ContextReceived { .. })
            )
        })
        .filter(|event| !rejected_input_event_ids.contains(&event.id))
        .cloned()
        .collect()
}

fn context_events_since_last_model(events: &[EventEnvelope]) -> Vec<EventEnvelope> {
    let events = current_cycle_events(events);
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
    context_events(
        &events
            .iter()
            .filter(|event| event.sequence > last_model_completed)
            .cloned()
            .collect::<Vec<_>>(),
    )
}

/// 主会话续跑时待注入的工具结果上下文。
///
/// 模型是否已经看到工具结果，以 `ModelEvent::CallStarted.context_event_ids` 为准，
/// 而不是以事件序号或最近一次模型完成事件为准。
///
/// 这一区别对异步子任务很重要：子任务结果可能在主会话模型调用已经开始后、该调用
/// 完成前到达。此时结果事件的序号小于 `ModelEvent::Completed`，但它并没有出现在该
/// 调用的 `context_event_ids` 中，必须保留到下一次模型调用。
///
/// 工具结果进入会话是一条显式的无权限限制通道：不经过会话最低控制权限审查。安全性
/// 由授权规则保证——工具事件以 `None` 权限持久化且永远不能作为权限父节点，只能被
/// 模型阅读，不能参与提权。
fn pending_tool_results(events: &[EventEnvelope]) -> Vec<ModelContextItem> {
    let events = current_cycle_events(events);
    let injected_context_event_ids: HashSet<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            AgentEvent::Model(model) => match model.as_ref() {
                ModelEvent::CallStarted {
                    context_event_ids, ..
                } => Some(context_event_ids.iter()),
                _ => None,
            },
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    events
        .iter()
        .filter(|event| !injected_context_event_ids.contains(&event.id))
        .filter_map(|event| match &event.payload {
            AgentEvent::Tool(tool) => match tool.as_ref() {
                ToolEvent::Finished { result, .. } => Some(ModelContextItem {
                    event_id: event.id,
                    role: ModelInputRole::Tool,
                    content: result.model_content(),
                    permission: koi_core::domain::PermissionLevel::None,
                }),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn has_cancellation_request(events: &[EventEnvelope]) -> bool {
    current_cycle_events(events).iter().any(|event| {
        matches!(
            event.payload,
            AgentEvent::Ingress(ref ingress)
                if matches!(ingress.as_ref(), IngressEvent::CancellationRequested { .. })
        ) || matches!(
            event.payload,
            AgentEvent::Control(ref control)
                if matches!(control.as_ref(), ControlEvent::TaskCancelled { .. })
        )
    })
}

/// 当前执行周期是否已提出暂停请求。
fn has_pause_request(events: &[EventEnvelope]) -> bool {
    current_cycle_events(events).iter().any(|event| {
        matches!(
            event.payload,
            AgentEvent::Control(ref control)
                if matches!(control.as_ref(), ControlEvent::PauseRequested { .. })
        )
    })
}

/// 读取当前暂停请求的说明，用于运行器写入最终的暂停确认事件。
fn pause_reason(events: &[EventEnvelope]) -> Option<String> {
    current_cycle_events(events).iter().rev().find_map(|event| {
        let AgentEvent::Control(control) = &event.payload else {
            return None;
        };
        match control.as_ref() {
            ControlEvent::PauseRequested { reason } => Some(reason.clone()),
            _ => None,
        }
    })
}

/// 判断当前排队状态是否由恢复请求产生，而非仅由新建或普通入队产生。
fn was_resumed(events: &[EventEnvelope]) -> bool {
    events.iter().rev().find_map(|event| {
        let AgentEvent::Control(control) = &event.payload else {
            return None;
        };
        match control.as_ref() {
            ControlEvent::ResumeRequested => Some(true),
            ControlEvent::TaskQueued => Some(false),
            _ => None,
        }
    }) == Some(true)
}

fn completed_approval(events: &[EventEnvelope]) -> Option<koi_core::domain::EventId> {
    let events = current_cycle_events(events);
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

/// 最近一次入队或恢复请求界定当前工作周期。主会话在重启后重新入队时，旧周期的输入、
/// 取消和暂停都只能保留为审计历史，不能再影响新的对话。
fn current_cycle_events(events: &[EventEnvelope]) -> &[EventEnvelope] {
    let start = events
        .iter()
        .rposition(|event| {
            matches!(
                event.payload,
                AgentEvent::Control(ref control)
                    if matches!(
                        control.as_ref(),
                        ControlEvent::TaskQueued | ControlEvent::ResumeRequested
                    )
            )
        })
        .map_or(0, |index| index.saturating_add(1));
    &events[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use koi_core::domain::{EventId, ToolResult, Usage};

    fn finished_tool(sequence: u64, summary: &str) -> EventEnvelope {
        EventEnvelope::new(
            TaskId::MAIN,
            sequence,
            None,
            AgentEvent::tool(ToolEvent::Finished {
                execution_started_event_id: EventId::new(),
                result: ToolResult {
                    summary: summary.to_owned(),
                    data: serde_json::Value::Null,
                    truncated: false,
                },
            }),
        )
    }

    fn model_call_started(sequence: u64, context_event_ids: Vec<EventId>) -> EventEnvelope {
        EventEnvelope::new(
            TaskId::MAIN,
            sequence,
            None,
            AgentEvent::model(ModelEvent::CallStarted {
                context_event_ids,
                context_hash: "test-context".to_owned(),
                provider: "test".to_owned(),
                model_id: "test-model".to_owned(),
            }),
        )
    }

    fn model_completed(sequence: u64, call_started_event_id: EventId) -> EventEnvelope {
        EventEnvelope::new(
            TaskId::MAIN,
            sequence,
            Some(call_started_event_id),
            AgentEvent::model(ModelEvent::Completed {
                call_started_event_id,
                outputs: Vec::new(),
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
            }),
        )
    }

    #[test]
    fn pending_tool_results_uses_model_context_ids_instead_of_sequence_watermark() {
        let queued = EventEnvelope::new(
            TaskId::MAIN,
            1,
            None,
            AgentEvent::control(ControlEvent::TaskQueued),
        );
        let already_injected = finished_tool(2, "already injected");
        let first_call = model_call_started(3, vec![already_injected.id]);
        let arrived_during_call = finished_tool(4, "arrived during call");
        let completed = model_completed(5, first_call.id);

        let events = vec![
            queued,
            already_injected.clone(),
            first_call.clone(),
            arrived_during_call.clone(),
            completed,
        ];
        let pending = pending_tool_results(&events);

        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, arrived_during_call.id);
        assert_eq!(pending[0].content, "arrived during call");

        let second_call = model_call_started(6, vec![already_injected.id, arrived_during_call.id]);
        let events = [events, vec![second_call]].concat();
        assert!(pending_tool_results(&events).is_empty());
    }

    #[test]
    fn resume_starts_a_new_cycle_without_reusing_old_pause_request() {
        let queued = EventEnvelope::new(
            TaskId::MAIN,
            1,
            None,
            AgentEvent::control(ControlEvent::TaskQueued),
        );
        let pause = EventEnvelope::new(
            TaskId::MAIN,
            2,
            None,
            AgentEvent::control(ControlEvent::PauseRequested {
                reason: "暂停".into(),
            }),
        );
        let paused = EventEnvelope::new(
            TaskId::MAIN,
            3,
            Some(pause.id),
            AgentEvent::control(ControlEvent::TaskPaused {
                reason: "暂停".into(),
            }),
        );
        let resume = EventEnvelope::new(
            TaskId::MAIN,
            4,
            None,
            AgentEvent::control(ControlEvent::ResumeRequested),
        );
        let events = vec![queued, pause, paused, resume];

        assert!(was_resumed(&events));
        assert!(!has_pause_request(&events));
        assert!(current_cycle_events(&events).is_empty());
    }
}
