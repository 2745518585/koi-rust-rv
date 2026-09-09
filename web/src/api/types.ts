export type TaskStatus =
  | "New"
  | "Created"
  | "Queued"
  | "Running"
  | "WaitingApproval"
  | "Pausing"
  | "Paused"
  | "Cancelling"
  | "Completed"
  | "Failed"
  | "Cancelled"
  | "Expired";

export type PermissionLevel = "None" | "User" | "Operator" | "Admin" | "System";

export type ToolSideEffect = "ReadOnly" | "Notification" | "Stateful" | "Destructive";

export type EventSource = "system" | "model" | "tool" | string;

export type EventKind =
  | "ingress"
  | "control"
  | "model"
  | "tool";

export type ApprovalStatus = "Pending" | "Approved" | "Denied" | "Expired";

export interface Scope {
  kind: string;
  id: string;
}

export interface UsageTotals {
  inputTokens: number;
  outputTokens: number;
  cachedInputTokens: number;
  reasoningTokens: number;
  costUsd: number;
}

/** 当前事件流将注入模型的文本上下文估算，不含系统提示词和工具定义。 */
export interface ContextUsage {
  estimatedTokens: number;
  contextWindowTokens: number | null;
}

export interface ModelSelection {
  provider: string;
  modelId: string;
}

export interface TaskSummary {
  taskId: string;
  isMain: boolean;
  title: string;
  status: TaskStatus;
  source: string;
  scope: Scope;
  startedAt: string;
  updatedAt: string;
  lastEventKind: EventKind;
  lastEventSummary: string;
  minimumControlPermission: PermissionLevel;
  selectedModel: ModelSelection | null;
  usage: UsageTotals;
  tokenBudget: number | null;
  contextUsage: ContextUsage;
  eventCount: number;
}

export interface TaskEvent {
  id: string;
  taskId: string;
  sequence: number;
  occurredAt: string;
  source: EventSource;
  /** 直接来源用户的显示名；无用户身份的内部事件为 null。 */
  sourceUser?: string | null;
  kind: EventKind;
  title: string;
  summary: string;
  /** 事件正文；存在时可在事件流中展开。 */
  detail?: string | null;
  permission: PermissionLevel;
  /** 所属工具调用的原始 Proposed 事件 ID；非工具事件为 null/未提供。 */
  toolProposalEventId?: string | null;
  /** 模型完成事件的供应商 usage；其他事件为 null/未提供。 */
  usage?: UsageTotals | null;
  /** 模型完成事件按当前价格配置换算的费用（美元）。 */
  costUsd?: number | null;
  payload?: unknown;
}

export interface ApprovalRequest {
  approvalRequestEventId: string;
  toolProposalEventId: string;
  taskId: string;
  toolName: string;
  toolDescription: string;
  requiredPermission: PermissionLevel;
  requestedAt: string;
  argumentsHash: string;
  argumentsPreview: string;
  scope: Scope;
  status: ApprovalStatus;
  requester: string;
  requesterSubject?: string | null;
}

export interface AuthorizationNotification {
  taskId: string;
  approvalRequestEventId: string;
  toolProposalEventId: string;
  toolName: string;
  argumentsHash: string;
  requiredPermission: PermissionLevel;
  originalEvidenceEventIds: string[];
  requesterSubject?: string | null;
}

export interface ToolDefinition {
  name: string;
  description: string;
  requiredPermission: PermissionLevel;
  sideEffect: ToolSideEffect;
  timeoutMs: number;
  modelVisible: boolean;
}

export interface HealthStatus {
  api: "healthy" | "degraded" | "offline";
  eventStore: "healthy" | "degraded" | "offline";
  modelProvider: "healthy" | "degraded" | "offline";
  lastHeartbeatAt: string;
}

export interface UsageSummary {
  inputTokensToday: number;
  cachedInputTokensToday: number;
  outputTokensToday: number;
  totalTokensToday: number;
  inputTokensMonth: number;
  cachedInputTokensMonth: number;
  outputTokensMonth: number;
  totalTokensMonth: number;
  monthSpentUsd: number;
  monthlyBudgetUsd: number;
  tokenBudgetPerTask: number | null;
  inputPricePerMillionTokens: number;
  cachedInputPricePerMillionTokens: number;
  outputPricePerMillionTokens: number;
  daily: Array<{
    label: string;
    input: number;
    cachedInput: number;
    output: number;
    total: number;
    costUsd: number;
  }>;
}

export interface SystemSnapshot {
  generatedAt: string;
  health: HealthStatus;
  tasks: TaskSummary[];
  approvals: ApprovalRequest[];
  recentEvents: TaskEvent[];
  tools: ToolDefinition[];
  models: ModelSelection[];
  defaultModel: ModelSelection | null;
  usage: UsageSummary;
}

export interface CreateTaskRequest {
  minimumPermission: PermissionLevel;
}

export interface ApprovalSubmission {
  approved: boolean;
  grant?: ApprovalGrant;
  reason?: string;
  suggestedPermission?: PermissionLevel;
}

export type ApprovalGrant =
  | { current_operation: { original_event_payload_id: string } }
  | "any_operation";

export type TaskControlRequest =
  | { action: "pause"; reason?: string }
  | { action: "resume" }
  | { action: "cancel"; reason?: string }
  | { action: "select_model"; provider: string; modelId: string }
  | { action: "set_minimum_permission"; minimumPermission: PermissionLevel };

export type StreamEvent =
  | { type: "task.updated"; task: TaskSummary }
  | { type: "event.appended"; event: TaskEvent }
  | { type: "approval.updated"; approval: ApprovalRequest }
  | { type: "authorization.requested"; request: AuthorizationNotification }
  | { type: "usage.updated"; usage: UsageSummary };
