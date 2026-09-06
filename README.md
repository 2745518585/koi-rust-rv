# Koi Rust Remastered Version

Koi Rust Remastered Version，即 koi Rust 重置版，原 koi 实现见 [Pond-Ink/koi](https://github.com/Pond-Ink/koi)

> [!CAUTION]
> 本项目完全依靠 AI 完成落地，谨慎使用。

## 开始

> 注：本节内容由 AI 生成。

前置依赖：Rust stable 工具链（见 `rust-toolchain.toml`，edition 2024）。如要修改 Web 前端（`web/`，Vite + TypeScript），需 Node.js 并重新构建到 `web/dist`（由 `[server].web_dist_dir` 指向，服务启动时托管）。

1. 复制 `config/agent.example.toml` 为 `config/agent.toml`（本地运行时文件，已被 Git 忽略），填写：
   - `[server]`：监听地址、Web 构建目录、JSONL 事件目录（`data/events`）、Web 用户库路径（`data/users.json`）、Cookie 是否要求 `Secure`；
   - `[models]`：`default_provider`/`default_model_id` 与至少一条可用模型条目（供应商、`base_url`、`model_id`、`api_key`、协议、超时与上下文窗口）；支持 OpenAI Responses 与 Chat Completions 两类协议；
   - `[usage]`：可选月度预算（当前仅用于展示）。
2. 复制 `config/authorization.example.toml` 为 `config/authorization.toml`，声明核心身份权限目录：`[source_defaults]` 给出来源的默认身份权限，`[[principals]]` 给出 `(来源, 用户)` 精确身份权限；精确身份优先于来源默认值，未配置的身份按 `None` 失败关闭。该目录是核心权限裁决的依据，外部来源只能提交建议权限、不能改写它。
3. 运行 `cargo run -p koi-server`（启动时读取 `config/agent.toml` 与 `config/authorization.toml`）。模型系统提示词内嵌于 `koi-server`（`apps/koi-server/prompts/main.md`、`child.md`），无需额外配置。
4. 打开浏览器访问 `[server].bind_addr`：先注册 Web 账号，随后把该账号写入 `authorization.toml`（`[[principals]] source="web" subject="<用户名>" permission="Admin"`）并重启服务以管理员身份使用——权限目录仅在启动时加载。

## 核心用法

以下说明均基于设计中的理想情况，实际实现可能存在偏差。

### 会话

本项目为满足多人协作场景，设计了主会话与任务会话两类会话。其中主会话职能为监听用户、告警等输入，管理子任务会话完成任务，原则上自身不进行长时间任务；子任务会话的职能为按照主会话或用户的指示完成任务。只有主会话拥有调用特殊工具会话管理器的权限，子任务会话无法看到也无法调用会话管理器。

### 事件

本项目针对多用户在同一 Agent 会话协作的场景，设计了一套权限审查体系。

本项目中核心对外通信以事件形式完成，共抽象为四类事件：
- 输入事件：向具体会话的输入，需要注入会话上下文。
- 输出事件：模型的输出，目前不包含模型思维链输出。
- 工具事件：工具相关事件，包括模型调用工具时发起的事件、工具审查结果、工具返回结果等。
- 控制事件：既不注入上下文，也不来自模型输出的事件，用于外部发起中断、暂停等或内部包括失败等。

### 权限

每一个事件包含一个来源，来源由来源模块、来源用户、来源建议权限组成，其中来源模块与来源用户的二元组为一个权限主题，其拥有的最高权限记录在核心中并通过核心给出，一个来源的权限为来源用户权限和来源建议权限的较低值。具体的，权限由高到低为：
- `System`：只能来自模块 `system`，即核心内部发起的事件，包括系统提示词等。
- `Admin`：外部最高权限，原则上可以进行任意操作。
- `Operator`：操作者权限，原则上可以完成所有现有的运维操作，但无法进行任意代码执行。
- `User`：用户权限，原则上只进行可读且不需要提权的操作。
- `None`：无权限，一般用于模型输出、工具输出等不应当包含权限的事件。

一个事件的权限除了来自自身的来源，还可以来自上级事件。每个事件可以链接一个上级事件，则该事件将继承上级事件的权限。由于模型自身不具有权限，因此模型发出的事件要包含权限必须继承自上级权限，而模型给出的上级事件必须为注入其会话上下文的输入事件。特别的，`System` 权限事件、过期事件也不能作为上级事件授权。

要注入输入事件、控制事件，或者发起工具事件，必须经过权限检查。输入事件、控制事件所拥有的权限必须不低于注入会话的最低控制权限，工具事件所拥有的权限必须不低于该工具的最低调用权限。最低控制权限在启动会话的控制事件中需要给出，同时可以通过控制事件修改最低控制权限，均不得高于该控制事件本身的权限。工具的最低调用权限由外部工具注册时给出。

检查事件权限时，若经过向上追溯后最终来源权限不足，则视为未通过检查。此时若来源用户权限足够但来源建议权限不足，则会调用来源方提供的提权接口，由来源方发起提权请求。用户可在来源选择拒绝、允许当前操作与允许任意操作，提权完成后创建新的权限足够的输入事件注入会话上下文。若只允许当前操作，则提权事件会携带原事件 id，作为上级事件授权时会检查事件负载与原事件负载是否相同。

### 来源接口

> 注：本节内容由 AI 生成。

外部来源（QQ、Web、Alertmanager 等）通过以下核心接口与 Agent 协作；完整参考实现为 `koi-infra::web_source::KoiWebSource`：

- **注册来源**：向 `IngressSourceRegistry` 注册 `IngressSourceDefinition { source, maximum_permission }`（例如 Web 来源登记上限 `Admin`）。来源上限与身份记录权限共同截断输入的建议权限，未登记来源的输入直接拒绝。
- **解析身份权限**：实现 `IngressPermissionResolver::maximum_permission`，返回该（来源模块、来源用户）在核心记录中的最高权限。宿主可直接使用 `StaticPermissionDirectory`：由 `config/authorization.toml` 加载，查询顺序为 精确身份 > 来源默认值 > `None`。
- **提交输入**：来源方在完成自己的认证后构造 `IngressDraft`（`Context` 上下文输入 / `Approval` 提权确认 / `Cancellation` 取消请求），交给 `IngressRegistrar` 落为已审计事件；`suggested_permission` 仅是建议，核心会按 来源上限 与 身份目录 重新核定并持久化结论。
- **处理提权**：实现 `SourceAuthorizationProvider` 并注册进 `SourceAuthorizationRegistry`。核心在权限不足时以 `AuthorizationRequest`（含原工具提案、参数指纹、所需权限）调用它，来源方返回 `Denied` / `Pending`（先展示确认，随后以新输入事件继续）/ `Authorized`（核心仍会独立读取并复核该输入事件的绑定关系）。
- **执行控制事件**：控制事件不注入上下文，由 `ControlExecutor` 直接执行并写入事件流。外部调用方必须先构造经过来源注册、身份认证与权限截断的 `DirectControlAuthority`；模型与工具不能作为控制事件的直接来源。

### 工具接口

> 注：本节内容由 AI 生成。

- **声明与注册**：实现 `ToolExecutor`，携带 `ToolDefinition`（名称、描述、JSON Schema 输入、`required_permission` 最低调用权限、`side_effect` 副作用类别、超时、`model_visible`、`main_session_only`）注册进 `ToolRegistry`。Schema 非法、普通工具缺少最低权限、重名注册都会被拒绝。
- **调用链**：模型只能引用注入其会话上下文的输入事件作为上级授权证据 → 核心解析权限链并记录 `ToolEvent::Proposed` / `AuthorizationChecked` → 权限满足时构造 `AuthorizedToolInvocation`（绑定工具提案事件与执行事件、记录实际采纳的授权证据）交给执行器；执行与错误结果以 `ToolEvent` 回传模型。执行器不自行解释群聊或 Web 的授权语义。
- **权限不足**：核心记录审批请求并调用来源的提权接口；用户同意后以新的输入事件注入会话，核心复核绑定后才继续执行原工具调用。
- **会话管理工具**：`task.*` 系列（`task.start` / `task.input` / `task.control` / `task.name` / `task.delete` / `task.list` / `task.inspect`）由 `koi-core::agent::task_tools` 注册为占位实现，真正执行由 `AgentLoop` 拦截并以主会话事件流审计。它们 `main_session_only`，调用权来自“主会话”结构身份而非输入证据，因此对子会话不可见、不可调用。

#### 运维工具

> 注：本节内容由 AI 生成。

运维工具指由 `koi-infra::tools` 内置、对服务器进行读取或变更的“传统工具”目录。它们使用结构化参数与固定命令模板：命令一律以 argv 数组启动，不经 shell 拼接，每个工具的程序名固定（任意程序执行只存在于 Admin 级工具 `system.command`）。工具定义携带 `required_permission` 与 `side_effect`，核心在每次调用前依据授权证据做权限检查，并把提案、审查、执行与结果全部写成可审计的工具事件。工具将命令输出视为不可信数据：输出有界、超时受控，并对常见敏感形状（口令、令牌、密钥等）做脱敏后才返回模型。

#### 投送工具

> 注：本节内容由 AI 生成。

投送工具指模型向外部渠道（群聊、私聊、Web 通知等）主动发送消息的出站通道。**当前仓库未注册任何投送工具**：模型的最终文本只作为模型输出事件写入会话，由宿主与来源方界面负责展示。出站通知属于来源方的职责——审批/提权流程由核心调用来源的 `SourceAuthorizationProvider` 送达（Web 来源通过 SSE 事件流与审批界面呈现）。

#### 记忆工具

> 注：本节内容由 AI 生成。

当前实现保留了记忆能力的核心端口（`koi-core::ports::memory::MemoryStore`）与语义（`ModelInputRole::Memory`、`MemoryQuery`/`MemoryContextBuilder`；记忆以 `None` 权限注入，仅作参考资料、不可授权）。但**未内置注册记忆工具，也没有持久化实现**：`AgentLoop` 支持按运行请求注入记忆检索结果，宿主运行器当前未挂载记忆存储，因此记忆的写入/检索目前不可用，属预留能力。

## 预实现外部模块

### 预实现来源

#### web

> 注：本节内容由 AI 生成。

Web 是仓库内置并默认启用的外部来源（`koi-infra::web_source::KoiWebSource` + `koi-infra::web_identity::WebUserStore`），作为其他来源（QQ、告警等）的参考实现：

- **账号与会话**：本地 `users.json` 保存账号（argon2 密码哈希），登录/注册后签发 8 小时随机令牌 Cookie（`HttpOnly`、`SameSite=Strict`，是否带 `Secure` 由 `[server].web_cookie_secure` 决定）；会话仅存于进程内存，服务重启后需重新登录。
- **身份权限**：Web 身份权限不随账号记录保存，而是由核心权限目录决定（`authorization.toml` 中 `source="web"` 的 `source_defaults`/`principals`）；Web 来源在 `IngressSourceRegistry` 中的来源上限为 `Admin`。
- **能力**：创建/浏览任务与会话事件流、向会话追加输入（含告警类）、请求取消、审批或否决提权请求、对会话发起控制（暂停/恢复/取消/选择模型/修改最低控制权限）、命名与删除任务。任务可见性与可操作性以“会话最低控制权限”为门槛，每个 SSE 事件还会再次复核。
- **实时推送**：`koi-api` 提供 REST 路由与 SSE 事件流（模型、工具与系统事件经事件存储订阅转发，Web 自身命令事件由 Web 来源直接发布）。
- **接入方式**：`koi-server` 启动时装配 `ModelProviderRegistry`（[models] 条目）、`TaskManager` 与 `AgentSupervisor`，事件存储为 JSONL 文件（`JsonlEventStore`），单一进程部署。

### 预实现工具

#### 运维工具

> 注：本节内容由 AI 生成。

- 文件系统：`fs.read` `fs.list` `fs.stat` `fs.find` `fs.search` `fs.write` `fs.mkdir` `fs.copy` `fs.move` `fs.delete`
- 主机与系统：`system.info` `system.resources` `system.processes` `system.filesystems` `system.logs` `system.kernel_messages`（只读），任意命令执行 `system.command`（Admin）
- 网络与 HTTP：`network.interfaces` `network.connections` `network.routes` `network.dns_lookup` `network.port_check` `network.tls_check`、`http.get` `http.request`、`curl.get` `curl.request`
- 服务与进程：`service.status` `service.logs` `service.start` `service.stop` `service.restart` `service.reload` `service.enable` `service.disable` `service.mask` `service.unmask` `service.daemon_reload`、`process.signal` `process.renice`
- Git 与 Docker：`git.status` `git.log` `git.diff` `git.show` `git.branch` `git.remote` `git.fetch` `git.pull` `git.push` `git.add` `git.commit` `git.merge` `git.rebase` `git.checkout` `git.stash` `git.clone` `git.reset` `git.clean`；`docker.version` `docker.ps` `docker.inspect` `docker.logs` `docker.stats` `docker.images` `docker.pull` `docker.start` `docker.stop` `docker.restart` `docker.rm` `docker.run` `docker.exec` `docker.build` `docker.push` `docker.tag` `docker.prune` `docker.compose_up` `docker.compose_down`
- 包、计划任务与防火墙：`package.search` `package.list` `package.install` `package.upgrade` `package.remove`；`schedule.list` `schedule.install` `schedule.clear` `schedule.timers`；`firewall.status` `firewall.port` `firewall.reload`
- 归档与数据库：`archive.create` `archive.list` `archive.extract`；`database.status` `database.query_readonly`
- 会话管理（主会话专用，见“会话”与“工具接口”）：`task.start` `task.input` `task.control` `task.name` `task.delete` `task.list` `task.inspect`

#### 投送工具

> 注：本节内容由 AI 生成。

（无）模型无主动投送通道；出站通知与审批请求由来源方送达，Web 来源通过 SSE 与审批界面完成。

## 卸载

> 注：本节内容由 AI 生成。

仓库不提供卸载脚本，手动卸载即可：

1. 停止 `koi-server` 进程（Ctrl+C 或结束进程）；
2. 删除运行期生成的数据与本地配置：事件目录（`[server].event_store_dir`，默认 `data/events/`）、Web 用户库（`[server].user_store_path`，默认 `data/users.json`）、`config/agent.toml` 与 `config/authorization.toml`（这两个文件本身不入库）；
3. 可选：清理构建产物（`cargo clean`；删除 `web/dist`，如需前端源码构建则保留 `web/`）。

## 依赖项

> 注：本节内容由 AI 生成。

- Rust stable 工具链（`rust-toolchain.toml`，edition 2024，workspace 要求 rust-version 1.85）
- 运行与 Web：`tokio`、`axum`、`tower-http`
- 数据与标识：`serde` / `serde_json`、`chrono`、`uuid`（事件/任务 ID 使用 v7）
- 安全与网络客户端：`argon2`（Web 密码哈希）、`rand`、`reqwest`（rustls，模型供应商与 HTTP 工具共用）
- 异步与错误：`async-trait`、`futures-util`、`tokio-util`、`thiserror`
- 日志：`tracing` / `tracing-subscriber`
- 持久化：事件存储当前为单进程 JSONL 文件实现（`JsonlEventStore`，`koi-infra::event_store`）；`sqlx`（SQLite）已列入 workspace 依赖，作为后续数据库适配器预留，当前代码未使用
- Web 前端：`web/`（Vite + TypeScript），构建产物为 `web/dist`，由 `koi-server` 静态托管

## 开源协议

[MIT](LICENSE) LICENSE

## 致谢

感谢 GPT 5.6 系列、GLM 5.3 系列、DeepSeek V4 系列模型对此项目的贡献。

感谢 [Pond Ink](https://github.com/Pond-Ink) 团队提供服务器和群聊用于测试。

感谢清华大学计算机科学与技术系提供的平台支持。

原始项目：[Pond-Ink/koi](https://github.com/Pond-Ink/koi)
