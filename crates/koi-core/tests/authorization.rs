use std::sync::Arc;

use async_trait::async_trait;
use koi_core::domain::{
    AuthorizationEvidence, AuthorizationEvidenceEventKind, AuthorizationEvidenceStatus,
    AuthorizationRequest, AuthorizationRequestResult, EventId, EventSource, PermissionCheckResult,
    PermissionChecker, PermissionLevel, Principal, SourceName, TaskId, ToolDefinition,
    ToolSideEffect,
};
use koi_core::ports::{
    AuthorizationError, SourceAuthorizationProvider, SourceAuthorizationRegistry,
};
use serde_json::json;

fn restart_definition() -> ToolDefinition {
    ToolDefinition {
        name: "service.restart".into(),
        description: "重启服务".into(),
        input_schema: json!({"type": "object"}),
        required_permission: PermissionLevel::Operator,
        side_effect: ToolSideEffect::Stateful,
        timeout_ms: 10_000,
        model_visible: true,
                main_session_only: false,    }
}

fn evidence(
    permission: PermissionLevel,
    status: AuthorizationEvidenceStatus,
) -> AuthorizationEvidence {
    AuthorizationEvidence {
        event_id: EventId::new(),
        source: EventSource::External(SourceName::new("qq").unwrap()),
        event_kind: AuthorizationEvidenceEventKind::Ingress,
        principal: Some(Principal::new("qq", "10001")),
        source_maximum_permission: permission,
        permission,
        status,
        authority_parent_event_id: None,
        expires_at: None,
        approval_request_event_id: None,
    }
}

#[test]
fn control_event_cannot_be_an_authority_parent() {
    assert!(!AuthorizationEvidenceEventKind::Control.can_be_authority_parent());
    assert!(AuthorizationEvidenceEventKind::Ingress.can_be_authority_parent());
}

#[test]
fn permission_checker_ignores_none_expired_and_revoked_evidence() {
    let active_operator = evidence(
        PermissionLevel::Operator,
        AuthorizationEvidenceStatus::Active,
    );
    let expired_admin = evidence(PermissionLevel::Admin, AuthorizationEvidenceStatus::Expired);
    let reference_only = evidence(PermissionLevel::None, AuthorizationEvidenceStatus::Active);

    let result = PermissionChecker::check(
        &restart_definition(),
        &[reference_only, expired_admin, active_operator.clone()],
    );

    assert_eq!(
        result,
        PermissionCheckResult::Allowed {
            effective_permission: PermissionLevel::Operator,
            evidence_event_ids: vec![active_operator.event_id],
        }
    );
}

struct WebAuthorizationProvider;

#[async_trait]
impl SourceAuthorizationProvider for WebAuthorizationProvider {
    fn source(&self) -> &'static str {
        "web"
    }

    async fn request_authorization(
        &self,
        _request: AuthorizationRequest,
    ) -> Result<AuthorizationRequestResult, AuthorizationError> {
        Ok(AuthorizationRequestResult::Pending)
    }
}

#[tokio::test]
async fn source_registry_routes_elevation_requests_by_source() {
    let mut registry = SourceAuthorizationRegistry::default();
    registry
        .register(Arc::new(WebAuthorizationProvider) as Arc<dyn SourceAuthorizationProvider>)
        .unwrap();

    let result = registry
        .get("web")
        .unwrap()
        .request_authorization(AuthorizationRequest {
            task_id: TaskId::new(),
            approval_request_event_id: EventId::new(),
            tool_proposal_event_id: EventId::new(),
            tool_name: "service.restart".into(),
            arguments_hash: "sha256:example".into(),
            required_permission: PermissionLevel::Operator,
            original_evidence_event_ids: vec![EventId::new()],
        })
        .await
        .unwrap();

    assert_eq!(result, AuthorizationRequestResult::Pending);
    assert!(registry.get("qq").is_none());
}
