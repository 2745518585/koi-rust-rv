use koi_core::domain::{
    AuthorizedToolInvocation, EventId, PermissionLevel, TaskId, ToolCall, ToolErrorKind,
    ToolSideEffect,
};
use koi_core::ports::{ToolInvocationError, ToolRegistry};
use koi_infra::tools::{ToolPolicy, register_builtin_tools};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn invocation(name: &str, arguments: serde_json::Value) -> AuthorizedToolInvocation {
    AuthorizedToolInvocation {
        task_id: TaskId::new(),
        proposal_event_id: EventId::new(),
        execution_started_event_id: EventId::new(),
        tool_call: ToolCall {
            name: name.into(),
            arguments,
            provider_call_id: None,
            authority_parent_event_id: None,
        },
        authorization_evidence_event_ids: vec![EventId::new()],
    }
}

#[test]
fn registers_the_builtin_tool_catalog_with_expected_risk_levels() {
    let mut registry = ToolRegistry::default();
    let count = register_builtin_tools(&mut registry, ToolPolicy::default()).unwrap();

    assert_eq!(count, 94);
    assert_eq!(registry.list_definitions().len(), count);
    let command = registry.get_definition("system.command").unwrap();
    assert_eq!(command.required_permission, PermissionLevel::Admin);
    assert_eq!(command.side_effect, ToolSideEffect::Destructive);
    assert!(!command.model_visible);

    let write = registry.get_definition("fs.write").unwrap();
    assert_eq!(write.required_permission, PermissionLevel::Operator);
    let read = registry.get_definition("fs.read").unwrap();
    assert_eq!(read.required_permission, PermissionLevel::User);
    let compose = registry.get_definition("docker.compose_up").unwrap();
    assert_eq!(compose.required_permission, PermissionLevel::Admin);
    assert!(!compose.model_visible);
    let build = registry.get_definition("docker.build").unwrap();
    assert_eq!(build.required_permission, PermissionLevel::Admin);
    assert!(!build.model_visible);
}

#[tokio::test]
async fn filesystem_tools_are_scoped_and_support_write_read_delete() {
    let root = std::env::temp_dir().join(format!("koi-tools-{}", EventId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("status.txt");
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry, ToolPolicy::development(&root)).unwrap();

    registry
        .invoke(
            invocation("fs.write", json!({"path": file, "content": "healthy\n"})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let result = registry
        .invoke(
            invocation("fs.read", json!({"path": file})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.data["content"], "healthy\n");

    registry
        .invoke(
            invocation("fs.delete", json!({"path": file})),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!file.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn disabled_mutations_and_admin_commands_fail_closed() {
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry, ToolPolicy::default()).unwrap();

    let write_error = registry
        .invoke(
            invocation("fs.write", json!({"path":"missing.txt","content":"x"})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        write_error,
        ToolInvocationError::ExecutionFailed(error)
            if error.kind == ToolErrorKind::ExecutionFailed
    ));

    let command_error = registry
        .invoke(
            invocation(
                "system.command",
                json!({"program":"rustc","args":["--version"]}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        command_error,
        ToolInvocationError::ExecutionFailed(error)
            if error.kind == ToolErrorKind::ExecutionFailed
    ));
}

#[tokio::test]
async fn admin_command_uses_structured_arguments() {
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry, ToolPolicy::development(std::env::temp_dir())).unwrap();

    let result = registry
        .invoke(
            invocation(
                "system.command",
                json!({"program":"rustc","args":["--version"]}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.data["exit_code"], 0);
    assert!(result.data["stdout"].as_str().unwrap().contains("rustc"));
}
