use std::fs;

use koi_infra::tools::ToolPolicy;
use serde::Deserialize;

mod prompts;

#[derive(Debug, Default, Deserialize)]
struct RuntimeConfig {
    #[serde(default)]
    security: ToolPolicy,
}

fn main() {
    tracing_subscriber::fmt::init();
    let policy = load_tool_policy();
    let prompts = prompts::ServerPromptProvider;
    let mut tools = koi_core::ports::ToolRegistry::default();
    let registered =
        koi_infra::tools::register_builtin_tools(&mut tools, policy).expect("内置工具定义必须有效");
    tracing::info!(
        app = koi_core::APP_NAME,
        tool_count = registered,
        "koi-rust-rv 内置工具已注册"
    );
    let _ = (
        koi_api::CRATE_NAME,
        koi_infra::CRATE_NAME,
        tools.list_definitions(),
        prompts,
    );
}

fn load_tool_policy() -> ToolPolicy {
    let path = std::env::var("KOI_CONFIG_PATH").unwrap_or_else(|_| "config/agent.toml".into());
    let Ok(contents) = fs::read_to_string(&path) else {
        tracing::warn!(path, "未找到运行配置，使用默认的 fail-closed 工具策略");
        return ToolPolicy::default();
    };
    match toml::from_str::<RuntimeConfig>(&contents) {
        Ok(config) => config.security,
        Err(error) => {
            tracing::error!(path, %error, "运行配置解析失败，使用默认的 fail-closed 工具策略");
            ToolPolicy::default()
        }
    }
}
