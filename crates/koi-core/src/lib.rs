//! koi-rust-rv 的纯领域模型与应用逻辑。
//!
//! 核心将任务活动记录为仅追加事件；传输、存储和具体模型供应商实现均位于此 crate 之外。

pub mod agent;
pub mod domain;
pub mod ports;

pub const APP_NAME: &str = "koi-rust-rv";
