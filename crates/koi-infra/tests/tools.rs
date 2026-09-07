use std::path::PathBuf;
use std::sync::Arc;

use koi_core::agent::TaskManager;
use koi_core::domain::{
    AuthorizedToolInvocation, EventId, PermissionLevel, TaskId, ToolCall, ToolSideEffect,
};
use koi_core::ports::{StaticPermissionDirectory, ToolRegistry};
use koi_infra::event_store::JsonlEventStore;
use koi_infra::qq_source::{QqConfig, QqSource};
use koi_infra::tools::{register_builtin_tools, register_qq_tools};
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
    let count = register_builtin_tools(&mut registry).unwrap();

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
async fn filesystem_tools_support_write_read_delete() {
    let root = std::env::temp_dir().join(format!("koi-tools-{}", EventId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("status.txt");
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry).unwrap();

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
async fn mutating_and_admin_tools_run_after_core_authorization() {
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry).unwrap();
    let command = registry
        .invoke(
            invocation(
                "system.command",
                json!({"program":"rustc","args":["--version"]}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(command.data["exit_code"], 0);
}

#[tokio::test]
async fn admin_command_uses_structured_arguments() {
    let mut registry = ToolRegistry::default();
    register_builtin_tools(&mut registry).unwrap();

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

fn test_qq_source() -> (Arc<QqSource>, PathBuf) {
    let directory = std::env::temp_dir().join(format!("koi-qq-tool-{}", EventId::new()));
    let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
    let permissions = Arc::new(StaticPermissionDirectory::new(
        [("qq".to_owned(), PermissionLevel::User)],
        std::iter::empty(),
    ));
    let task_manager = Arc::new(TaskManager::new(Arc::new(Arc::clone(&store))));
    let source = Arc::new(
        QqSource::new(
            QqConfig {
                app_id: Some("app-id".into()),
                app_secret: Some("app-secret".into()),
                ..QqConfig::default()
            },
            store,
            permissions,
            task_manager,
        )
        .unwrap(),
    );
    (source, directory)
}

fn test_qq_source_with_report_group() -> (Arc<QqSource>, PathBuf) {
    let directory = std::env::temp_dir().join(format!("koi-qq-report-tool-{}", EventId::new()));
    let store = Arc::new(JsonlEventStore::open(&directory).unwrap());
    let permissions = Arc::new(StaticPermissionDirectory::new(
        [("qq".to_owned(), PermissionLevel::User)],
        std::iter::empty(),
    ));
    let task_manager = Arc::new(TaskManager::new(Arc::new(Arc::clone(&store))));
    let source = Arc::new(
        QqSource::new(
            QqConfig {
                app_id: Some("app-id".into()),
                app_secret: Some("app-secret".into()),
                report_group_openid: Some("report-group".into()),
                ..QqConfig::default()
            },
            store,
            permissions,
            task_manager,
        )
        .unwrap(),
    );
    (source, directory)
}

#[test]
fn registers_qq_group_delivery_tool_with_notification_metadata() {
    let (source, directory) = test_qq_source();
    let mut registry = ToolRegistry::default();
    assert_eq!(register_qq_tools(&mut registry, source).unwrap(), 2);

    let definition = registry.get_definition("qq.group_send").unwrap();
    assert_eq!(definition.required_permission, PermissionLevel::Operator);
    assert_eq!(definition.side_effect, ToolSideEffect::Notification);
    assert!(definition.model_visible);
    assert_eq!(
        definition.input_schema["required"],
        json!(["group_openid", "content"])
    );

    let reply = registry.get_definition("qq.reply").unwrap();
    assert_eq!(reply.required_permission, PermissionLevel::User);
    assert_eq!(reply.side_effect, ToolSideEffect::Notification);
    assert!(reply.model_visible);
    assert_eq!(reply.input_schema["required"], json!(["content"]));
    assert!(
        reply.input_schema["properties"]
            .get("group_openid")
            .is_none()
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn registers_primary_qq_report_tool_only_when_configured() {
    let (source, directory) = test_qq_source_with_report_group();
    let mut registry = ToolRegistry::default();
    assert_eq!(register_qq_tools(&mut registry, source).unwrap(), 3);

    let report = registry.get_definition("qq.report").unwrap();
    assert_eq!(report.required_permission, PermissionLevel::Operator);
    assert_eq!(report.side_effect, ToolSideEffect::Notification);
    assert!(report.model_visible);
    assert_eq!(report.input_schema["required"], json!(["content"]));
    assert!(
        report.input_schema["properties"]
            .get("group_openid")
            .is_none()
    );

    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn qq_group_delivery_rejects_invalid_arguments_before_network() {
    let (source, directory) = test_qq_source();
    let mut registry = ToolRegistry::default();
    register_qq_tools(&mut registry, source).unwrap();

    let error = registry
        .invoke(
            invocation(
                "qq.group_send",
                json!({"group_openid": "", "content": "hello"}),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("group_openid"));

    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn qq_group_delivery_honors_cancellation_before_network() {
    let (source, directory) = test_qq_source();
    let mut registry = ToolRegistry::default();
    register_qq_tools(&mut registry, source).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = registry
        .invoke(
            invocation(
                "qq.group_send",
                json!({"group_openid": "group-1", "content": "hello"}),
            ),
            cancel,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));

    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn qq_reply_requires_a_context_authority_parent_event() {
    let (source, directory) = test_qq_source();
    let mut registry = ToolRegistry::default();
    register_qq_tools(&mut registry, source).unwrap();

    let error = registry
        .invoke(
            invocation("qq.reply", json!({"content": "hello"})),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("authority-parent"));

    std::fs::remove_dir_all(directory).unwrap();
}
