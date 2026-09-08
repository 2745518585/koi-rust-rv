# Koi Rust Remastered Version

Koi Rust Remastered Version，即 koi Rust 重置版，原 koi 实现见 [Pond-Ink/koi](https://github.com/Pond-Ink/koi)

> [!CAUTION]
> 本项目完全依靠 AI 完成落地，谨慎使用。

## 开始

> 注：本节内容由 AI 生成。

前置依赖：Rust stable 工具链（见 `rust-toolchain.toml`，edition 2024）。Web 前端（`web/`，Vite + TypeScript + React）需要 Node.js/npm 构建，首次使用前必须先执行下面的构建。

### 构建 Web 前端

```bash
cd web
npm install          # 首次或 package-lock.json 变更后执行
npm run build        # 等价于 tsc --noEmit && vite build，产物输出到 web/dist
```

- 产物位置：`npm run build` 输出到 `web/dist`，由后端 `[server].web_dist_dir` 指向并在启动时托管（若该目录不存在，服务会提示并只提供 API）。
- 开发模式：`npm run dev` 启动 Vite 开发服务器（默认 `http://127.0.0.1:5173`，`/api` 已代理到 `http://127.0.0.1:8080`），修改前端代码即时生效、无需重新构建；联调前先启动后端 `cargo run -p koi-server`。
- 预览产物：`npm run preview`（默认 `http://127.0.0.1:4173`）。

然后按以下步骤配置并启动：

1. 复制 `config/agent.example.toml` 为 `config/agent.toml`（本地运行时文件，已被 Git 忽略），填写：
   - `[server]`：监听地址、Web 构建目录、JSONL 事件目录（`data/events`）、Web 用户库路径（`data/users.json`）、Cookie 是否要求 `Secure`；
   - `[models]`：`default_provider`/`default_model_id` 与至少一条可用模型条目（供应商、`base_url`、`model_id`、`api_key`、协议、超时与上下文窗口）；支持 OpenAI Responses 与 Chat Completions 两类协议；
   - `[qq]`：QQ 开放平台 AppID/AppSecret（或设置 `QQ_BOT_APP_ID`、`QQ_BOT_APP_SECRET` 环境变量）；不填写凭证时 QQ 来源自动跳过；
   - `[monitor]`：可选的本地基础服务监测；支持 HTTP、TCP 和本机原生服务状态检查；
   - `[alerts]`：外部告警 Webhook 的来源名与密钥；密钥也可以通过 `KOI_ALERT_WEBHOOK_TOKEN` 环境变量提供；
   - `[usage]`：可选月度预算（当前仅用于展示）。
2. 复制 `config/authorization.example.toml` 为 `config/authorization.toml`，声明核心身份权限目录：`[source_defaults]` 给出来源的默认身份权限，`[[principals]]` 给出 `(来源, 用户)` 精确身份权限；精确身份优先于来源默认值，未配置的身份按 `None` 失败关闭。该目录是核心权限裁决的依据，外部来源只能提交建议权限、不能改写它。
3. 运行 `cargo run -p koi-server`（启动时读取 `config/agent.toml` 与 `config/authorization.toml`）。模型系统提示词内嵌于 `koi-server`（`apps/koi-server/prompts/main.md`、`qq.md`、`child.md`；QQ 片段会组装到主会话提示词），无需额外配置。
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

### 服务监测与外部告警 Webhook

项目内置一个轻量服务监测器，监测器不调用 Agent 工具，而是执行确定性的检查，在状态变化时把告警写入主会话：

- `kind = "http"`：检查 HTTP/HTTPS 状态码，默认期望 `200`；
- `kind = "tcp"`：检查 `host:port` 是否可以建立连接；
- `kind = "service"`：Windows 使用 `sc.exe` 检查服务，Linux/Unix 使用 `systemctl` 检查服务是否 active。

示例配置：

```toml
[monitor]
enabled = true
instance = "server-01"
interval_secs = 30
timeout_secs = 5
failure_threshold = 3
recovery_threshold = 2

[[monitor.checks]]
id = "koi-api"
kind = "http"
target = "http://127.0.0.1:8080/healthz"
name = "Koi API"
severity = "critical"
expected_status = 200
```

连续失败达到 `failure_threshold` 后产生 `firing` 告警，连续恢复达到 `recovery_threshold` 后产生 `resolved` 告警；重复状态不会持续刷入事件流。监测器以独立后台任务运行，服务器退出时会随统一关闭令牌停止。

外部监控系统可向以下任一地址发送 JSON `POST` 请求：

```text
/api/v1/alerts/webhook
/api/v1/webhooks/alerts
```

请求使用 `Authorization: Bearer <token>` 或 `X-Koi-Webhook-Token: <token>` 鉴权。默认来源为 `alertmanager`，也可以在 `[alerts].webhook_source` 中选择 `webhook`；请求体不能自行提升或切换来源权限。接口同时接受 Alertmanager 风格的 `alerts` 数组和单条规范化告警对象，并按来源实例与告警指纹去重。告警最终都会以 `ContextKind::Alert` 进入 `TaskId::MAIN`，高风险工具操作仍需要现有审批流程。

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

投送工具指模型向外部渠道（群聊、私聊、Web 通知等）主动发送消息的出站通道。QQ 凭证配置完整时注册 `qq.reply` 与 `qq.group_send`：前者要求 `User` 权限，只接收正文并回复当前工具调用授权父事件所选中的 QQ 入站消息；后者要求 `Operator` 权限，接收指定 `group_openid`、`content` 以及可选的 `reply_to_message_id`。如果配置了 `[qq].report_group_openid`，还会注册只接收正文、目标固定为该群的 `qq.report`（`Operator`）。三者副作用类别均为 `Notification`，工具调用仍先由核心完成权限审查，QQ API 返回的消息 ID 会作为工具结果写回事件流；未配置 QQ 时不会暴露这些工具。QQ 模型最终文本不会自动转发，是否回复必须由模型显式调用工具决定；审批/提权流程仍由核心调用来源的 `SourceAuthorizationProvider` 送达原群。

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

#### qq

QQ 来源对接 QQ 开放平台 Bot API v2：启动后使用 AppID/AppSecret 获取 App Access Token，再通过 Gateway 接收事件，并使用官方 HTTP API 回复消息。默认订阅群聊@与 C2C 事件；如需频道@消息，在 `intents` 中加入 `PUBLIC_GUILD_MESSAGES`（`1 << 30`），如需频道私信则加入 `DIRECT_MESSAGE`（`1 << 12`）。

- **支持的输入**：C2C 私聊、群聊@、频道@与频道私信；普通 `GROUP_MESSAGE_CREATE` 在 `mention_only = true` 时会被过滤。
- **会话与审计**：所有 QQ C2C/群/频道消息统一注入 Koi 主会话（`TaskId::MAIN`），消息统一写入主会话事件存储，再由 Agent Supervisor 调度；每条模型可见正文前都会标记 QQ 来源类型、会话标识、发言人、消息 ID 及是否明确 `@bot`，避免跨群汇总时混淆；结构化 `scope`、`actor` 与 `origin` 仍作为权限和回复路由的事实依据；重启后从事件流恢复消息去重状态。
- **可靠性**：Gateway 实现 Hello/Identify、Heartbeat、Resume、Reconnect 与断线退避；HTTP 请求带超时、有限重试和 Token 缓存。
- **权限与审批**：QQ 来源的权限建议固定为两级：普通发言建议 `User`，明确 @bot 的发言建议 `Operator`；来源注册最高为 `Operator`，不会从 QQ 输入建议 `Admin`。工具需要提权时，QQ 会在群里描述操作并给出 token；有权限成员必须 @bot 回复 `/confirm <token>` 确认当前操作，或回复 `/confirm all` 确认当前群全部待处理操作。最终权限仍由 `authorization.toml` 按来源和具体 `subject` 截断，未配置身份继续失败关闭。
- **模型控制出站**：QQ 模型最终文本不会自动发送回 QQ。模型需要回复当前 QQ 消息时调用 `qq.reply`（目标从授权父事件恢复，参数只有正文）；需要向指定群主动通知时调用 `qq.group_send`；配置 `[qq].report_group_openid` 后，可调用 `qq.report` 向固定的主要汇报群发送事故或运维报告。三类投送都会按 QQ 来源的单条消息长度配置拆分。
- **主要汇报群**：在 `[qq]` 中设置 `report_group_openid = "<group_openid>"` 即可启用 `qq.report`。该工具不接受目标群参数，模型不能改写汇报目的地；提权审批通知仍发送到发起操作的原群。

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

- QQ：`qq.reply`（QQ 凭证完整时注册，`User`，`Notification`）、`qq.group_send`（`Operator`，`Notification`）、`qq.report`（配置主要汇报群后注册，`Operator`，`Notification`）

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
