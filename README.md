# koi-rust-rv

Koi Rust Remastered Version：一个面向小型服务器运维群的 QQ 协作式故障初诊 Agent。

当前仓库提供可编译的 Rust workspace、运行配置模板、人格模板、环境变量模板和数据库迁移约定。业务模块将在后续迭代中实现。

## Quick start

1. Copy `.env.example` to `.env` and fill in secrets.
2. Copy `config/agent.example.toml` to `config/agent.toml`.
3. Copy `config/persona.example.toml` to `config/persona.toml`.
4. Run `cargo check-all`.

`config/agent.toml` and `config/persona.toml` are local runtime files and should not contain secrets. API keys and QQ credentials remain in `.env`.
