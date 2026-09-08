//! Infrastructure adapters: QQ, database, model providers, and ops tools.

pub mod alerts;
pub mod event_store;
pub mod llm;
pub mod qq_source;
pub mod service_monitor;
pub mod tools;
pub mod web_identity;
pub mod web_source;

pub const CRATE_NAME: &str = "koi-infra";
