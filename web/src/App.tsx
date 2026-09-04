import { useEffect, useMemo, useState } from "react";
import type { FormEvent } from "react";
import {
  Activity,
  AlertTriangle,
  ArrowUpRight,
  Bell,
  Bot,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CircleDot,
  CircleStop,
  Clock3,
  Command,
  Database,
  FileClock,
  Filter,
  Gauge,
  Globe2,
  Inbox,
  LayoutDashboard,
  LoaderCircle,
  Menu,
  MessageSquare,
  MoreHorizontal,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Search,
  Server,
  Settings2,
  ShieldAlert,
  ShieldCheck,
  Sparkles,
  Terminal,
  TicketCheck,
  Wifi,
  Wrench,
  X,
  XCircle,
  Zap,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { createDemoSnapshot, MAIN_TASK_ID } from "./api/demo";
import { createKoiApiClient } from "./api/client";
import type { AuthUser } from "./api/client";
import type {
  ApprovalRequest,
  ApprovalStatus,
  EventKind,
  PermissionLevel,
  StreamEvent,
  SystemSnapshot,
  TaskEvent,
  TaskStatus,
  TaskSummary,
  ToolDefinition,
  ToolSideEffect,
} from "./api/types";
import { useI18n } from "./i18n";

type ViewKey = "overview" | "tasks" | "approvals" | "tools" | "audit";

interface NavItem {
  key: ViewKey;
  label: string;
  icon: LucideIcon;
  count?: number;
}

const navIcons: Array<Pick<NavItem, "key" | "icon">> = [
  { key: "overview", icon: LayoutDashboard }, { key: "tasks", icon: Command },
  { key: "approvals", icon: TicketCheck }, { key: "tools", icon: Wrench },
  { key: "audit", icon: FileClock },
];

const statusMeta: Record<
  TaskStatus,
  { label: string; className: string; icon: LucideIcon }
> = {
  New: { label: "新建", className: "status-neutral", icon: CircleDot },
  Created: { label: "已创建", className: "status-neutral", icon: CircleDot },
  Queued: { label: "排队中", className: "status-info", icon: Clock3 },
  Running: { label: "运行中", className: "status-running", icon: LoaderCircle },
  WaitingApproval: { label: "待审批", className: "status-warning", icon: ShieldAlert },
  Paused: { label: "已暂停", className: "status-paused", icon: Pause },
  Cancelling: { label: "取消中", className: "status-warning", icon: CircleStop },
  Completed: { label: "已完成", className: "status-success", icon: CheckCircle2 },
  Failed: { label: "失败", className: "status-danger", icon: XCircle },
  Cancelled: { label: "已取消", className: "status-neutral", icon: CircleStop },
  Expired: { label: "已过期", className: "status-neutral", icon: Clock3 },
};

const permissionMeta: Record<PermissionLevel, { label: string; className: string }> = {
  None: { label: "无权限", className: "permission-none" },
  User: { label: "User", className: "permission-user" },
  Operator: { label: "Operator", className: "permission-operator" },
  Admin: { label: "Admin", className: "permission-admin" },
  System: { label: "System", className: "permission-system" },
};

const eventKindMeta: Record<EventKind, { label: string; className: string; icon: LucideIcon }> = {
  ingress: { label: "输入", className: "event-ingress", icon: MessageSquare },
  model: { label: "模型", className: "event-model", icon: Bot },
  tool: { label: "工具", className: "event-tool", icon: Terminal },
  approval: { label: "授权", className: "event-approval", icon: ShieldCheck },
  control: { label: "控制", className: "event-control", icon: Settings2 },
  system: { label: "系统", className: "event-system", icon: Zap },
};

const sideEffectMeta: Record<ToolSideEffect, { label: string; className: string }> = {
  ReadOnly: { label: "只读", className: "effect-readonly" },
  Notification: { label: "通知", className: "effect-notification" },
  Stateful: { label: "有状态", className: "effect-stateful" },
  Destructive: { label: "高风险", className: "effect-destructive" },
};

function formatRelative(date: string): string {
  const minutes = Math.max(0, Math.floor((Date.now() - new Date(date).getTime()) / 60_000));
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}

function formatNumber(value: number): string {
  return new Intl.NumberFormat("zh-CN").format(value);
}

function compactId(value: string): string {
  if (value === MAIN_TASK_ID) return "主会话";
  return `${value.slice(0, 8)}…${value.slice(-4)}`;
}

function scopeLabel(task: Pick<TaskSummary, "scope">): string {
  return `${task.scope.kind}:${task.scope.id}`;
}

function applyStreamEvent(snapshot: SystemSnapshot, streamEvent: StreamEvent): SystemSnapshot {
  if (streamEvent.type === "task.updated") {
    return {
      ...snapshot,
      tasks: snapshot.tasks.map((task) =>
        task.taskId === streamEvent.task.taskId ? streamEvent.task : task,
      ),
    };
  }

  if (streamEvent.type === "approval.updated") {
    const existing = snapshot.approvals.some(
      (approval) =>
        approval.approvalRequestEventId === streamEvent.approval.approvalRequestEventId,
    );
    return {
      ...snapshot,
      approvals: existing
        ? snapshot.approvals.map((approval) =>
            approval.approvalRequestEventId ===
            streamEvent.approval.approvalRequestEventId
              ? streamEvent.approval
              : approval,
          )
        : [streamEvent.approval, ...snapshot.approvals],
    };
  }

  if (streamEvent.type === "authorization.requested") {
    // 该通知只用于唤醒刷新；审批 DTO 仍由下方从事件存储重建，不能信任传输层自行声明的
    // 审批状态或权限。
    return snapshot;
  }

  return {
    ...snapshot,
    recentEvents: [streamEvent.event, ...snapshot.recentEvents].slice(0, 24),
  };
}

function AuthScreen({
  api,
  onAuthenticated,
}: {
  api: ReturnType<typeof createKoiApiClient>;
  onAuthenticated: (user: AuthUser) => void;
}) {
  const { t, locale } = useI18n();
  const [mode, setMode] = useState<"login" | "register">("login");
  const [email, setEmail] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const user =
        mode === "register"
          ? await api.register({ email, username, password })
          : await api.login({ email, password });
      onAuthenticated(user);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Unable to authenticate");
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="auth-shell">
      <section className="auth-card">
        <div className="brand-mark">K</div>
        <p className="eyebrow">KOI OPERATIONS</p>
        <h1>{mode === "login" ? t("loginTitle") : t("registerTitle")}</h1>
        <p className="auth-copy">
          {mode === "login"
            ? (locale === "en" ? "Continue with your email and password." : "使用邮箱和密码继续。")
            : (locale === "en" ? "Your username is the stable identity recorded in Koi core events." : "用户名将作为写入 Koi 核心事件的稳定用户标识。")}
        </p>
        <form onSubmit={submit} className="auth-form">
          <label>
            {t("email")}
            <input type="email" value={email} onChange={(event) => setEmail(event.target.value)} required />
          </label>
          {mode === "register" && (
            <label>
              {t("username")}
              <input value={username} onChange={(event) => setUsername(event.target.value)} minLength={3} maxLength={64} required />
            </label>
          )}
          <label>
            {t("password")}
            <input type="password" value={password} onChange={(event) => setPassword(event.target.value)} minLength={12} required />
          </label>
          {error && <p className="auth-error">{error}</p>}
          <button type="submit" className="primary-button auth-submit" disabled={busy}>
            {busy ? "…" : mode === "login" ? t("login") : t("register")}
          </button>
        </form>
        <button type="button" className="auth-switch" onClick={() => { setMode(mode === "login" ? "register" : "login"); setError(null); }}>
          {mode === "login" ? "还没有账户？注册" : "已有账户？登录"}
        </button>
      </section>
    </main>
  );
}

function App() {
  const { locale, setLocale, t } = useI18n();
  const api = useMemo(() => createKoiApiClient(), []);
  const [snapshot, setSnapshot] = useState<SystemSnapshot>(() => createDemoSnapshot());
  const [view, setView] = useState<ViewKey>("overview");
  const [selectedTaskId, setSelectedTaskId] = useState(MAIN_TASK_ID);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [isLive, setIsLive] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [approvalBusy, setApprovalBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [composerOpen, setComposerOpen] = useState(false);
  const [user, setUser] = useState<AuthUser | null>(null);
  const [authChecked, setAuthChecked] = useState(false);

  const pendingApprovals = snapshot.approvals.filter((approval) => approval.status === "Pending");
  const selectedTask =
    snapshot.tasks.find((task) => task.taskId === selectedTaskId) ?? snapshot.tasks[0];

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(null), 3200);
    return () => window.clearTimeout(timer);
  }, [toast]);

  useEffect(() => {
    api.currentUser()
      .then(async (currentUser) => {
        setUser(currentUser);
        setSnapshot(await api.getSnapshot());
        setIsLive(true);
      })
      .catch(() => undefined)
      .finally(() => setAuthChecked(true));
  }, [api]);

  useEffect(() => {
    if (!isLive) return;
    return api.openEventStream(
      undefined,
      (event) => {
        setSnapshot((current) => applyStreamEvent(current, event));
        if (event.type === "authorization.requested") {
          void api.getSnapshot().then(setSnapshot).catch(() => undefined);
        }
        if (
          event.type === "event.appended" &&
          (event.event.kind === "control" || event.event.kind === "approval")
        ) {
          // 生命周期和审批状态由事件投影计算，收到这两类事件后重新读取权威快照。
          void api.getSnapshot().then(setSnapshot).catch(() => undefined);
        }
      },
      () => undefined,
    );
  }, [api, isLive]);

  if (!authChecked) {
    return <main className="auth-shell"><p className="auth-loading">{t("loadingSession")}</p></main>;
  }

  if (!user) {
    return <AuthScreen api={api} onAuthenticated={setUser} />;
  }

  async function refreshSnapshot() {
    if (!isLive) {
      setSnapshot((current) => ({ ...current, generatedAt: new Date().toISOString() }));
      setToast("演示数据已刷新");
      return;
    }

    setRefreshing(true);
    try {
      setSnapshot(await api.getSnapshot());
      setToast("数据已刷新");
    } catch {
      setToast("刷新失败，请检查 API 服务");
    } finally {
      setRefreshing(false);
    }
  }

  async function handleApproval(approval: ApprovalRequest, approved: boolean) {
    setApprovalBusy(approval.approvalRequestEventId);
    if (isLive) {
      try {
        const updated = await api.submitApproval(approval.approvalRequestEventId, { approved });
        setSnapshot((current) => applyStreamEvent(current, { type: "approval.updated", approval: updated }));
        setToast(approved ? "授权已提交，任务将继续运行" : "已拒绝此次操作");
      } catch {
        setToast("审批提交失败，请稍后重试");
      } finally {
        setApprovalBusy(null);
      }
      return;
    }

    setSnapshot((current) => ({
      ...current,
      approvals: current.approvals.map((item) =>
        item.approvalRequestEventId === approval.approvalRequestEventId
          ? { ...item, status: approved ? "Approved" : "Denied" }
          : item,
      ),
      tasks: current.tasks.map((task) =>
        task.taskId === approval.taskId && approved
          ? {
              ...task,
              status: "Running",
              lastEventKind: "tool",
              lastEventSummary: `${approval.toolName} 已获授权，准备执行`,
              updatedAt: new Date().toISOString(),
            }
          : task,
      ),
    }));
    setApprovalBusy(null);
    setToast(approved ? "演示授权已通过，任务继续运行" : "演示审批已拒绝");
  }

  function handleNewTask(task: TaskSummary) {
    setSnapshot((current) => ({ ...current, tasks: [task, ...current.tasks] }));
    setSelectedTaskId(task.taskId);
    setView("tasks");
    setComposerOpen(false);
    setToast("诊断任务已加入队列");
  }

  const navWithCounts: NavItem[] = navIcons.map((item) => ({ ...item, label: t(item.key) })).map((item) =>
    item.key === "approvals" ? { ...item, count: pendingApprovals.length } : item,
  );

  return (
    <div className="app-shell">
      <aside className={`sidebar ${sidebarOpen ? "sidebar-open" : ""}`}>
        <div className="brand-lockup">
          <div className="brand-mark" aria-hidden="true">
            <span />
            <span />
            <span />
          </div>
          <div>
            <div className="brand-name">koi</div>
            <div className="brand-caption">OPS CONSOLE</div>
          </div>
          <button
            className="icon-button sidebar-close"
            onClick={() => setSidebarOpen(false)}
            aria-label="关闭导航"
          >
            <X size={18} />
          </button>
        </div>

        <div className="sidebar-section-label">{t("workspace")}</div>
        <nav className="sidebar-nav" aria-label="主导航">
          {navWithCounts.map((item) => {
            const Icon = item.icon;
            return (
              <button
                className={`nav-item ${view === item.key ? "nav-item-active" : ""}`}
                key={item.key}
                onClick={() => {
                  setView(item.key);
                  setSidebarOpen(false);
                }}
              >
                <Icon size={17} strokeWidth={1.9} />
                <span>{item.label}</span>
                {item.count ? <span className="nav-count">{item.count}</span> : null}
                {view === item.key ? <ChevronRight className="nav-arrow" size={15} /> : null}
              </button>
            );
          })}
        </nav>

        <div className="sidebar-spacer" />

        <div className="sidebar-system-card">
          <div className="system-card-topline">
            <span className="live-dot" />
            <span>系统运行正常</span>
            <MoreHorizontal size={16} />
          </div>
          <div className="system-card-value">99.98%</div>
          <div className="system-card-meta">过去 30 天 Agent 可用性</div>
          <div className="mini-bars" aria-hidden="true">
            {Array.from({ length: 22 }, (_, index) => (
              <span key={index} style={{ height: `${9 + ((index * 13) % 18)}px` }} />
            ))}
          </div>
        </div>

        <div className="profile-row">
          <div className="avatar avatar-coral">{user.username.slice(0, 1).toUpperCase()}</div>
          <div className="profile-copy">
            <strong>{user.username}</strong>
            <span>{t("userWorkspace")}</span>
          </div>
          <ChevronDown size={15} />
        </div>
      </aside>

      {sidebarOpen ? (
        <button className="sidebar-scrim" onClick={() => setSidebarOpen(false)} aria-label="关闭导航" />
      ) : null}

      <main className="main-panel">
        <header className="topbar">
          <div className="topbar-left">
            <button
              className="icon-button mobile-menu"
              onClick={() => setSidebarOpen(true)}
              aria-label="打开导航"
            >
              <Menu size={20} />
            </button>
            <div className="breadcrumb">
              <span>Koi</span>
              <ChevronRight size={14} />
              <strong>{navWithCounts.find((item) => item.key === view)?.label}</strong>
            </div>
          </div>
          <div className="topbar-actions">
            <button
              className={`data-source-pill ${isLive ? "data-source-live" : ""}`}
              disabled
              title={t("live")}
            >
              <span className="source-dot" />
              {isLive ? t("connected") : t("disconnected")}
            </button>
            <button className="icon-button topbar-icon" aria-label="通知">
              <Bell size={18} />
              <span className="notification-dot" />
            </button>
            <div className="topbar-divider" />
            <button className="topbar-avatar" aria-label="账户菜单" onClick={() => void api.logout().finally(() => { setUser(null); setIsLive(false); })}>{user.username.slice(0, 1).toUpperCase()}</button>
            <button className="icon-button topbar-icon" onClick={() => setLocale(locale === "zh-CN" ? "en" : "zh-CN")} aria-label="Language">{t("language")}</button>
          </div>
        </header>

        <div className="content-wrap">
          {view === "overview" ? (
            <OverviewView
              snapshot={snapshot}
              selectedTask={selectedTask}
              pendingApprovals={pendingApprovals}
              onSelectTask={(taskId) => {
                setSelectedTaskId(taskId);
                setView("tasks");
              }}
              onOpenApprovals={() => setView("approvals")}
              onApproval={handleApproval}
              approvalBusy={approvalBusy}
              onRefresh={refreshSnapshot}
              refreshing={refreshing}
              onNewTask={() => setComposerOpen(true)}
            />
          ) : null}
          {view === "tasks" ? (
            <TasksView
              tasks={snapshot.tasks}
              selectedTaskId={selectedTaskId}
              recentEvents={snapshot.recentEvents}
              onSelectTask={setSelectedTaskId}
              onNewTask={() => setComposerOpen(true)}
              onRefresh={refreshSnapshot}
              refreshing={refreshing}
            />
          ) : null}
          {view === "approvals" ? (
            <ApprovalsView
              approvals={snapshot.approvals}
              tasks={snapshot.tasks}
              onApproval={handleApproval}
              approvalBusy={approvalBusy}
              onRefresh={refreshSnapshot}
              refreshing={refreshing}
            />
          ) : null}
          {view === "tools" ? <ToolsView tools={snapshot.tools} /> : null}
          {view === "audit" ? (
            <AuditView
              events={snapshot.recentEvents}
              tasks={snapshot.tasks}
              onSelectTask={(taskId) => {
                setSelectedTaskId(taskId);
                setView("tasks");
              }}
            />
          ) : null}
        </div>
      </main>

      {composerOpen ? (
        <TaskComposer
          isLive={isLive}
          api={api}
          onClose={() => setComposerOpen(false)}
          onCreated={handleNewTask}
          onToast={setToast}
        />
      ) : null}

      {toast ? (
        <div className="toast" role="status">
          <CheckCircle2 size={17} />
          <span>{toast}</span>
        </div>
      ) : null}
    </div>
  );
}

interface OverviewProps {
  snapshot: SystemSnapshot;
  selectedTask?: TaskSummary;
  pendingApprovals: ApprovalRequest[];
  onSelectTask: (taskId: string) => void;
  onOpenApprovals: () => void;
  onApproval: (approval: ApprovalRequest, approved: boolean) => void;
  approvalBusy: string | null;
  onRefresh: () => void;
  refreshing: boolean;
  onNewTask: () => void;
}

function OverviewView({
  snapshot,
  selectedTask,
  pendingApprovals,
  onSelectTask,
  onOpenApprovals,
  onApproval,
  approvalBusy,
  onRefresh,
  refreshing,
  onNewTask,
}: OverviewProps) {
  const runningCount = snapshot.tasks.filter((task) =>
    ["Running", "Queued", "WaitingApproval"].includes(task.status),
  ).length;
  const completedCount = snapshot.tasks.filter((task) => task.status === "Completed").length;
  const errorCount = snapshot.tasks.filter((task) => task.status === "Failed").length;
  const budgetPercent = Math.round((snapshot.usage.monthSpentUsd / snapshot.usage.monthlyBudgetUsd) * 100);

  return (
    <>
      <PageHeader
        eyebrow="THURSDAY · 03 SEP 2026"
        title="今天，系统正在替你盯住异常。"
        description="从实时任务到每一次授权决定，把运维现场收拢到一个清晰的工作台。"
        action={
          <>
            <button className="button button-secondary" onClick={onRefresh} disabled={refreshing}>
              <RefreshCw className={refreshing ? "spin" : ""} size={16} />
              刷新
            </button>
            <button className="button button-primary" onClick={onNewTask}>
              <Plus size={17} />
              新建诊断
            </button>
          </>
        }
      />

      <div className="metric-grid">
        <MetricCard
          label="活跃任务"
          value={String(runningCount)}
          detail="较昨日 +2"
          icon={Activity}
          accent="teal"
          trend="up"
        />
        <MetricCard
          label="待处理审批"
          value={String(pendingApprovals.length).padStart(2, "0")}
          detail="需要 Operator 决定"
          icon={ShieldAlert}
          accent="amber"
          attention={pendingApprovals.length > 0}
        />
        <MetricCard
          label="今日完成"
          value={String(completedCount + 11)}
          detail="成功率 96.4%"
          icon={CheckCircle2}
          accent="blue"
          trend="up"
        />
        <MetricCard
          label="异常任务"
          value={String(errorCount).padStart(2, "0")}
          detail="过去 24 小时"
          icon={AlertTriangle}
          accent="coral"
          attention={errorCount > 0}
        />
      </div>

      <div className="primary-grid">
        <section className="card activity-card">
          <div className="section-heading">
            <div>
              <div className="section-kicker"><span className="live-dot" />实时事件流</div>
              <h2>现场正在发生什么</h2>
            </div>
            <button className="text-button" onClick={() => onSelectTask(selectedTask?.taskId ?? MAIN_TASK_ID)}>
              查看主会话 <ArrowUpRight size={15} />
            </button>
          </div>
          <div className="activity-list">
            {snapshot.recentEvents.slice(0, 5).map((item) => (
              <EventRow
                event={item}
                key={item.id}
                onClick={() => onSelectTask(item.taskId)}
                compact
              />
            ))}
          </div>
          <div className="stream-footer">
            <div className="stream-status"><Wifi size={14} /> 事件流已连接</div>
            <span>最后更新 {formatRelative(snapshot.generatedAt)}</span>
          </div>
        </section>

        <section className="card approval-card">
          <div className="section-heading">
            <div>
              <div className="section-kicker section-kicker-amber"><span className="pulse-dot" />需要你的决定</div>
              <h2>授权收件箱</h2>
            </div>
            <button className="icon-button subtle-button" onClick={onOpenApprovals} aria-label="打开全部审批">
              <ArrowUpRight size={17} />
            </button>
          </div>
          {pendingApprovals.length ? (
            <div className="approval-stack">
              {pendingApprovals.slice(0, 2).map((approval) => (
                <ApprovalCard
                  approval={approval}
                  task={snapshot.tasks.find((task) => task.taskId === approval.taskId)}
                  key={approval.approvalRequestEventId}
                  onApproval={onApproval}
                  busy={approvalBusy === approval.approvalRequestEventId}
                />
              ))}
            </div>
          ) : (
            <EmptyState icon={CheckCircle2} title="收件箱是空的" description="当前没有等待确认的高风险操作。" />
          )}
          <button className="full-link-button" onClick={onOpenApprovals}>
            打开审批中心 <ChevronRight size={16} />
          </button>
        </section>
      </div>

      <div className="secondary-grid">
        <section className="card usage-card">
          <div className="section-heading">
            <div>
              <div className="section-kicker"><Gauge size={14} />资源使用</div>
              <h2>模型调用趋势</h2>
            </div>
            <div className="period-pill">本周 <ChevronDown size={14} /></div>
          </div>
          <UsageChart data={snapshot.usage.daily} />
          <div className="usage-footer">
            <div>
              <span className="legend-dot legend-input" />输入 {formatNumber(snapshot.usage.inputTokensToday)} tokens
            </div>
            <div>
              <span className="legend-dot legend-output" />输出 {formatNumber(snapshot.usage.outputTokensToday)} tokens
            </div>
            <div className="budget-copy">预算已用 {budgetPercent}%</div>
          </div>
        </section>

        <section className="card health-card">
          <div className="section-heading">
            <div>
              <div className="section-kicker"><Server size={14} />运行状况</div>
              <h2>关键依赖</h2>
            </div>
            <span className="health-badge"><span className="live-dot" />全部正常</span>
          </div>
          <div className="health-list">
            <HealthRow icon={Globe2} label="API gateway" status={snapshot.health.api} latency="42 ms" />
            <HealthRow icon={Database} label="Event store" status={snapshot.health.eventStore} latency="18 ms" />
            <HealthRow icon={Bot} label="Model provider" status={snapshot.health.modelProvider} latency="860 ms" />
          </div>
          <div className="health-footer">
            <span>心跳 {formatRelative(snapshot.health.lastHeartbeatAt)}</span>
            <span className="muted-chip">v0.1.0 · Rust core</span>
          </div>
        </section>
      </div>

      <section className="card task-card">
        <div className="section-heading section-heading-table">
          <div>
            <div className="section-kicker"><Command size={14} />任务现场</div>
            <h2>正在运行的任务</h2>
          </div>
          <button className="text-button" onClick={() => onSelectTask(MAIN_TASK_ID)}>
            查看全部 <ArrowUpRight size={15} />
          </button>
        </div>
        <TaskTable
          tasks={snapshot.tasks.slice(0, 4)}
          selectedTaskId={selectedTask?.taskId}
          onSelectTask={onSelectTask}
        />
      </section>
    </>
  );
}

function PageHeader({
  eyebrow,
  title,
  description,
  action,
}: {
  eyebrow: string;
  title: string;
  description: string;
  action?: React.ReactNode;
}) {
  return (
    <div className="page-header">
      <div>
        <div className="page-eyebrow">{eyebrow}</div>
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      {action ? <div className="page-actions">{action}</div> : null}
    </div>
  );
}

function MetricCard({
  label,
  value,
  detail,
  icon: Icon,
  accent,
  trend,
  attention,
}: {
  label: string;
  value: string;
  detail: string;
  icon: LucideIcon;
  accent: string;
  trend?: "up";
  attention?: boolean;
}) {
  return (
    <div className={`metric-card metric-${accent} ${attention ? "metric-attention" : ""}`}>
      <div className="metric-topline">
        <span>{label}</span>
        <span className="metric-icon"><Icon size={17} /></span>
      </div>
      <div className="metric-value-row">
        <strong>{value}</strong>
        {trend ? <span className="trend-badge"><ArrowUpRight size={13} /> 12%</span> : null}
      </div>
      <div className="metric-detail">{detail}</div>
    </div>
  );
}

function EventRow({ event, onClick, compact = false }: { event: TaskEvent; onClick?: () => void; compact?: boolean }) {
  const meta = eventKindMeta[event.kind] ?? eventKindMeta.system;
  const Icon = meta.icon;
  return (
    <button className={`event-row ${compact ? "event-row-compact" : ""}`} onClick={onClick}>
      <span className={`event-icon ${meta.className}`}><Icon size={16} /></span>
      <span className="event-row-copy">
        <span className="event-row-title"><strong>{event.title}</strong><small>{formatRelative(event.occurredAt)}</small></span>
        <span className="event-row-summary">{event.summary}</span>
      </span>
      <span className="event-row-source">{event.source}</span>
    </button>
  );
}

function ApprovalCard({
  approval,
  task,
  onApproval,
  busy,
}: {
  approval: ApprovalRequest;
  task?: TaskSummary;
  onApproval: (approval: ApprovalRequest, approved: boolean) => void;
  busy: boolean;
}) {
  return (
    <div className="approval-item">
      <div className="approval-item-head">
        <span className="risk-label"><ShieldAlert size={14} />需要确认</span>
        <span className="approval-time">{formatRelative(approval.requestedAt)}</span>
      </div>
      <div className="approval-title-row">
        <strong>{approval.toolName}</strong>
        <PermissionBadge level={approval.requiredPermission} />
      </div>
      <p>{approval.toolDescription}</p>
      <div className="approval-context">
        <span>{approval.scope.kind}:{approval.scope.id}</span>
        <code>{approval.argumentsHash}</code>
      </div>
      <div className="approval-actions">
        <button className="button button-danger-ghost" onClick={() => onApproval(approval, false)} disabled={busy}>
          <X size={15} />拒绝
        </button>
        <button className="button button-approve" onClick={() => onApproval(approval, true)} disabled={busy}>
          {busy ? <LoaderCircle className="spin" size={15} /> : <Check size={15} />}批准操作
        </button>
      </div>
      {task ? <div className="approval-task-ref"><Command size={12} /> {task.title}</div> : null}
    </div>
  );
}

function PermissionBadge({ level }: { level: PermissionLevel }) {
  const meta = permissionMeta[level];
  return <span className={`permission-badge ${meta.className}`}>{meta.label}</span>;
}

function UsageChart({ data }: { data: SystemSnapshot["usage"]["daily"] }) {
  const max = Math.max(...data.map((item) => item.input + item.output));
  const points = data
    .map((item, index) => {
      const x = 18 + (index * 264) / Math.max(1, data.length - 1);
      const y = 130 - ((item.input + item.output) / max) * 92;
      return `${x},${y}`;
    })
    .join(" ");
  const areaPoints = `18,142 ${points} 282,142`;
  return (
    <div className="usage-chart-wrap">
      <svg className="usage-chart" viewBox="0 0 300 158" role="img" aria-label="本周模型调用趋势图">
        <defs>
          <linearGradient id="usageFill" x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stopColor="#1d9d8d" stopOpacity="0.25" />
            <stop offset="100%" stopColor="#1d9d8d" stopOpacity="0" />
          </linearGradient>
        </defs>
        {[34, 70, 106, 142].map((y) => <line className="chart-gridline" key={y} x1="18" x2="282" y1={y} y2={y} />)}
        <polygon points={areaPoints} fill="url(#usageFill)" />
        <polyline points={points} fill="none" stroke="#159786" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round" />
        {data.map((item, index) => {
          const x = 18 + (index * 264) / Math.max(1, data.length - 1);
          const y = 130 - ((item.input + item.output) / max) * 92;
          return <circle className="chart-point" cx={x} cy={y} key={item.label} r="4" />;
        })}
      </svg>
      <div className="chart-labels">{data.map((item) => <span key={item.label}>{item.label}</span>)}</div>
    </div>
  );
}

function HealthRow({
  icon: Icon,
  label,
  status,
  latency,
}: {
  icon: LucideIcon;
  label: string;
  status: HealthStatusValue;
  latency: string;
}) {
  return (
    <div className="health-row">
      <span className="health-icon"><Icon size={16} /></span>
      <strong>{label}</strong>
      <span className={`health-state health-${status}`}><span />{status === "healthy" ? "正常" : status === "degraded" ? "降级" : "离线"}</span>
      <span className="health-latency">{latency}</span>
    </div>
  );
}

type HealthStatusValue = "healthy" | "degraded" | "offline";

function TaskTable({
  tasks,
  selectedTaskId,
  onSelectTask,
}: {
  tasks: TaskSummary[];
  selectedTaskId?: string;
  onSelectTask: (taskId: string) => void;
}) {
  if (!tasks.length) {
    return <EmptyState icon={Inbox} title="还没有任务" description="创建一个诊断任务，Agent 会在这里展示完整事件链。" />;
  }
  return (
    <div className="table-scroll">
      <table className="task-table">
        <thead>
          <tr><th>任务</th><th>状态</th><th>来源 / 范围</th><th>最后事件</th><th>用量</th><th /></tr>
        </thead>
        <tbody>
          {tasks.map((task) => {
            const meta = statusMeta[task.status];
            const StatusIcon = meta.icon;
            return (
              <tr
                className={selectedTaskId === task.taskId ? "task-row-selected" : ""}
                key={task.taskId}
                onClick={() => onSelectTask(task.taskId)}
                onKeyDown={(event) => { if (event.key === "Enter") onSelectTask(task.taskId); }}
                tabIndex={0}
              >
                <td><div className="task-name-cell"><span className={`task-avatar ${task.isMain ? "task-avatar-main" : ""}`}><Bot size={15} /></span><span><strong>{task.title}</strong><small>{compactId(task.taskId)}</small></span></div></td>
                <td><span className={`status-badge ${meta.className}`}><StatusIcon size={13} className={task.status === "Running" ? "spin-slow" : ""} />{meta.label}</span></td>
                <td><div className="source-cell"><strong>{task.source}</strong><small>{scopeLabel(task)}</small></div></td>
                <td><div className="last-event-cell"><span>{task.lastEventSummary}</span><small>{formatRelative(task.updatedAt)}</small></div></td>
                <td><span className="token-cell">{formatNumber(task.usage.inputTokens + task.usage.outputTokens)}</span></td>
                <td><ChevronRight className="row-chevron" size={17} /></td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function EmptyState({ icon: Icon, title, description }: { icon: LucideIcon; title: string; description: string }) {
  return <div className="empty-state"><span className="empty-icon"><Icon size={21} /></span><strong>{title}</strong><p>{description}</p></div>;
}

function TasksView({
  tasks,
  selectedTaskId,
  recentEvents,
  onSelectTask,
  onNewTask,
  onRefresh,
  refreshing,
}: {
  tasks: TaskSummary[];
  selectedTaskId: string;
  recentEvents: TaskEvent[];
  onSelectTask: (taskId: string) => void;
  onNewTask: () => void;
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const [statusFilter, setStatusFilter] = useState<TaskStatus | "all">("all");
  const [query, setQuery] = useState("");
  const [showFilters, setShowFilters] = useState(false);
  const visibleTasks = tasks.filter((task) => {
    const matchesStatus = statusFilter === "all" || task.status === statusFilter;
    const normalized = query.trim().toLowerCase();
    const matchesQuery = !normalized || `${task.title} ${task.source} ${scopeLabel(task)}`.toLowerCase().includes(normalized);
    return matchesStatus && matchesQuery;
  });
  const selected = tasks.find((task) => task.taskId === selectedTaskId) ?? tasks[0];
  const selectedEvents = recentEvents.filter((event) => event.taskId === selected?.taskId);

  return (
    <>
      <PageHeader
        eyebrow="TASK CONTROL"
        title="任务队列"
        description="追踪每一个 Agent 任务的状态、上下文来源与可审计事件。"
        action={<><button className="button button-secondary" onClick={onRefresh} disabled={refreshing}><RefreshCw className={refreshing ? "spin" : ""} size={16} />刷新</button><button className="button button-primary" onClick={onNewTask}><Plus size={17} />新建诊断</button></>}
      />
      <section className="card filter-card">
        <div className="filter-toolbar">
          <div className="search-field"><Search size={16} /><input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索任务、来源或范围" aria-label="搜索任务" /></div>
          <div className="filter-actions"><button className={`button button-secondary ${showFilters ? "button-selected" : ""}`} onClick={() => setShowFilters((value) => !value)}><Filter size={16} />筛选</button><span className="result-count">显示 {visibleTasks.length} / {tasks.length}</span></div>
        </div>
        {showFilters ? <div className="filter-options">{(["all", "Running", "WaitingApproval", "Paused", "Completed", "Failed"] as const).map((value) => <button key={value} className={`filter-chip ${statusFilter === value ? "filter-chip-active" : ""}`} onClick={() => setStatusFilter(value)}>{value === "all" ? "全部" : statusMeta[value].label}</button>)}</div> : null}
        <TaskTable tasks={visibleTasks} selectedTaskId={selectedTaskId} onSelectTask={onSelectTask} />
      </section>
      {selected ? <TaskInspector task={selected} events={selectedEvents} /> : null}
    </>
  );
}

function TaskInspector({ task, events }: { task: TaskSummary; events: TaskEvent[] }) {
  const meta = statusMeta[task.status];
  const StatusIcon = meta.icon;
  return (
    <section className="card inspector-card">
      <div className="inspector-top"><div><div className="section-kicker"><Command size={14} />任务详情</div><h2>{task.title}</h2></div><span className={`status-badge ${meta.className}`}><StatusIcon size={13} />{meta.label}</span></div>
      <div className="inspector-grid"><div><span>任务 ID</span><strong>{compactId(task.taskId)}</strong></div><div><span>输入来源</span><strong>{task.source} · {scopeLabel(task)}</strong></div><div><span>事件总数</span><strong>{task.eventCount} 条</strong></div><div><span>最低控制权限</span><strong><PermissionBadge level={task.minimumControlPermission} /></strong></div></div>
      <div className="inspector-events"><div className="subheading">最近事件</div>{events.length ? events.slice(0, 3).map((event) => <EventRow event={event} key={event.id} />) : <p className="muted-copy">当前演示数据没有该任务的局部事件，切换到实时 API 后会显示完整事件流。</p>}</div>
    </section>
  );
}

function ApprovalsView({
  approvals,
  tasks,
  onApproval,
  approvalBusy,
  onRefresh,
  refreshing,
}: {
  approvals: ApprovalRequest[];
  tasks: TaskSummary[];
  onApproval: (approval: ApprovalRequest, approved: boolean) => void;
  approvalBusy: string | null;
  onRefresh: () => void;
  refreshing: boolean;
}) {
  const pending = approvals.filter((item) => item.status === "Pending");
  const history = approvals.filter((item) => item.status !== "Pending");
  return (
    <>
      <PageHeader eyebrow="AUTHORIZATION CENTER" title="审批中心" description="每一次高风险工具调用都必须有清晰的权限证据与明确的人工决定。" action={<button className="button button-secondary" onClick={onRefresh} disabled={refreshing}><RefreshCw className={refreshing ? "spin" : ""} size={16} />刷新</button>} />
      <div className="approval-page-grid"><section className="card approval-inbox"><div className="section-heading"><div><div className="section-kicker section-kicker-amber"><ShieldAlert size={14} />待处理</div><h2>{pending.length ? `${pending.length} 个请求等待决定` : "没有待处理请求"}</h2></div><span className="inbox-number">{String(pending.length).padStart(2, "0")}</span></div>{pending.length ? <div className="approval-page-stack">{pending.map((approval) => <ApprovalCard approval={approval} task={tasks.find((task) => task.taskId === approval.taskId)} key={approval.approvalRequestEventId} onApproval={onApproval} busy={approvalBusy === approval.approvalRequestEventId} />)}</div> : <EmptyState icon={ShieldCheck} title="全部处理完毕" description="新的授权请求会实时出现在这里。" />}</section><section className="card approval-history"><div className="section-heading"><div><div className="section-kicker"><FileClock size={14} />审计记录</div><h2>最近决定</h2></div><span className="muted-chip">仅显示本次会话</span></div>{history.length ? <div className="history-list">{history.map((approval) => <ApprovalHistoryRow approval={approval} key={approval.approvalRequestEventId} />)}</div> : <EmptyState icon={FileClock} title="暂无审批记录" description="处理过的请求会保留在这里。" />}</section></div>
    </>
  );
}

function ApprovalHistoryRow({ approval }: { approval: ApprovalRequest }) {
  const approved = approval.status === "Approved";
  return <div className="history-row"><span className={`history-icon ${approved ? "history-approved" : "history-denied"}`}>{approved ? <Check size={15} /> : <X size={15} />}</span><div><strong>{approval.toolName}</strong><span>{approval.scope.kind}:{approval.scope.id} · {approval.requester}</span></div><div className="history-end"><span className={`history-status ${approved ? "history-status-approved" : "history-status-denied"}`}>{approved ? "已批准" : approval.status === "Expired" ? "已过期" : "已拒绝"}</span><small>{formatRelative(approval.requestedAt)}</small></div></div>;
}

function ToolsView({ tools }: { tools: ToolDefinition[] }) {
  const [query, setQuery] = useState("");
  const [effect, setEffect] = useState<ToolSideEffect | "all">("all");
  const visible = tools.filter((tool) => {
    const matchesQuery = !query.trim() || `${tool.name} ${tool.description}`.toLowerCase().includes(query.toLowerCase());
    return matchesQuery && (effect === "all" || tool.sideEffect === effect);
  });
  return (
    <>
      <PageHeader eyebrow="TOOL CATALOG" title="工具目录" description="工具定义来自 Rust 核心注册表，Web 端只负责展示风险与权限边界。" action={<div className="catalog-summary"><span><strong>{tools.length}</strong> 个已注册工具</span><span><span className="live-dot" />策略默认 fail-closed</span></div>} />
      <section className="card filter-card"><div className="filter-toolbar"><div className="search-field"><Search size={16} /><input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索工具名称或说明" aria-label="搜索工具" /></div><div className="filter-actions"><select className="native-select" value={effect} onChange={(event) => setEffect(event.target.value as ToolSideEffect | "all")} aria-label="按副作用筛选"><option value="all">全部副作用</option><option value="ReadOnly">只读</option><option value="Notification">通知</option><option value="Stateful">有状态</option><option value="Destructive">高风险</option></select></div></div><div className="tool-grid">{visible.map((tool) => <ToolCard tool={tool} key={tool.name} />)}</div></section>
    </>
  );
}

function ToolCard({ tool }: { tool: ToolDefinition }) {
  const effect = sideEffectMeta[tool.sideEffect];
  return <article className={`tool-item ${tool.sideEffect === "Destructive" ? "tool-item-risk" : ""}`}><div className="tool-item-head"><span className="tool-symbol"><Terminal size={17} /></span><span className={`effect-badge ${effect.className}`}>{effect.label}</span></div><h3>{tool.name}</h3><p>{tool.description}</p><div className="tool-item-footer"><PermissionBadge level={tool.requiredPermission} /><span>超时 {tool.timeoutMs / 1000}s</span><span className={tool.modelVisible ? "visible-label" : "hidden-label"}>{tool.modelVisible ? "模型可见" : "模型隐藏"}</span></div></article>;
}

function AuditView({ events, tasks, onSelectTask }: { events: TaskEvent[]; tasks: TaskSummary[]; onSelectTask: (taskId: string) => void }) {
  const [kind, setKind] = useState<EventKind | "all">("all");
  const visible = events.filter((event) => kind === "all" || event.kind === kind);
  return (
    <>
      <PageHeader eyebrow="EVENT LEDGER" title="事件审计" description="事件流是事实来源；所有任务状态、模型输出与工具授权都可以追溯。" action={<div className="audit-live"><span className="live-dot" />实时监听中</div>} />
      <section className="card audit-card"><div className="audit-toolbar"><div className="audit-intro"><span className="audit-total">{events.length}</span><div><strong>最近事件</strong><span>按记录时间倒序排列</span></div></div><div className="audit-filters">{(["all", "ingress", "model", "tool", "approval", "control", "system"] as const).map((value) => <button key={value} className={`filter-chip ${kind === value ? "filter-chip-active" : ""}`} onClick={() => setKind(value)}>{value === "all" ? "全部" : eventKindMeta[value].label}</button>)}</div></div><div className="audit-list">{visible.map((event) => <AuditEventRow event={event} task={tasks.find((task) => task.taskId === event.taskId)} key={event.id} onSelectTask={onSelectTask} />)}</div></section>
    </>
  );
}

function AuditEventRow({ event, task, onSelectTask }: { event: TaskEvent; task?: TaskSummary; onSelectTask: (taskId: string) => void }) {
  const meta = eventKindMeta[event.kind] ?? eventKindMeta.system;
  const Icon = meta.icon;
  return <button className="audit-event-row" onClick={() => onSelectTask(event.taskId)}><span className={`audit-event-icon ${meta.className}`}><Icon size={16} /></span><span className="audit-event-main"><span><strong>{event.title}</strong><small>{formatRelative(event.occurredAt)} · seq {event.sequence}</small></span><p>{event.summary}</p></span><span className="audit-event-task">{task?.title ?? compactId(event.taskId)}</span><PermissionBadge level={event.permission} /><ChevronRight className="row-chevron" size={17} /></button>;
}

function TaskComposer({
  isLive,
  api,
  onClose,
  onCreated,
  onToast,
}: {
  isLive: boolean;
  api: ReturnType<typeof createKoiApiClient>;
  onClose: () => void;
  onCreated: (task: TaskSummary) => void;
  onToast: (message: string) => void;
}) {
  const [message, setMessage] = useState("");
  const [scope, setScope] = useState("service:order-api");
  const [busy, setBusy] = useState(false);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    if (!message.trim()) return;
    const [kind = "service", id = "order-api"] = scope.split(":");
    setBusy(true);
    if (isLive) {
      try {
        const created = await api.createTask({ message: message.trim(), scope: { kind, id } });
        onCreated(created);
      } catch {
        onToast("任务创建失败，请检查 API 服务");
      } finally {
        setBusy(false);
      }
      return;
    }

    const now = new Date().toISOString();
    onCreated({
      taskId: window.crypto.randomUUID(),
      isMain: false,
      title: message.trim().slice(0, 28),
      status: "Queued",
      source: "web",
      scope: { kind, id },
      startedAt: now,
      updatedAt: now,
      lastEventKind: "ingress",
      lastEventSummary: "Web 控制台已提交诊断请求，等待主会话接管",
      minimumControlPermission: "User",
      usage: { inputTokens: 0, outputTokens: 0, cachedInputTokens: 0, reasoningTokens: 0 },
      eventCount: 1,
    });
    setBusy(false);
  }

  return <div className="modal-layer" role="presentation"><button className="modal-backdrop" onClick={onClose} aria-label="关闭新建任务" /><form className="composer-modal" onSubmit={submit}><div className="modal-head"><div><div className="section-kicker"><Sparkles size={14} />新建诊断</div><h2>把现场交给 Koi</h2></div><button type="button" className="icon-button subtle-button" onClick={onClose} aria-label="关闭"><X size={18} /></button></div><p className="modal-copy">描述你观察到的现象，Agent 会携带来源与范围进入可审计的任务流。</p><label className="field-label" htmlFor="task-message">问题描述</label><textarea id="task-message" autoFocus value={message} onChange={(event) => setMessage(event.target.value)} placeholder="例如：检查 order-api 最近 10 分钟的 5xx 与连接池状态" rows={4} /><label className="field-label" htmlFor="task-scope">作用域</label><div className="scope-input"><span>scope</span><input id="task-scope" value={scope} onChange={(event) => setScope(event.target.value)} /></div><div className="modal-foot"><span><ShieldCheck size={14} />当前身份：Operator</span><button className="button button-primary" type="submit" disabled={busy || !message.trim()}>{busy ? <LoaderCircle className="spin" size={16} /> : <Play size={16} />}开始诊断</button></div></form></div>;
}

export default App;
