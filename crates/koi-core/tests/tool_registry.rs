use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizedToolInvocation, EventId, PermissionLevel, TaskId, ToolCall, ToolDefinition,
    ToolError, ToolResult, ToolSideEffect,
};
use koi_core::ports::{ToolExecutor, ToolRegistry};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct EchoTool {
    definition: ToolDefinition,
    calls: AtomicUsize,
}

impl EchoTool {
    fn new() -> Self {
        Self {
            definition: ToolDefinition {
                name: "server.status".into(),
                description: "查询服务器状态".into(),
                input_schema: json!({"type": "object"}),
                required_permission: PermissionLevel::User,
                side_effect: ToolSideEffect::ReadOnly,
                timeout_ms: 5_000,
                model_visible: true,
                main_session_only: false,            },
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ToolExecutor for EchoTool {
    fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    async fn execute(
        &self,
        invocation: AuthorizedToolInvocation,
        _cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult {
            summary: format!("已执行 {}", invocation.tool_call.name),
            data: invocation.tool_call.arguments,
            truncated: false,
        })
    }
}

fn authorized_invocation() -> AuthorizedToolInvocation {
    AuthorizedToolInvocation {
        task_id: TaskId::new(),
        proposal_event_id: EventId::new(),
        execution_started_event_id: EventId::new(),
        tool_call: ToolCall {
            name: "server.status".into(),
            arguments: json!({"server": "demo-1"}),
            provider_call_id: Some("call-1".into()),
            authority_parent_event_id: Some(EventId::new()),
        },
        authorization_evidence_event_ids: vec![EventId::new()],
    }
}

#[tokio::test]
async fn registry_registers_queries_and_invokes_authorized_tools() {
    let tool = Arc::new(EchoTool::new());
    let mut registry = ToolRegistry::default();
    registry
        .register(Arc::clone(&tool) as Arc<dyn ToolExecutor>)
        .unwrap();

    let definition = registry.get_definition("server.status").unwrap();
    assert_eq!(definition.required_permission, PermissionLevel::User);
    assert_eq!(registry.list_definitions().len(), 1);

    let result = registry
        .invoke(authorized_invocation(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.summary, "已执行 server.status");
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn registry_rejects_duplicate_tool_names() {
    let mut registry = ToolRegistry::default();
    registry
        .register(Arc::new(EchoTool::new()) as Arc<dyn ToolExecutor>)
        .unwrap();

    assert!(
        registry
            .register(Arc::new(EchoTool::new()) as Arc<dyn ToolExecutor>)
            .is_err()
    );
}
