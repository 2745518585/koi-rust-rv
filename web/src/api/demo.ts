import type {
  ApprovalRequest,
  EventKind,
  HealthStatus,
  PermissionLevel,
  SystemSnapshot,
  TaskEvent,
  TaskStatus,
  TaskSummary,
  ToolDefinition,
  ToolSideEffect,
} from "./types";

const MAIN_TASK_ID = "00000000-0000-0000-0000-000000000000";

function ago(minutes: number): string {
  return new Date(Date.now() - minutes * 60_000).toISOString();
}

function task(
  taskId: string,
  title: string,
  status: TaskStatus,
  source: string,
  scopeKind: string,
  scopeId: string,
  lastEventKind: EventKind,
  lastEventSummary: string,
  eventCount: number,
  updatedMinutesAgo: number,
  isMain = false,
): TaskSummary {
  return {
    taskId,
    isMain,
    title,
    status,
    source,
    scope: { kind: scopeKind, id: scopeId },
    startedAt: ago(updatedMinutesAgo + 18),
    updatedAt: ago(updatedMinutesAgo),
    lastEventKind,
    lastEventSummary,
    minimumControlPermission: status === "WaitingApproval" ? "Operator" : "User",
    selectedModel: null,
    usage: {
      inputTokens: 4800 + eventCount * 68,
      outputTokens: 1200 + eventCount * 17,
      cachedInputTokens: 720,
      reasoningTokens: 860,
    },
    eventCount,
  };
}

function event(
  id: string,
  taskId: string,
  sequence: number,
  source: TaskEvent["source"],
  kind: EventKind,
  title: string,
  summary: string,
  permission: PermissionLevel,
  occurredMinutesAgo: number,
): TaskEvent {
  return {
    id,
    taskId,
    sequence,
    occurredAt: ago(occurredMinutesAgo),
    source,
    kind,
    title,
    summary,
    permission,
  };
}

function tool(
  name: string,
  description: string,
  requiredPermission: PermissionLevel,
  sideEffect: ToolSideEffect,
  modelVisible = true,
): ToolDefinition {
  return {
    name,
    description,
    requiredPermission,
    sideEffect,
    timeoutMs: sideEffect === "ReadOnly" ? 30_000 : 120_000,
    modelVisible,
  };
}

export function createDemoSnapshot(): SystemSnapshot {
  const tasks: TaskSummary[] = [
    task(
      MAIN_TASK_ID,
      "主会话",
      "Running",
      "web",
      "workspace",
      "ops-room",
      "model",
      "正在汇总订单 API 的连接池指标",
      47,
      1,
      true,
    ),
    task(
      "2c70d202-6eb9-4e2d-9b66-4dbdaec7e416",
      "订单 API 连接池异常",
      "WaitingApproval",
      "qq",
      "service",
      "order-api",
      "approval",
      "等待 Operator 确认重启服务",
      18,
      4,
    ),
    task(
      "2c70d202-6eb9-4e2d-9b66-4dbdaec7e417",
      "Redis 主从延迟初诊",
      "Completed",
      "alertmanager",
      "server",
      "prod-1",
      "control",
      "已生成初诊摘要并回传告警频道",
      23,
      16,
    ),
    task(
      "2c70d202-6eb9-4e2d-9b66-4dbdaec7e418",
      "数据库备份窗口检查",
      "Paused",
      "web",
      "database",
      "koi-main",
      "control",
      "由值班人员暂时挂起，保留当前上下文",
      12,
      29,
    ),
    task(
      "2c70d202-6eb9-4e2d-9b66-4dbdaec7e419",
      "网关 5xx 峰值排查",
      "Failed",
      "alertmanager",
      "service",
      "gateway",
      "system",
      "上游指标接口连续超时，任务已失败",
      9,
      51,
    ),
  ];

  const approvalId = "7a1e5b62-7d8b-4a75-9e68-9d9cbe0b2a01";
  const approvals: ApprovalRequest[] = [
    {
      approvalRequestEventId: approvalId,
      taskId: tasks[1].taskId,
      toolName: "service.restart",
      toolDescription: "重启策略允许范围内的 systemd 服务",
      requiredPermission: "Operator",
      requestedAt: ago(4),
      argumentsHash: "sha256:8e1c…9ac2",
      argumentsPreview: '{ "service": "order-api" }',
      scope: { kind: "service", id: "order-api" },
      status: "Pending",
      requester: "QQ · 运营群",
    },
    {
      approvalRequestEventId: "7a1e5b62-7d8b-4a75-9e68-9d9cbe0b2a02",
      taskId: tasks[2].taskId,
      toolName: "network.probe",
      toolDescription: "探测 allowlist 中的主机连通性",
      requiredPermission: "User",
      requestedAt: ago(24),
      argumentsHash: "sha256:61b0…07fd",
      argumentsPreview: '{ "host": "redis.prod-1" }',
      scope: { kind: "server", id: "prod-1" },
      status: "Approved",
      requester: "告警 · Redis",
    },
  ];

  const recentEvents: TaskEvent[] = [
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12201",
      tasks[1].taskId,
      18,
      "system",
      "approval",
      "授权请求已登记",
      "service.restart 需要 Operator 权限，已将参数指纹固定并等待确认",
      "System",
      4,
    ),
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12202",
      MAIN_TASK_ID,
      47,
      "model",
      "model",
      "模型正在分析连接池",
      "已将最近 32 条上下文与数据库状态摘要合并到当前调用",
      "None",
      5,
    ),
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12203",
      tasks[1].taskId,
      17,
      "model",
      "tool",
      "提出 service.restart",
      "模型建议重启 order-api，并绑定当前 QQ 来源证据",
      "None",
      7,
    ),
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12204",
      tasks[2].taskId,
      23,
      "tool",
      "tool",
      "network.probe 已完成",
      "redis.prod-1 延迟 12ms，未发现网络层丢包",
      "None",
      16,
    ),
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12205",
      tasks[3].taskId,
      12,
      "system",
      "control",
      "任务已挂起",
      "值班人员暂时停止数据库备份窗口检查，事件流保持可恢复",
      "Operator",
      29,
    ),
    event(
      "a1e5b4c0-455f-4c8b-a7af-2bd5e2a12206",
      tasks[4].taskId,
      9,
      "system",
      "system",
      "任务执行失败",
      "上游指标接口连续超时，未执行任何有副作用的工具",
      "None",
      51,
    ),
  ];

  const health: HealthStatus = {
    api: "healthy",
    eventStore: "healthy",
    modelProvider: "healthy",
    lastHeartbeatAt: ago(0),
  };

  return {
    generatedAt: new Date().toISOString(),
    health,
    tasks,
    approvals,
    recentEvents,
    tools: [
      tool("filesystem.inspect", "读取受策略约束的文件与目录元数据", "User", "ReadOnly"),
      tool("system.processes", "查看主机当前进程与资源占用", "User", "ReadOnly"),
      tool("network.probe", "探测 allowlist 中的网络目标", "User", "ReadOnly"),
      tool("http.request", "向允许的 HTTP 主机发起诊断请求", "User", "ReadOnly"),
      tool("database.query", "在只读事务中查询允许的数据库目标", "User", "ReadOnly"),
      tool("service.restart", "重启策略允许范围内的系统服务", "Operator", "Stateful"),
      tool("git.reset", "恢复仓库到指定提交或工作树状态", "Admin", "Destructive"),
      tool("system.command", "执行结构化 Admin 命令入口", "Admin", "Destructive", false),
    ],
    models: [],
    defaultModel: null,
    usage: {
      inputTokensToday: 128_640,
      outputTokensToday: 34_820,
      monthSpentUsd: 3.84,
      monthlyBudgetUsd: 10,
      daily: [
        { label: "周一", input: 42, output: 18 },
        { label: "周二", input: 58, output: 23 },
        { label: "周三", input: 36, output: 16 },
        { label: "周四", input: 71, output: 31 },
        { label: "周五", input: 54, output: 24 },
        { label: "周六", input: 29, output: 12 },
        { label: "今天", input: 64, output: 28 },
      ],
    },
  };
}

/**
 * 后端不可用时使用的空壳数据。不能回退到演示任务，否则真实部署故障会被伪装成
 * 一套看似可操作的界面。
 */
export function createEmptySnapshot(): SystemSnapshot {
  return {
    generatedAt: new Date().toISOString(),
    health: {
      api: "offline",
      eventStore: "offline",
      modelProvider: "offline",
      lastHeartbeatAt: new Date().toISOString(),
    },
    tasks: [],
    approvals: [],
    recentEvents: [],
    tools: [],
    models: [],
    defaultModel: null,
    usage: {
      inputTokensToday: 0,
      outputTokensToday: 0,
      monthSpentUsd: 0,
      monthlyBudgetUsd: 0,
      daily: [],
    },
  };
}

export { MAIN_TASK_ID };
