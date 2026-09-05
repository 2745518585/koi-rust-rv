# Web frontend

这是 Koi 的 React + TypeScript + Vite 运维控制台。界面以「会话工作台」为核心：左侧选择
任务会话，中间是与 Agent 的对话；对话既可以按一般聊天气泡展示，也可以切换到完整事件流，
头部提供暂停 / 恢复、中止（打断）、重命名、删除与模型切换等受审计的控制操作。审批、工具
目录、事件审计保留为轻量功能面板。默认连接真实后端 `/api/v1`；数据层没有演示兜底数据，
后端不可用时界面会明确显示连接失败。

代码按模块拆分维护：

- `src/App.tsx` — 应用壳：登录状态、快照轮询、SSE 订阅与视图切换；
- `src/components/` — `Conversation`（会话工作台）、`Sidebar`、`ApprovalCard`、`AuthScreen`、`TaskComposerModal`；
- `src/views/` — `ApprovalsView`、`ToolsView`、`AuditView`；
- `src/lib/` — 格式化与状态/权限/事件元数据、共享 UI 组件；
- `src/api/` — HTTP 客户端、类型与空快照。

## 本地运行

```bash
npm install
npm run dev
```

生产构建输出到 `web/dist`，与 `config/agent.example.toml` 中的 `server.web_dist_dir` 对齐：

```bash
npm run build
```

## 后端接入

默认 API 根路径为 `/api/v1`。开发服务器会将 `/api` 转发到 `127.0.0.1:8080`。设置 `VITE_KOI_API_BASE` 可以覆盖路径。登录后控制台自动拉取快照并订阅 SSE；后端不可用时会显示连接错误条与重连按钮，不会回退到伪造数据。

服务端要求在 `config/agent.toml` 的 `[server].web_admin_token` 中配置管理端令牌，未配置时会拒绝启动。首次连接时，前端会提示输入该令牌：它只保留在当前页面内存中，并用 `POST /api/v1/session` 换取同源、HttpOnly 的短期会话 Cookie，供浏览器原生 SSE 使用。不要把生产令牌写进构建产物。

普通使用者可直接在前端以邮箱、用户名和密码注册，或使用邮箱与密码登录。账号保存在 `server.user_store_path`（默认 `./data/users.json`），密码仅以 Argon2 哈希形式保存。用户名注册后不可变，并会作为写入核心事件的 `Principal.subject`。

前端预期的最小接口为：

- `GET /api/v1/dashboard`
- `POST /api/v1/session`（Bearer 令牌换取 SSE 会话）
- `POST /api/v1/auth/register`、`POST /api/v1/auth/login`、`GET /api/v1/auth/me`
- `GET /api/v1/tasks/:task_id/events`
- `POST /api/v1/tasks/:task_id/events`（追加受限的 Web 上下文事件）
- `POST /api/v1/tasks`
- `POST /api/v1/approvals/:approval_request_event_id`
- `POST /api/v1/authorizations/:approval_request_event_id`（提权确认接口，兼容审批接口）
- `POST /api/v1/tasks/:task_id/cancellation-requests`（记录取消 Ingress）
- `POST /api/v1/tasks/:task_id/controls`（暂停、恢复、取消、调整最低控制权限、切换模型）
- `GET /api/v1/events/stream?task_id=:task_id`（SSE）

输入框下方的“建议授权”选择器会随新任务、会话输入、取消请求和审批请求发送
`suggestedPermission`。它只是本次操作的权限上限建议，服务端仍会按当前身份、Web 来源
上限和核心权限规则重新核定；未提供该字段的旧客户端默认使用当前身份权限。

模型切换使用 `select_model` 控制事件；可用供应商模型组合由 dashboard 响应的 `models` 字段返回：

```json
{
  "action": "select_model",
  "provider": "deepseek",
  "modelId": "deepseek-chat"
}
```

未显式选择时，任务使用 `[models].default_provider` 与 `[models].default_model_id` 指定的模型。
模型条目直接配置 `provider` 和 `model_id`，不需要额外的应用别名。
