//! 模型目录与凭据配置。
//!
//! 该模块刻意不依赖 Web、QQ 或任务运行时：直接运行模式由 `koi-server` 调用，
//! 独立模型代理模式则由 `koi-model-proxy` 调用同一份配置。公开目录不会包含 API Key。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use koi_core::agent::DEFAULT_CONTEXT_WINDOW_TOKENS;
use koi_core::domain::{ModelGenerationOptions, ModelProtocol, ModelSelection};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::llm::{
    ModelProviderEntry, ModelProviderRegistry, OpenAiCompatibleModelConfig,
    OpenAiCompatibleModelProvider,
};

/// 从主运行配置中移出的模型配置文件默认位置。
pub const DEFAULT_MODELS_CONFIG_PATH: &str = "config/models.toml";

/// 包含供应商连接信息和生成选项的模型配置。仅在直接模式或模型代理中读取。
#[derive(Clone, Debug, Deserialize)]
pub struct ModelsConfig {
    pub default_provider: String,
    pub default_model_id: String,
    pub entries: Vec<ModelConfig>,
}

/// 单个 OpenAI 兼容模型的部署配置。
#[derive(Clone, Debug, Deserialize)]
pub struct ModelConfig {
    pub provider: String,
    pub base_url: String,
    pub model_id: String,
    /// 兼容旧配置的内联密钥。生产部署更建议改用 `api_key_file`。
    #[serde(default)]
    pub api_key: Option<String>,
    /// 只包含 API Key 的文件路径；读取后不会进入公开模型目录或日志。
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    pub protocol: String,
    pub request_timeout_secs: u64,
    /// 模型上下文窗口上限；兼容旧配置时可由 `max_context_messages` 推导。
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
    /// 旧版按消息数量限制上下文的配置，仅用于迁移，不再直接控制上下文。
    #[serde(default)]
    pub max_context_messages: Option<usize>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub reasoning_summary: Option<String>,
}

/// 不含密钥、可通过模型代理发送给 Agent 的模型目录。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelCatalog {
    pub default_model: ModelSelection,
    pub entries: Vec<ModelCatalogEntry>,
}

/// 一项供 Agent 选择模型和构建请求的公开元数据。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelCatalogEntry {
    pub selection: ModelSelection,
    pub protocol: ModelProtocol,
    pub model_options: ModelGenerationOptions,
    pub context_window_tokens: u32,
}

#[derive(Debug, Error)]
pub enum ModelConfigurationError {
    #[error("读取模型配置 {path} 失败：{source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("模型配置 {path} 解析失败：{source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("模型配置无效：{0}")]
    Invalid(String),
    #[error("模型 Provider 初始化失败：{0}")]
    Provider(String),
}

impl ModelsConfig {
    /// 从独立 TOML 文件加载模型配置。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ModelConfigurationError> {
        let path = path.as_ref().to_path_buf();
        let contents =
            fs::read_to_string(&path).map_err(|source| ModelConfigurationError::Read {
                path: path.clone(),
                source,
            })?;
        toml::from_str(&contents).map_err(|source| ModelConfigurationError::Parse { path, source })
    }

    /// 构造不含 API Key 的目录，供独立模型代理向 Agent 宣告可选模型。
    pub fn catalog(&self) -> Result<ModelCatalog, ModelConfigurationError> {
        if self.entries.is_empty() {
            return Err(ModelConfigurationError::Invalid(
                "至少需要配置一个模型条目".into(),
            ));
        }
        let default_model =
            ModelSelection::new(self.default_provider.clone(), self.default_model_id.clone())
                .map_err(|error| {
                    ModelConfigurationError::Invalid(format!("默认模型无效：{error}"))
                })?;
        let entries = self
            .entries
            .iter()
            .map(ModelConfig::catalog_entry)
            .collect::<Result<Vec<_>, _>>()?;
        if !entries.iter().any(|entry| entry.selection == default_model) {
            return Err(ModelConfigurationError::Invalid(format!(
                "默认模型 {default_model} 未出现在 entries 中"
            )));
        }
        Ok(ModelCatalog {
            default_model,
            entries,
        })
    }
}

impl ModelConfig {
    fn catalog_entry(&self) -> Result<ModelCatalogEntry, ModelConfigurationError> {
        let selection = ModelSelection::new(self.provider.clone(), self.model_id.clone())
            .map_err(|error| ModelConfigurationError::Invalid(format!("模型条目无效：{error}")))?;
        Ok(ModelCatalogEntry {
            selection,
            protocol: parse_model_protocol(&self.protocol)?,
            model_options: self.model_options(),
            context_window_tokens: self.context_window_tokens()?,
        })
    }

    fn model_options(&self) -> ModelGenerationOptions {
        ModelGenerationOptions {
            max_output_tokens: self.max_output_tokens,
            reasoning_effort: self
                .reasoning_effort
                .clone()
                .filter(|effort| !effort.trim().is_empty()),
            reasoning_summary: self
                .reasoning_summary
                .clone()
                .filter(|summary| !summary.trim().is_empty()),
            ..ModelGenerationOptions::default()
        }
    }

    fn context_window_tokens(&self) -> Result<u32, ModelConfigurationError> {
        let configured = self.context_window_tokens.or_else(|| {
            self.max_context_messages
                .and_then(|messages| u32::try_from(messages).ok())
                .map(|messages| messages.saturating_mul(1024))
        });
        let tokens = configured.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        if tokens == 0 {
            return Err(ModelConfigurationError::Invalid(format!(
                "模型 {}/{} 的 context_window_tokens 必须大于零",
                self.provider, self.model_id
            )));
        }
        Ok(tokens)
    }

    fn resolved_api_key(&self) -> Result<Option<String>, ModelConfigurationError> {
        let inline = self
            .api_key
            .as_deref()
            .filter(|value| !value.trim().is_empty());
        if inline.is_some() && self.api_key_file.is_some() {
            return Err(ModelConfigurationError::Invalid(format!(
                "模型 {}/{} 不能同时设置 api_key 与 api_key_file",
                self.provider, self.model_id
            )));
        }
        if let Some(path) = &self.api_key_file {
            let key = fs::read_to_string(path).map_err(|source| ModelConfigurationError::Read {
                path: path.clone(),
                source,
            })?;
            let key = key.trim().to_owned();
            if key.is_empty() {
                return Err(ModelConfigurationError::Invalid(format!(
                    "模型 {}/{} 的 api_key_file 为空",
                    self.provider, self.model_id
                )));
            }
            return Ok(Some(key));
        }
        Ok(inline.map(str::to_owned))
    }
}

/// 按当前进程直接持有的模型配置创建 Provider 注册表。
///
/// 在独立代理模式中，应改用 `model_proxy::build_proxy_model_registry`，使本进程
/// 根本不读取模型配置文件和 API Key。
pub fn build_direct_model_registry(
    config: &ModelsConfig,
) -> Result<Arc<ModelProviderRegistry>, ModelConfigurationError> {
    let catalog = config.catalog()?;
    let mut registry = ModelProviderRegistry::new(catalog.default_model)
        .map_err(|error| ModelConfigurationError::Invalid(error.to_string()))?;
    for model in &config.entries {
        let entry = model.catalog_entry()?;
        let api_key = model.resolved_api_key()?;
        let provider_config = OpenAiCompatibleModelConfig::new(
            model.provider.clone(),
            model.base_url.clone(),
            model.model_id.clone(),
            api_key,
        )
        .with_protocol(entry.protocol)
        .with_request_timeout_secs(model.request_timeout_secs)
        .with_context_window_tokens(entry.context_window_tokens);
        let provider = Arc::new(OpenAiCompatibleModelProvider::new(provider_config).map_err(
            |error| ModelConfigurationError::Provider(format!("{}：{error}", entry.selection)),
        )?);
        registry
            .register(
                entry.selection,
                ModelProviderEntry::new(provider, entry.model_options, entry.context_window_tokens),
            )
            .map_err(|error| ModelConfigurationError::Invalid(error.to_string()))?;
    }
    registry
        .resolve(None)
        .map_err(|error| ModelConfigurationError::Invalid(error.to_string()))?;
    Ok(Arc::new(registry))
}

fn parse_model_protocol(raw: &str) -> Result<ModelProtocol, ModelConfigurationError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "responses" => Ok(ModelProtocol::Responses),
        "chat_completions" | "chat-completions" | "chat" => Ok(ModelProtocol::ChatCompletions),
        other => Err(ModelConfigurationError::Invalid(format!(
            "不支持的模型协议：{other}，可选 responses 或 chat_completions"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_excludes_credentials_and_keeps_generation_options() {
        let config: ModelsConfig = toml::from_str(
            r#"
                default_provider = "example"
                default_model_id = "example-model"

                [[entries]]
                provider = "example"
                base_url = "https://example.invalid/v1"
                model_id = "example-model"
                api_key = "secret"
                protocol = "responses"
                request_timeout_secs = 60
                context_window_tokens = 8192
                max_output_tokens = 1024
                reasoning_effort = "low"
            "#,
        )
        .unwrap();

        let catalog = config.catalog().unwrap();
        let encoded = serde_json::to_string(&catalog).unwrap();
        assert!(!encoded.contains("secret"));
        assert_eq!(
            catalog.entries[0].model_options.max_output_tokens,
            Some(1024)
        );
        assert_eq!(catalog.entries[0].context_window_tokens, 8192);
    }

    #[test]
    fn rejects_two_credential_sources() {
        let config: ModelsConfig = toml::from_str(
            r#"
                default_provider = "example"
                default_model_id = "example-model"

                [[entries]]
                provider = "example"
                base_url = "https://example.invalid/v1"
                model_id = "example-model"
                api_key = "secret"
                api_key_file = "key.txt"
                protocol = "responses"
                request_timeout_secs = 60
            "#,
        )
        .unwrap();

        let result = build_direct_model_registry(&config);
        assert!(matches!(result, Err(error) if error.to_string().contains("不能同时设置")));
    }
}
