use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use koi_core::domain::{
    EventId, ModelCapabilities, ModelCapability, ModelContextItem, ModelDeltaKind, ModelError,
    ModelGenerationOptions, ModelInputRole, ModelOutput, ModelOutputContract, ModelProtocol,
    ModelProviderDescriptor, ModelRequest, ModelStreamEvent, ModelToolDefinition, PermissionLevel,
    TaskId, Usage,
};
use koi_core::ports::{ModelEventStream, ModelProvider};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct FakeProvider;

#[async_trait]
impl ModelProvider for FakeProvider {
    fn descriptor(&self) -> ModelProviderDescriptor {
        ModelProviderDescriptor {
            provider: "fake".into(),
            model_id: "fake-model".into(),
            protocol: ModelProtocol::ChatCompletions,
            capabilities: ModelCapabilities::new([ModelCapability::Streaming]),
        }
    }

    async fn start(
        &self,
        request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelEventStream, ModelError> {
        request.validate().map_err(|error| {
            ModelError::new(
                koi_core::domain::ModelErrorKind::Internal,
                error.to_string(),
                false,
            )
        })?;

        Ok(Box::pin(stream::iter(vec![
            Ok(ModelStreamEvent::Delta {
                sequence: 0,
                kind: ModelDeltaKind::Text,
                content: "正在分析".into(),
            }),
            Ok(ModelStreamEvent::Completed(koi_core::domain::ModelTurn {
                outputs: vec![ModelOutput::Text {
                    text: "服务正常".into(),
                }],
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    cached_input_tokens: None,
                    reasoning_tokens: None,
                },
                provider_response_id: Some("provider-response-1".into()),
            })),
        ])))
    }
}

fn valid_request() -> ModelRequest {
    ModelRequest {
        task_id: TaskId::new(),
        instructions: "你是运维 Agent。".into(),
        instructions_hash: "instructions-v1".into(),
        context: vec![ModelContextItem {
            event_id: EventId::new(),
            role: ModelInputRole::User,
            content: "检查服务状态".into(),
            permission: PermissionLevel::User,
        }],
        tools: vec![ModelToolDefinition {
            name: "service_status".into(),
            description: "查询服务状态".into(),
            input_schema: json!({"type": "object"}),
            strict: true,
        }],
        output_contract: ModelOutputContract::Text,
        options: ModelGenerationOptions::default(),
    }
}

#[tokio::test]
async fn provider_stream_uses_a_vendor_neutral_contract() {
    let provider = FakeProvider;
    let descriptor = provider.descriptor();
    assert_eq!(descriptor.protocol, ModelProtocol::ChatCompletions);
    assert!(
        !descriptor
            .capabilities
            .supports(ModelCapability::NativeToolCalls)
    );

    let events = provider
        .start(valid_request(), CancellationToken::new())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;

    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], Ok(ModelStreamEvent::Delta { .. })));
    assert!(matches!(events[1], Ok(ModelStreamEvent::Completed(_))));
}

#[test]
fn request_validation_rejects_duplicate_context_events() {
    let mut request = valid_request();
    request.context.push(request.context[0].clone());

    assert!(request.validate().is_err());
}

#[test]
fn permission_level_keeps_reference_data_out_of_authorization() {
    assert!(!PermissionLevel::None.can_authorize());
    assert!(PermissionLevel::User.can_authorize());
    assert!(PermissionLevel::Admin.allows(PermissionLevel::Operator));
    assert!(!PermissionLevel::User.allows(PermissionLevel::Operator));
}
