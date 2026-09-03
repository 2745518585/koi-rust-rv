use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use koi_core::domain::{
    EventId, MemoryContextBuilder, MemoryKind, MemoryOrigin, MemoryQuery, MemoryRecord,
    MemorySearchResult, ModelInputRole, PermissionLevel, Scope,
};

fn memory_record(
    created_at: chrono::DateTime<Utc>,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> MemoryRecord {
    let source_event_id = EventId::new();
    MemoryRecord {
        id: EventId::new(),
        schema_version: 1,
        kind: MemoryKind::ServiceFact,
        scopes: vec![Scope::new("server", "prod-1")],
        content: "deploy.service 部署在 prod-1".into(),
        metadata: BTreeMap::default(),
        origin: MemoryOrigin::VerifiedToolResult {
            event_id: source_event_id,
        },
        source_event_ids: vec![source_event_id],
        created_at,
        expires_at,
    }
}

#[test]
fn memory_context_is_reference_only_and_respects_budget() {
    let now = Utc::now();
    let active = memory_record(now - Duration::minutes(2), None);
    let expired = memory_record(now - Duration::hours(2), Some(now - Duration::minutes(1)));
    let query = MemoryQuery {
        scopes: vec![Scope::new("server", "prod-1")],
        text: "部署服务在哪里".into(),
        kinds: vec![MemoryKind::ServiceFact],
        limit: 3,
        token_budget: 20,
        now,
    };
    let mut out_of_scope = memory_record(now, None);
    out_of_scope.scopes = vec![Scope::new("server", "staging-1")];

    let context = MemoryContextBuilder::build(
        &query,
        vec![
            MemorySearchResult {
                record: out_of_scope,
                relevance_score: 1.1,
                estimated_tokens: 1,
            },
            MemorySearchResult {
                record: expired,
                relevance_score: 1.0,
                estimated_tokens: 4,
            },
            MemorySearchResult {
                record: active.clone(),
                relevance_score: 0.9,
                estimated_tokens: 8,
            },
            MemorySearchResult {
                record: memory_record(now, None),
                relevance_score: 0.8,
                estimated_tokens: 30,
            },
        ],
    );

    assert_eq!(context.len(), 1);
    assert_eq!(context[0].event_id, active.id);
    assert_eq!(context[0].role, ModelInputRole::Memory);
    assert_eq!(context[0].permission, PermissionLevel::None);
}
