//! Infrastructure adapters: QQ, database, model providers, and ops tools.

pub mod event_store;
pub mod llm;
pub mod tools;
pub mod web_identity;
pub mod web_source;

pub const CRATE_NAME: &str = "koi-infra";
