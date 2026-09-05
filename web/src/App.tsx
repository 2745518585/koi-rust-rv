import { useEffect, useMemo, useRef, useState } from "react";
import { CheckCircle2, RefreshCw } from "lucide-react";
import { createKoiApiClient } from "./api/client";
import type { AuthUser } from "./api/client";
import { createEmptySnapshot, MAIN_TASK_ID } from "./api/snapshot";
import type {
  ApprovalRequest,
  PermissionLevel,
  StreamEvent,
  SystemSnapshot,
  TaskSummary,
} from "./api/types";
import { useI18n } from "./i18n";
import type { ViewKey } from "./lib/ui";
import { suggestedPermissionOptions } from "./lib/ui";
import { AuthScreen } from "./components/AuthScreen";
import { Sidebar } from "./components/Sidebar";
import { Conversation } from "./components/Conversation";
import { TaskComposerModal } from "./components/TaskComposerModal";
import { ApprovalsView } from "./views/ApprovalsView";
import { ToolsView } from "./views/ToolsView";
import { AuditView } from "./views/AuditView";

/** SSE 只负责更新本地快照；审批/控制事件仍会触发一次权威快照重读。 */
function applyStreamEvent(snapshot: SystemSnapshot, streamEvent: StreamEvent): SystemSnapshot {
  if (streamEvent.type === "task.updated") {
    const existing = snapshot.tasks.some((task) => task.taskId === streamEvent.task.taskId);
    return {
      ...snapshot,
      tasks: existing
        ? snapshot.tasks.map((task) =>
            task.taskId === streamEvent.task.taskId ? streamEvent.task : task,
          )
        : [streamEvent.task, ...snapshot.tasks],
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
            approval.approvalRequestEventId === streamEvent.approval.approvalRequestEventId
              ? streamEvent.approval
              : approval,
          )
        : [streamEvent.approval, ...snapshot.approvals],
    };
  }

  if (streamEvent.type === "authorization.requested") {
    // 该通知只用于唤醒刷新；审批 DTO 仍由事件存储重建，不能信任传输层自行声明的状态。
    return snapshot;
  }

  return {
    ...snapshot,
    recentEvents: [streamEvent.event, ...snapshot.recentEvents].slice(0, 24),
  };
}

export default function App() {
  const { t } = useI18n();
  const api = useMemo(() => createKoiApiClient(), []);
  const [snapshot, setSnapshot] = useState<SystemSnapshot>(() => createEmptySnapshot());
  const [view, setView] = useState<ViewKey>("conversation");
  const [selectedTaskId, setSelectedTaskId] = useState(MAIN_TASK_ID);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [isLive, setIsLive] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [approvalBusy, setApprovalBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [composerOpen, setComposerOpen] = useState(false);
  const [user, setUser] = useState<AuthUser | null>(null);
  const [suggestedPermission, setSuggestedPermission] = useState<PermissionLevel>("User");
  const [authChecked, setAuthChecked] = useState(false);
  const [apiError, setApiError] = useState<string | null>(null);
  const [eventsRevision, setEventsRevision] = useState(0);

  // 快照加载只在登录状态变化时触发；选中会话通过 ref 读取，避免切换会话时整页重载。
  const selectedTaskIdRef = useRef(selectedTaskId);
  selectedTaskIdRef.current = selectedTaskId;

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
      .then(setUser)
      .catch(() => undefined)
      .finally(() => setAuthChecked(true));
  }, [api]);

  useEffect(() => {
    if (!user) return;
    const options = suggestedPermissionOptions(user.permission);
    setSuggestedPermission((current) =>
      options.includes(current) ? current : options[0] ?? "User",
    );
  }, [user]);

  useEffect(() => {
    if (!user) return;
    let active = true;
    api.getSnapshot()
      .then((next) => {
        if (!active) return;
        setSnapshot(next);
        setIsLive(true);
        setApiError(null);
        if (!next.tasks.some((task) => task.taskId === selectedTaskIdRef.current)) {
          setSelectedTaskId(
            next.tasks.find((task) => task.isMain)?.taskId ?? next.tasks[0]?.taskId ?? MAIN_TASK_ID,
          );
        }
      })
      .catch(() => {
        if (!active) return;
        setIsLive(false);
        setApiError(t("connBroken"));
      });
    return () => {
      active = false;
    };
  }, [api, user, t]);

  useEffect(() => {
    if (!isLive) return;
    return api.openEventStream(
      undefined,
      (event) => {
        setSnapshot((current) => applyStreamEvent(current, event));
        if (event.type === "event.appended" && event.event.taskId === selectedTaskIdRef.current) {
          // 事件流只负责通知变化，当前会话详情仍从事件存储重读，避免本地状态漏掉
          // 模型完成、工具结果或子任务回传等连续事件。
          setEventsRevision((current) => current + 1);
        }
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

  async function refreshSnapshot() {
    setRefreshing(true);
    try {
      setSnapshot(await api.getSnapshot());
      setIsLive(true);
      setApiError(null);
      setToast(t("refreshed"));
    } catch {
      setIsLive(false);
      setApiError(t("connBroken"));
      setToast(t("refreshFailed"));
    } finally {
      setRefreshing(false);
    }
  }

  async function handleApproval(approval: ApprovalRequest, approved: boolean) {
    setApprovalBusy(approval.approvalRequestEventId);
    if (!isLive) {
      setApprovalBusy(null);
      setToast(t("backendOffline"));
      return;
    }
    try {
      const updated = await api.submitApproval(approval.approvalRequestEventId, {
        approved,
        suggestedPermission,
      });
      setSnapshot((current) => applyStreamEvent(current, { type: "approval.updated", approval: updated }));
      setToast(approved ? t("approvalSubmitted") : t("approvalDenied"));
    } catch {
      setToast(t("approvalFailed"));
    } finally {
      setApprovalBusy(null);
    }
  }

  function handleNewTask(task: TaskSummary) {
    setSnapshot((current) => ({
      ...current,
      tasks: current.tasks.some((item) => item.taskId === task.taskId)
        ? current.tasks.map((item) => item.taskId === task.taskId ? task : item)
        : [task, ...current.tasks],
    }));
    setSelectedTaskId(task.taskId);
    setView("conversation");
    setComposerOpen(false);
    setToast(t("taskCreated"));
  }

  function handleTaskUpdated(task: TaskSummary) {
    setSnapshot((current) => ({
      ...current,
      tasks: current.tasks.map((item) => item.taskId === task.taskId ? task : item),
    }));
  }

  function handleTaskDeleted(taskId: string) {
    setSnapshot((current) => ({
      ...current,
      tasks: current.tasks.filter((task) => task.taskId !== taskId),
      recentEvents: current.recentEvents.filter((event) => event.taskId !== taskId),
      approvals: current.approvals.filter((approval) => approval.taskId !== taskId),
    }));
    setSelectedTaskId(MAIN_TASK_ID);
  }

  function openTask(taskId: string) {
    setSelectedTaskId(taskId);
    setView("conversation");
    setSidebarOpen(false);
  }

  if (!authChecked) {
    return <main className="auth-shell"><p className="auth-loading">{t("loadingSession")}</p></main>;
  }

  if (!user) {
    return <AuthScreen api={api} onAuthenticated={setUser} />;
  }

  return (
    <div className="app">
      <Sidebar
        tasks={snapshot.tasks}
        selectedTaskId={selectedTask?.taskId ?? ""}
        view={view}
        pendingApprovals={pendingApprovals.length}
        user={user}
        isLive={isLive}
        open={sidebarOpen}
        onSelectTask={openTask}
        onNavigate={(next) => {
          setView(next);
          setSidebarOpen(false);
        }}
        onNewTask={() => setComposerOpen(true)}
        onLogout={() => {
          void api.logout().finally(() => {
            setUser(null);
            setIsLive(false);
          });
        }}
        onClose={() => setSidebarOpen(false)}
      />

      {sidebarOpen ? (
        <button className="side-scrim" onClick={() => setSidebarOpen(false)} aria-label="关闭导航" />
      ) : null}

      <main className="main">
        {apiError ? (
          <div className="api-error" role="alert">
            <strong>{t("disconnectedTitle")}</strong>
            <p>{apiError}</p>
            <button className="button button-secondary" onClick={() => void refreshSnapshot()}>
              <RefreshCw size={14} />
              {t("retry")}
            </button>
          </div>
        ) : null}

        {view === "conversation" ? (
          <Conversation
            api={api}
            task={selectedTask}
            eventsRevision={eventsRevision}
            isLive={isLive}
            approvals={snapshot.approvals}
            models={snapshot.models}
            maximumPermission={user.permission}
            suggestedPermission={suggestedPermission}
            onPermissionChange={setSuggestedPermission}
            onApproval={handleApproval}
            approvalBusy={approvalBusy}
            onTaskUpdated={handleTaskUpdated}
            onTaskDeleted={handleTaskDeleted}
            onRefresh={refreshSnapshot}
            onToast={setToast}
            onNewTask={() => setComposerOpen(true)}
            onMenu={() => setSidebarOpen(true)}
          />
        ) : null}
        {view === "approvals" ? (
          <ApprovalsView
            approvals={snapshot.approvals}
            tasks={snapshot.tasks}
            onApproval={handleApproval}
            approvalBusy={approvalBusy}
            onRefresh={() => void refreshSnapshot()}
            refreshing={refreshing}
            onMenu={() => setSidebarOpen(true)}
          />
        ) : null}
        {view === "tools" ? (
          <ToolsView tools={snapshot.tools} onMenu={() => setSidebarOpen(true)} />
        ) : null}
        {view === "audit" ? (
          <AuditView
            events={snapshot.recentEvents}
            onSelectTask={openTask}
            onMenu={() => setSidebarOpen(true)}
          />
        ) : null}
      </main>

      {composerOpen ? (
        <TaskComposerModal
          api={api}
          isLive={isLive}
          permission={user.permission}
          suggestedPermission={suggestedPermission}
          onClose={() => setComposerOpen(false)}
          onCreated={handleNewTask}
          onToast={setToast}
        />
      ) : null}

      {toast ? (
        <div className="toast" role="status">
          <CheckCircle2 size={16} />
          <span>{toast}</span>
        </div>
      ) : null}
    </div>
  );
}
