# Web frontend

这是 Koi 的 React + TypeScript + Vite 运维控制台。首版围绕任务、事件、审批和工具风险展示，默认使用演示数据；数据层已经预留 Rust 后端的 `/api/v1` JSON 接口和 SSE 事件流。

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

默认 API 根路径为 `/api/v1`。开发服务器会将 `/api` 转发到 `127.0.0.1:8080`。设置 `VITE_KOI_API_BASE` 可以覆盖路径；点击页面右上角的“演示数据”可以尝试连接真实 API。

服务端要求配置 `KOI_WEB_ADMIN_TOKEN`，未配置时会拒绝启动。首次连接时，前端会提示输入该令牌：它只保留在当前页面内存中，并用 `POST /api/v1/session` 换取同源、HttpOnly 的短期会话 Cookie，供浏览器原生 SSE 使用。不要把生产令牌写进构建产物；`VITE_KOI_WEB_TOKEN` 只适用于本地开发。

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
- `POST /api/v1/tasks/:task_id/controls`（暂停、恢复、取消、调整最低控制权限）
- `GET /api/v1/events/stream?task_id=:task_id`（SSE）
