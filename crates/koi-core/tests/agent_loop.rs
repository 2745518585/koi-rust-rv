use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures_util::stream;
use koi_core::agent::{AgentLoop, AgentRunOutcome, AgentRunRequest, TaskRuntime};
use koi_core::domain::{
    AgentEvent, AuthorizationEvidence, AuthorizationEvidenceStatus, AuthorizedToolInvocation,
    EventEnvelope, EventId, EventSource, ModelCapabilities, ModelContextItem, ModelError,
    ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract, ModelProtocol,
    ModelProviderDescriptor, ModelRequest, ModelStreamEvent, PermissionLevel, Principal,
    SourceName, TaskId, ToolCall, ToolDefinition, ToolError, ToolResult, ToolSideEffect, Usage,
};
use koi_core::ports::{
    AuthorizationError, AuthorizationEvidenceResolver, EventStore, EventStoreError,
    ModelEventStream, ModelProvider, PromptError, PromptTaskKind, SourceAuthorizationProvider,
    SourceAuthorizationRegistry, SystemPrompt, SystemPromptProvider, ToolExecutor, ToolRegistry,
};
use serde_json::json;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
struct MemoryEventStore {
    events: Arc<Mutex<Vec<EventEnvelope>>>,
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(&self, event: &EventEnvelope) -> Result<(), EventStoreError> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }
}

struct TwoTurnModel {
    evidence_event_id: EventId,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for TwoTurnModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model: "test-model".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    async fn start(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let outputs = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![ModelOutput::ToolCall(ToolCall {
                name: "server.status".into(),
                arguments: json!({"server": "prod-1"}),
                provider_call_id: Some("call-1".into()),
                authority_parent_event_id: Some(self.evidence_event_id),
            })]
        } else {
            vec![ModelOutput::Text {
                text: "诊断完成".into(),
            }]
        };
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
                provider_response_id: None,
            }),
        )])))
    }
}

struct EvidenceResolver {
    ingress_event_id: EventId,
}

struct TestPromptProvider;

impl SystemPromptProvider for TestPromptProvider {
    fn prompt_for(&self, task_kind: PromptTaskKind) -> Result<SystemPrompt, PromptError> {
        Ok(SystemPrompt {
            content: match task_kind {
                PromptTaskKind::Main => "主会话提示词",
                PromptTaskKind::Child => "子任务提示词",
            }
            .into(),
        })
    }
}

#[async_trait]
impl AuthorizationEvidenceResolver for EvidenceResolver {
    async fn resolve(
        &self,
        _task_id: TaskId,
        event_id: EventId,
    ) -> Result<AuthorizationEvidence, AuthorizationError> {
        if event_id != self.ingress_event_id {
            return Ok(AuthorizationEvidence {
                event_id,
                source: EventSource::Model,
                event_kind: koi_core::domain::AuthorizationEvidenceEventKind::Tool,
                principal: None,
                source_maximum_permission: PermissionLevel::None,
                permission: PermissionLevel::None,
                status: AuthorizationEvidenceStatus::Active,
                authority_parent_event_id: Some(self.ingress_event_id),
                expires_at: None,
                approval_request_event_id: None,
            });
        }
        Ok(AuthorizationEvidence {
            event_id,
            source: EventSource::External(SourceName::new("qq").unwrap()),
            event_kind: koi_core::domain::AuthorizationEvidenceEventKind::Ingress,
            principal: Some(Principal::new("qq", "10001")),
            source_maximum_permission: PermissionLevel::User,
            permission: PermissionLevel::User,
            status: AuthorizationEvidenceStatus::Active,
            authority_parent_event_id: None,
            expires_at: None,
            approval_request_event_id: None,
        })
    }
}

struct StatusTool {
    definition: ToolDefinition,
    calls: AtomicUsize,
}

impl StatusTool {
    fn new() -> Self {
        Self::with_permission(PermissionLevel::User)
    }

    fn with_permission(required_permission: PermissionLevel) -> Self {
        Self {
            definition: ToolDefinition {
                name: "server.status".into(),
                description: "查询服务器状态".into(),
                input_schema: json!({"type": "object"}),
                required_permission,
                side_effect: ToolSideEffect::ReadOnly,
                timeout_ms: 1_000,
                model_visible: true,
                main_session_only: false,            },
            calls: AtomicUsize::new(0),
        }
    }
}

struct PendingQqAuthorizationProvider;

#[async_trait]
impl SourceAuthorizationProvider for PendingQqAuthorizationProvider {
    fn source(&self) -> &'static str {
        "qq"
    }

    async fn request_authorization(
        &self,
        _request: koi_core::domain::AuthorizationRequest,
    ) -> Result<koi_core::domain::AuthorizationRequestResult, AuthorizationError> {
        Ok(koi_core::domain::AuthorizationRequestResult::Pending)
    }
}

#[async_trait]
impl ToolExecutor for StatusTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        _invocation: AuthorizedToolInvocation,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            summary: "prod-1 正常".into(),
            data: json!({"status": "healthy"}),
            truncated: false,
        })
    }
}

#[tokio::test]
async fn main_loop_records_model_tool_and_final_response() {
    let evidence_event_id = EventId::new();
    let model = TwoTurnModel {
        evidence_event_id,
        calls: AtomicUsize::new(0),
    };
    let tool = Arc::new(StatusTool::new());
    let mut tools = ToolRegistry::default();
    tools
        .register(Arc::clone(&tool) as Arc<dyn ToolExecutor>)
        .unwrap();

    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    let resolver = EvidenceResolver {
        ingress_event_id: evidence_event_id,
    };
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);

    let outcome = agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(evidence_event_id),
                context: vec![ModelContextItem {
                    event_id: evidence_event_id,
                    role: ModelInputRole::User,
                    content: "检查 prod-1".into(),
                    permission: PermissionLevel::User,
                }],
                input_events: vec![],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 3,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        AgentRunOutcome::Completed {
            response: Some("诊断完成".into()),
        }
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.projection().status,
        koi_core::domain::TaskStatus::Running
    );
    let recorded_events = events.lock().await;
    assert!(recorded_events.len() >= 11);
    let proposed = recorded_events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                AgentEvent::Tool(ref tool)
                    if matches!(tool.as_ref(), koi_core::domain::ToolEvent::Proposed { .. })
            )
        })
        .unwrap();
    assert_eq!(proposed.provenance.creator, EventSource::Model);
    assert_eq!(
        proposed.provenance.authority_parent_event_id,
        Some(evidence_event_id)
    );
    let finished = recorded_events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                AgentEvent::Tool(ref tool)
                    if matches!(tool.as_ref(), koi_core::domain::ToolEvent::Finished { .. })
            )
        })
        .unwrap();
    assert_eq!(finished.provenance.creator, EventSource::Tool);
    assert_eq!(finished.provenance.authority_parent_event_id, None);
    assert!(!recorded_events.iter().any(|event| matches!(
        event.payload,
        AgentEvent::Control(ref control)
            if matches!(control.as_ref(), koi_core::domain::ControlEvent::TaskCompleted { .. })
    )));
}

#[tokio::test]
async fn main_loop_waits_for_source_authorization_before_privileged_tool_execution() {
    let evidence_event_id = EventId::new();
    let model = TwoTurnModel {
        evidence_event_id,
        calls: AtomicUsize::new(0),
    };
    let tool = Arc::new(StatusTool::with_permission(PermissionLevel::Operator));
    let mut tools = ToolRegistry::default();
    tools
        .register(Arc::clone(&tool) as Arc<dyn ToolExecutor>)
        .unwrap();
    let mut providers = SourceAuthorizationRegistry::default();
    providers
        .register(Arc::new(PendingQqAuthorizationProvider) as Arc<dyn SourceAuthorizationProvider>)
        .unwrap();

    let mut runtime = TaskRuntime::new(MemoryEventStore::default(), TaskId::new());
    let resolver = EvidenceResolver {
        ingress_event_id: evidence_event_id,
    };
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);
    let outcome = agent
        .run(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(evidence_event_id),
                context: vec![ModelContextItem {
                    event_id: evidence_event_id,
                    role: ModelInputRole::User,
                    content: "重启 prod-1 服务".into(),
                    permission: PermissionLevel::User,
                }],
                input_events: vec![],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 3,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome,
        AgentRunOutcome::AwaitingAuthorization { .. }
    ));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

struct TaskStartModel {
    evidence_event_id: EventId,
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for TaskStartModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model: "test-model".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    async fn start(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(vec![Ok(ModelStreamEvent::Completed(
            koi_core::domain::ModelTurn {
                outputs: vec![ModelOutput::ToolCall(ToolCall {
                    name: "task.start".into(),
                    arguments: json!({"message": "巡检磁盘空间并汇报"}),
                    provider_call_id: Some("call-task-1".into()),
                    authority_parent_event_id: Some(self.evidence_event_id),
                })],
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
                provider_response_id: None,
            },
        ))])))
    }
}

#[tokio::test]
async fn main_session_task_start_creates_child_and_returns_pending_external() {
    use koi_core::agent::TaskManager;
    use koi_core::domain::{ControlEvent, IngressEvent, ToolEvent};

    let evidence_event_id = EventId::new();
    let model = TaskStartModel {
        evidence_event_id,
        calls: AtomicUsize::new(0),
    };
    let mut tools = ToolRegistry::default();
    let registered = koi_core::agent::task_tools::register_task_management_tools(&mut tools)
        .expect("任务管理工具注册失败");
    assert_eq!(registered, 4);

    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let manager = TaskManager::new(Arc::new(store.clone()));
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    let resolver = EvidenceResolver {
        ingress_event_id: evidence_event_id,
    };
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent =
        AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts)
            .with_task_manager(&manager);

    let outcome = agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(evidence_event_id),
                context: vec![ModelContextItem {
                    event_id: evidence_event_id,
                    role: ModelInputRole::User,
                    content: "巡检磁盘".into(),
                    permission: PermissionLevel::User,
                }],
                input_events: vec![],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 3,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let AgentRunOutcome::StartedChildTask { task_id } = outcome else {
        panic!("task.start 应使本轮以 StartedChildTask 结束");
    };
    assert_ne!(task_id, TaskId::MAIN);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);

    let recorded = events.lock().await;
    // 主会话流必须留有跨任务创建审计，并且 Started 的 causation 指向 Accepted 事件。
    let accepted = recorded
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::TaskOperationAccepted {
                    target_task_id, ..
                } if *target_task_id == task_id => Some(event.id),
                _ => None,
            },
            _ => None,
        })
        .expect("主会话流必须记录跨任务创建的接受事件");
    let started = recorded
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                AgentEvent::Tool(tool) if matches!(tool.as_ref(), ToolEvent::Started { .. })
            )
        })
        .expect("主会话流必须记录 task.start 的 Started 事件");
    assert_eq!(started.causation_id, Some(accepted));
    assert_eq!(started.task_id, TaskId::MAIN);
    // fail-closed 的桩执行器不应被直接调用：不允许出现 Failed 工具事件。
    assert!(!recorded.iter().any(|event| matches!(
        &event.payload,
        AgentEvent::Tool(tool) if matches!(tool.as_ref(), ToolEvent::Failed { .. })
    )));

    // 子任务流：TaskCreated(trigger=accepted) -> TaskQueued -> 首条输入（System 记录）。
    let child_events: Vec<&EventEnvelope> = recorded
        .iter()
        .filter(|event| event.task_id == task_id)
        .collect();
    assert_eq!(child_events.len(), 3);
    let ControlEvent::TaskCreated {
        trigger_event_id: Some(trigger),
    } = (match &child_events[0].payload {
        AgentEvent::Control(control) => control.as_ref(),
        _ => panic!("子任务首事件必须是 TaskCreated"),
    }) else {
        panic!("子任务 TaskCreated 必须绑定主会话 task.start 审计事件");
    };
    assert_eq!(*trigger, accepted);
    assert!(
        matches!(&child_events[1].payload, AgentEvent::Control(control) if matches!(control.as_ref(), ControlEvent::TaskQueued))
    );
    assert!(
        matches!(&child_events[2].payload, AgentEvent::Ingress(ingress) if matches!(ingress.as_ref(), IngressEvent::ContextReceived { .. }))
    );
    // 子任务尚未运行：不应有任何模型事件。
    assert!(child_events.iter().all(|event| !matches!(event.payload, AgentEvent::Model(_))));
}
