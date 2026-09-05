use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures_util::stream;
use koi_core::agent::{
    AgentLoop, AgentRunOutcome, AgentRunRequest, PersistedAuthorizationEvidenceResolver,
    TaskManager, TaskRuntime,
};
use koi_core::domain::{
    AgentEvent, AuthorizationEvidence, AuthorizationEvidenceStatus, AuthorizedToolInvocation,
    EventEnvelope, EventId, EventSource, ModelCapabilities, ModelContextItem, ModelDeltaKind,
    ModelError, ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract,
    ModelProtocol, ModelProviderDescriptor, ModelRequest, ModelStreamEvent, PermissionLevel,
    Principal, SourceName, TaskId, ToolCall, ToolDefinition, ToolError, ToolResult, ToolSideEffect,
    Usage,
};
use koi_core::ports::{
    AuthorizationError, AuthorizationEvidenceResolver, EventStore, EventStoreError,
    InMemoryEventStore, ModelEventStream, ModelProvider, PromptError, PromptTaskKind,
    SourceAuthorizationProvider, SourceAuthorizationRegistry, SystemPrompt, SystemPromptProvider,
    ToolExecutor, ToolRegistry,
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

    async fn load_task(&self, task_id: TaskId) -> Result<Vec<EventEnvelope>, EventStoreError> {
        Ok(self
            .events
            .lock()
            .await
            .iter()
            .filter(|event| event.task_id == task_id)
            .cloned()
            .collect())
    }

    async fn load_event_any(
        &self,
        event_id: EventId,
    ) -> Result<Option<EventEnvelope>, EventStoreError> {
        Ok(self
            .events
            .lock()
            .await
            .iter()
            .find(|event| event.id == event_id)
            .cloned())
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
            model_id: "test-model".into(),
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
        Ok(Box::pin(stream::iter(vec![
            Ok(ModelStreamEvent::Delta {
                sequence: 0,
                kind: ModelDeltaKind::Text,
                content: "临时流式片段".into(),
            }),
            Ok(ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
                provider_response_id: None,
            })),
        ])))
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
                main_session_only: false,
            },
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
        AgentEvent::Model(ref model)
            if matches!(model.as_ref(), koi_core::domain::ModelEvent::Delta { .. })
    )));
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
            model_id: "test-model".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    async fn start(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if call == 0 {
            ModelOutput::ToolCall(ToolCall {
                name: "task.start".into(),
                arguments: json!({}),
                provider_call_id: Some("call-task-1".into()),
                authority_parent_event_id: Some(self.evidence_event_id),
            })
        } else {
            ModelOutput::Text {
                text: "子任务已创建，等待输入".into(),
            }
        };
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![output],
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn main_session_task_start_creates_queued_child_without_input() {
    use koi_core::agent::TaskManager;
    use koi_core::domain::{ControlEvent, ToolEvent};

    let evidence_event_id = EventId::new();
    let model = TaskStartModel {
        evidence_event_id,
        calls: AtomicUsize::new(0),
    };
    let mut tools = ToolRegistry::default();
    let registered = koi_core::agent::task_tools::register_task_management_tools(&mut tools)
        .expect("任务管理工具注册失败");
    assert_eq!(registered, 7);

    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let manager = TaskManager::new(Arc::new(store.clone()));
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    let resolver = EvidenceResolver {
        ingress_event_id: evidence_event_id,
    };
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts)
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

    assert!(matches!(
        outcome,
        AgentRunOutcome::Completed { response: Some(_) }
    ));
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);

    let recorded = events.lock().await;
    let task_id = recorded
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::TaskOperationAccepted { target_task_id, .. }
                    if !target_task_id.is_main() =>
                {
                    Some(*target_task_id)
                }
                _ => None,
            },
            _ => None,
        })
        .expect("主会话流必须记录新子任务 ID");
    assert_ne!(task_id, TaskId::MAIN);
    // 主会话流必须留有跨任务创建审计，并且 Started 的 causation 指向 Accepted 事件。
    let accepted = recorded
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Control(control) => match control.as_ref() {
                ControlEvent::TaskOperationAccepted { target_task_id, .. }
                    if *target_task_id == task_id =>
                {
                    Some(event.id)
                }
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

    // 子任务流：TaskCreated(trigger=accepted) -> TaskQueued；具体输入由 task.input 投递。
    let child_events: Vec<&EventEnvelope> = recorded
        .iter()
        .filter(|event| event.task_id == task_id)
        .collect();
    assert_eq!(child_events.len(), 2);
    let ControlEvent::TaskCreated {
        trigger_event_id: Some(trigger),
    } = (match &child_events[0].payload {
        AgentEvent::Control(control) => control.as_ref(),
        _ => panic!("子任务首事件必须是 TaskCreated"),
    })
    else {
        panic!("子任务 TaskCreated 必须绑定主会话 task.start 审计事件");
    };
    assert_eq!(*trigger, accepted);
    assert!(
        matches!(&child_events[1].payload, AgentEvent::Control(control) if matches!(control.as_ref(), ControlEvent::TaskQueued))
    );
    // 子任务尚未收到输入，也不应有任何模型事件。
    assert!(
        child_events
            .iter()
            .all(|event| !matches!(event.payload, AgentEvent::Model(_)))
    );
}

struct TaskDiscoveryModel {
    calls: AtomicUsize,
    contexts: Arc<Mutex<Vec<Vec<ModelContextItem>>>>,
}

#[async_trait]
impl ModelProvider for TaskDiscoveryModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model_id: "task-discovery-model".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    async fn start(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.contexts.lock().await.push(request.context);
        let output = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ModelOutput::ToolCall(ToolCall {
                name: "task.list".into(),
                arguments: json!({}),
                provider_call_id: Some("task-list-1".into()),
                authority_parent_event_id: None,
            })
        } else {
            ModelOutput::Text {
                text: "已查看任务列表".into(),
            }
        };
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![output],
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

#[tokio::test]
async fn main_model_can_discover_persisted_children_with_task_list() {
    let store = Arc::new(InMemoryEventStore::default());
    let child_id = TaskId::new();
    let mut child = TaskRuntime::new(Arc::clone(&store), child_id);
    child
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCreated {
                trigger_event_id: None,
            }),
            None,
        )
        .await
        .unwrap();
    child
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
            None,
        )
        .await
        .unwrap();
    child
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskResumed),
            None,
        )
        .await
        .unwrap();
    child
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCompleted {
                response: Some("子任务结果".into()),
            }),
            None,
        )
        .await
        .unwrap();

    let model = TaskDiscoveryModel {
        calls: AtomicUsize::new(0),
        contexts: Arc::new(Mutex::new(Vec::new())),
    };
    let mut tools = ToolRegistry::default();
    koi_core::agent::task_tools::register_task_management_tools(&mut tools).unwrap();
    let manager = TaskManager::new(Arc::new(Arc::clone(&store)));
    let resolver = EvidenceResolver {
        ingress_event_id: EventId::new(),
    };
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts)
        .with_task_manager(&manager);
    let mut main = TaskRuntime::new(Arc::clone(&store), TaskId::MAIN);
    let first_context_event_id = EventId::new();

    let outcome = agent
        .run_main(
            &mut main,
            AgentRunRequest {
                trigger_event_id: Some(first_context_event_id),
                context: vec![ModelContextItem {
                    event_id: first_context_event_id,
                    role: ModelInputRole::User,
                    content: "有哪些已有任务？".into(),
                    permission: PermissionLevel::None,
                }],
                input_events: Vec::new(),
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
            response: Some("已查看任务列表".into())
        }
    );
    let contexts = model.contexts.lock().await;
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[1].len(), 2);
    assert!(contexts[1][1].content.contains("[KOI_TOOL_DATA]"));
    assert!(contexts[1][1].content.contains(&child_id.to_string()));
}

struct DelegatedInputModel {
    input_event_id: EventId,
    calls: AtomicUsize,
    contexts: Arc<Mutex<Vec<Vec<ModelContextItem>>>>,
}

#[async_trait]
impl ModelProvider for DelegatedInputModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model_id: "delegated-input-model".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    async fn start(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.contexts.lock().await.push(request.context);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if call == 0 {
            ModelOutput::ToolCall(ToolCall {
                name: "server.status".into(),
                arguments: json!({"server": "prod-1"}),
                provider_call_id: Some("delegated-call-1".into()),
                authority_parent_event_id: Some(self.input_event_id),
            })
        } else {
            ModelOutput::Text {
                text: "委托输入已完成调查".into(),
            }
        };
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![output],
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

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn delegated_child_input_authorizes_tool_via_main_event_chain() {
    use koi_core::domain::{ContextKind, IngressEvent, ModelEvent, ToolEvent};

    let shared = Arc::new(MemoryEventStore::default());
    let manager = TaskManager::new(Arc::new(Arc::clone(&shared)));
    let mut main = TaskRuntime::new(Arc::clone(&shared), TaskId::MAIN);
    main.record(
        AgentEvent::control(koi_core::domain::ControlEvent::TaskCreated {
            trigger_event_id: None,
        }),
        None,
    )
    .await
    .unwrap();
    main.record(
        AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
        None,
    )
    .await
    .unwrap();
    let parent = main
        .record_with_provenance(
            external_message_event(PermissionLevel::User, "delegated-parent", "请调查部署失败"),
            None,
            external_provenance(PermissionLevel::User),
        )
        .await
        .unwrap();

    let mut created = manager.request_create_child(&mut main, None).await.unwrap();
    let child_id = created.task_id;
    created
        .runtime
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskCreated {
                trigger_event_id: Some(created.accepted_event_id),
            }),
            Some(created.accepted_event_id),
        )
        .await
        .unwrap();
    created
        .runtime
        .record(
            AgentEvent::control(koi_core::domain::ControlEvent::TaskQueued),
            None,
        )
        .await
        .unwrap();
    let input_event_id = manager
        .request_input_child(
            &mut main,
            child_id,
            "请根据用户输入调查部署失败，并只使用只读工具收集证据".into(),
            parent.id,
            PermissionLevel::User,
            None,
        )
        .await
        .unwrap();
    let child_input = shared
        .load_event(child_id, input_event_id)
        .await
        .unwrap()
        .expect("委托输入事件应存在");

    let contexts = Arc::new(Mutex::new(Vec::new()));
    let model = DelegatedInputModel {
        input_event_id,
        calls: AtomicUsize::new(0),
        contexts: Arc::clone(&contexts),
    };
    let tool = Arc::new(StatusTool::new());
    let mut tools = ToolRegistry::default();
    tools
        .register(Arc::clone(&tool) as Arc<dyn ToolExecutor>)
        .unwrap();
    let resolver = PersistedAuthorizationEvidenceResolver::new(shared.as_ref());
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);
    let mut child = TaskRuntime::recover(Arc::clone(&shared), child_id)
        .await
        .unwrap();
    let outcome = agent
        .run(
            &mut child,
            AgentRunRequest {
                trigger_event_id: Some(input_event_id),
                context: vec![],
                input_events: vec![child_input],
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
            response: Some("委托输入已完成调查".into())
        }
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    let model_contexts = contexts.lock().await;
    assert_eq!(model_contexts.len(), 2);
    assert_eq!(model_contexts[0].len(), 1);
    assert_eq!(model_contexts[0][0].event_id, input_event_id);
    assert_eq!(model_contexts[0][0].role, ModelInputRole::User);
    assert_eq!(model_contexts[0][0].permission, PermissionLevel::User);

    let events = shared.load_task(child_id).await.unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            AgentEvent::Model(model)
                if matches!(model.as_ref(), ModelEvent::Completed { .. })
        )
    }));
    let proposed = events
        .iter()
        .find_map(|event| match &event.payload {
            AgentEvent::Tool(tool) => match tool.as_ref() {
                ToolEvent::Proposed { tool_call } => Some(tool_call),
                _ => None,
            },
            _ => None,
        })
        .expect("应记录工具提议");
    assert_eq!(proposed.authority_parent_event_id, Some(input_event_id));
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            AgentEvent::Tool(tool)
                if matches!(
                    tool.as_ref(),
                    ToolEvent::AuthorizationChecked {
                        decision: koi_core::domain::PolicyDecision::Allow,
                        effective_permission: PermissionLevel::User,
                        ..
                    }
                )
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            AgentEvent::Ingress(ingress)
                if matches!(ingress.as_ref(), IngressEvent::ContextReceived { context, .. }
                    if context.kind == ContextKind::UserMessage)
        )
    }));
}

struct TextModel {
    calls: AtomicUsize,
}

#[async_trait]
impl ModelProvider for TextModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model_id: "test-model".into(),
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
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![ModelOutput::Text {
                    text: "收到".into(),
                }],
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

struct ContextCaptureModel {
    contexts: Arc<Mutex<Vec<Vec<ModelContextItem>>>>,
    calls: AtomicUsize,
    context_window_tokens: Option<u32>,
}

#[async_trait]
impl ModelProvider for ContextCaptureModel {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "test".into(),
            model_id: "context-capture".into(),
            protocol: ModelProtocol::Responses,
            capabilities: ModelCapabilities::default(),
        }
    }

    fn context_window_tokens(&self) -> Option<u32> {
        self.context_window_tokens
    }

    async fn start(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        self.contexts.lock().await.push(request.context);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Box::pin(stream::iter(vec![Ok(
            ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![ModelOutput::Text {
                    text: format!("第 {call} 轮回复"),
                }],
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

fn external_message_event(
    permission: PermissionLevel,
    native_event_id: &str,
    text: &str,
) -> AgentEvent {
    use koi_core::domain::{ContextEnvelope, ContextKind, ContextOrigin, ContextPayload};
    let now = chrono::Utc::now();
    AgentEvent::ingress(koi_core::domain::IngressEvent::ContextReceived {
        context: Box::new(ContextEnvelope {
            schema_version: 1,
            kind: ContextKind::UserMessage,
            origin: ContextOrigin {
                source: "qq".into(),
                source_instance: "group-42".into(),
                native_event_id: native_event_id.into(),
            },
            actor: Some(Principal::new("qq", "10001")),
            scope: koi_core::domain::Scope::new("qq_group", "42"),
            occurred_at: now,
            received_at: now,
            position: None,
            permission,
            payload: ContextPayload::Text {
                text: text.into(),
                mentions: vec!["bot".into()],
            },
            causation_id: None,
            content_hash: "test".into(),
        }),
        assessment: koi_core::domain::PermissionAssessment::new(permission, permission, permission),
    })
}

fn external_provenance(permission: PermissionLevel) -> koi_core::domain::EventProvenance {
    koi_core::domain::EventProvenance {
        creator: EventSource::External(SourceName::new("qq").unwrap()),
        direct_permission: Some(permission),
        authority_parent_event_id: None,
        expires_at: None,
    }
}

#[tokio::test]
async fn instruction_input_below_session_minimum_is_skipped_not_injected() {
    use koi_core::domain::ControlEvent;
    use koi_core::domain::ModelEvent;

    let model = TextModel {
        calls: AtomicUsize::new(0),
    };
    let tools = ToolRegistry::default();
    let store = MemoryEventStore::default();
    let events = Arc::clone(&store.events);
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    // 会话最低控制权限升到 Operator：User 输入不再满足指令门槛。
    runtime
        .record(
            AgentEvent::control(ControlEvent::MinimumControlPermissionChanged {
                minimum_permission: PermissionLevel::Operator,
            }),
            None,
        )
        .await
        .unwrap();
    let below_minimum = runtime
        .record_with_provenance(
            external_message_event(
                PermissionLevel::User,
                "message-below",
                "普通用户的低权限指令",
            ),
            None,
            external_provenance(PermissionLevel::User),
        )
        .await
        .unwrap();
    let above_minimum = runtime
        .record_with_provenance(
            external_message_event(PermissionLevel::Admin, "message-above", "管理员的合法指令"),
            None,
            external_provenance(PermissionLevel::Admin),
        )
        .await
        .unwrap();

    let resolver = EvidenceResolver {
        ingress_event_id: above_minimum.id,
    };
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);
    let outcome = agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(above_minimum.id),
                context: vec![],
                input_events: vec![below_minimum.clone(), above_minimum.clone()],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 2,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        outcome,
        AgentRunOutcome::Completed {
            response: Some("收到".into()),
        }
    );
    // 低权限输入被拒绝注入但不中断任务；高权限输入照常进入模型上下文。
    let call_started = events
        .lock()
        .await
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                AgentEvent::Model(ref model) if matches!(model.as_ref(), ModelEvent::CallStarted { .. })
            )
        })
        .unwrap()
        .clone();
    let AgentEvent::Model(model_event) = &call_started.payload else {
        unreachable!();
    };
    let ModelEvent::CallStarted {
        context_event_ids, ..
    } = model_event.as_ref()
    else {
        unreachable!();
    };
    assert!(!context_event_ids.contains(&below_minimum.id));
    assert!(context_event_ids.contains(&above_minimum.id));
}

#[tokio::test]
async fn oversized_history_is_compacted_and_persisted_before_model_call() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let model = ContextCaptureModel {
        contexts: Arc::clone(&contexts),
        calls: AtomicUsize::new(0),
        context_window_tokens: Some(5_000),
    };
    let store = MemoryEventStore::default();
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    let mut input_events = Vec::new();
    for index in 0..3 {
        input_events.push(
            runtime
                .record_with_provenance(
                    external_message_event(
                        PermissionLevel::User,
                        &format!("large-history-{index}"),
                        &"历史消息 ".repeat(1_200),
                    ),
                    None,
                    external_provenance(PermissionLevel::User),
                )
                .await
                .unwrap(),
        );
    }

    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let resolver = EvidenceResolver {
        ingress_event_id: input_events[0].id,
    };
    let tools = ToolRegistry::default();
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);
    agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(input_events[2].id),
                context: vec![],
                input_events,
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let events = runtime.load_events().await.unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            AgentEvent::Control(control)
                if matches!(control.as_ref(), koi_core::domain::ControlEvent::ContextCompacted {
                    summary, ..
                } if !summary.is_empty())
        )
    }));
    let captured = contexts.lock().await;
    assert!(
        captured[0]
            .iter()
            .any(|item| item.content.contains("KOI_CONTEXT_SUMMARY"))
    );
}

#[tokio::test]
async fn a_follow_up_model_call_receives_the_complete_persisted_history() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let model = ContextCaptureModel {
        contexts: Arc::clone(&contexts),
        calls: AtomicUsize::new(0),
        context_window_tokens: None,
    };
    let store = MemoryEventStore::default();
    let mut runtime = TaskRuntime::new(store, TaskId::MAIN);
    let providers = SourceAuthorizationRegistry::default();
    let prompts = TestPromptProvider;
    let resolver = EvidenceResolver {
        ingress_event_id: EventId::new(),
    };
    let tools = ToolRegistry::default();
    let agent = AgentLoop::new(&model, &tools, &resolver, &providers, None, &prompts);

    let first = runtime
        .record_with_provenance(
            external_message_event(PermissionLevel::User, "history-1", "第一次输入"),
            None,
            external_provenance(PermissionLevel::User),
        )
        .await
        .unwrap();
    agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(first.id),
                context: vec![],
                input_events: vec![first.clone()],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let second = runtime
        .record_with_provenance(
            external_message_event(PermissionLevel::User, "history-2", "第二次输入"),
            None,
            external_provenance(PermissionLevel::User),
        )
        .await
        .unwrap();
    agent
        .run_main(
            &mut runtime,
            AgentRunRequest {
                trigger_event_id: Some(second.id),
                context: vec![],
                input_events: vec![second.clone()],
                memory_query: None,
                output_contract: ModelOutputContract::Text,
                model_options: ModelGenerationOptions::default(),
                max_model_turns: 1,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let captured = contexts.lock().await;
    assert_eq!(captured.len(), 2);
    assert!(captured[1].iter().any(|item| item.event_id == first.id));
    assert!(captured[1].iter().any(|item| item.event_id == second.id));
    assert!(captured[1].iter().any(|item| item.content == "第 1 轮回复"));
}
