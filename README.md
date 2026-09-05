# koi-rust-rv

Koi Rust Remastered Version：一个面向小型服务器运维群的 QQ 协作式故障初诊 Agent。

当前仓库提供可编译的 Rust workspace、运行配置模板、人格模板、数据库迁移约定和一组带权限边界的内置运维工具。

## Quick start

1. Copy `config/agent.example.toml` to `config/agent.toml`.
2. Copy `config/persona.example.toml` to `config/persona.toml`.
3. Fill in the local TOML credentials, including each model entry's `api_key` when required.
4. Run `cargo check --workspace`.

`config/agent.toml` and `config/persona.toml` are local runtime files and are ignored by Git. Model and Web credentials belong in the local TOML file; the repository only tracks the example template.

## Built-in operations tools

`koi-infra::tools` registers the built-in operational tool catalog. It covers:

- filesystem inspection and scoped read/write/copy/move/delete;
- host resources, processes, filesystems, logs and network diagnostics;
- HTTP/curl-style requests, Service/systemd, Git and Docker;
- archives, package managers, process signals, crontab/systemd timers,
  firewall port rules and read-only database status/query checks;
- `system.command`, `docker.exec`, `docker.run`, `git.reset`, `git.clean` and
  Docker cleanup as Admin-only tools.

User-level tools are read-only. Operator-level tools cover scoped changes and
operations that may use `sudo -n`; arbitrary commands require Admin. The
default `ToolPolicy` fails closed: mutating tools, Admin command tools, paths,
services, HTTP hosts and database targets must be explicitly configured.

The current implementation uses structured arguments and fixed command
templates for specialized tools. It does not concatenate user input into a
shell command. Command output is bounded, and common secret-shaped output
fields are redacted before returning to the model.
