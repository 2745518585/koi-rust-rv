//! 独立模型代理入口。
//!
//! 该进程是唯一读取 `models.toml` 与 API Key 的组件。`koi-server` 启用代理后只会
//! 获取脱敏模型目录，并通过带令牌的私有 HTTP 协议转发规范化模型请求。

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use koi_infra::model_config::{
    DEFAULT_MODELS_CONFIG_PATH, ModelsConfig, build_direct_model_registry,
};
use koi_infra::model_proxy::router;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

const DEFAULT_PROXY_CONFIG_PATH: &str = "config/model-proxy.toml";

#[derive(Debug, Deserialize)]
struct ProxyRuntimeConfig {
    #[serde(default)]
    proxy: ProxyConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ProxyConfig {
    bind_addr: String,
    models_config_path: PathBuf,
    token: Option<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:9510".into(),
            models_config_path: PathBuf::from(DEFAULT_MODELS_CONFIG_PATH),
            token: None,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .init();

    if let Err(error) = run().await {
        tracing::error!(%error, "koi-model-proxy 启动失败");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config_path = config_path_from_args()?;
    let config = load_config(&config_path)?;
    let token = config
        .proxy
        .token
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("KOI_MODEL_PROXY_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .ok_or_else(|| {
            "模型代理必须配置 proxy.token 或 KOI_MODEL_PROXY_TOKEN，拒绝以无认证模式启动".to_owned()
        })?;
    let models =
        ModelsConfig::load(&config.proxy.models_config_path).map_err(|error| error.to_string())?;
    let registry = build_direct_model_registry(&models).map_err(|error| error.to_string())?;
    let listener = tokio::net::TcpListener::bind(&config.proxy.bind_addr)
        .await
        .map_err(|error| format!("监听 {} 失败：{error}", config.proxy.bind_addr))?;
    tracing::info!(
        bind_addr = %config.proxy.bind_addr,
        model_count = registry.len(),
        "koi-model-proxy 已启动"
    );
    axum::serve(listener, router(Arc::clone(&registry), token))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("模型代理 HTTP 服务异常结束：{error}"))
}

fn config_path_from_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => Ok(PathBuf::from(DEFAULT_PROXY_CONFIG_PATH)),
        Some("--config") => args
            .next()
            .map(PathBuf::from)
            .ok_or_else(|| "--config 需要提供配置文件路径".to_owned()),
        Some(other) => Err(format!("不支持的参数：{other}；可使用 --config <路径>")),
    }
}

fn load_config(path: &PathBuf) -> Result<ProxyRuntimeConfig, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("读取模型代理配置 {} 失败：{error}", path.display()))?;
    toml::from_str(&contents)
        .map_err(|error| format!("模型代理配置 {} 解析失败：{error}", path.display()))
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "无法监听关闭信号");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_config_defaults_to_isolated_models_file() {
        let config: ProxyRuntimeConfig =
            toml::from_str("[proxy]\nbind_addr = \"127.0.0.1:1\"").unwrap();
        assert_eq!(
            config.proxy.models_config_path,
            PathBuf::from(DEFAULT_MODELS_CONFIG_PATH)
        );
    }
}
