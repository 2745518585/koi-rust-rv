import type { LucideIcon } from "lucide-react";
import { ChevronDown, FileClock, LogOut, Plus, ShieldCheck, Wrench, X } from "lucide-react";
import type { AuthUser } from "../api/client";
import type { TaskSummary, UsageSummary } from "../api/types";
import { useI18n } from "../i18n";
import { shortId } from "../lib/format";
import type { ViewKey } from "../lib/ui";
import { labelFor, PermissionBadge, statusMeta } from "../lib/ui";

interface SidebarProps {
  tasks: TaskSummary[];
  selectedTaskId: string;
  view: ViewKey;
  pendingApprovals: number;
  usage: UsageSummary;
  user: AuthUser;
  isLive: boolean;
  /** 移动端抽屉展开状态。 */
  open: boolean;
  onSelectTask: (taskId: string) => void;
  onNavigate: (view: ViewKey) => void;
  onNewTask: () => void;
  onLogout: () => void;
  onClose: () => void;
}

export function Sidebar({
  tasks,
  selectedTaskId,
  view,
  pendingApprovals,
  usage,
  user,
  isLive,
  open,
  onSelectTask,
  onNavigate,
  onNewTask,
  onLogout,
  onClose,
}: SidebarProps) {
  const { t } = useI18n();
  const mainSession = tasks.find((task) => task.isMain);
  const children = tasks.filter((task) => !task.isMain);

  const navItems: Array<{ key: ViewKey; label: string; icon: LucideIcon; count?: number }> = [
    { key: "approvals", label: t("approvals"), icon: ShieldCheck, count: pendingApprovals },
    { key: "tools", label: t("tools"), icon: Wrench },
    { key: "audit", label: t("audit"), icon: FileClock },
  ];

  return (
    <aside className={`sidebar ${open ? "sidebar-open" : ""}`}>
      <div className="side-brand">
        <div className="brand-mark" aria-hidden="true">
          <span />
          <span />
          <span />
        </div>
        <strong className="side-brand-name">koi</strong>
        <span className={`conn-dot ${isLive ? "conn-dot-on" : ""}`} title={isLive ? t("connected") : t("disconnected")} />
        <button className="icon-button side-close" onClick={onClose} aria-label="关闭导航">
          <X size={17} />
        </button>
      </div>

      <div className="side-new">
        <button className="button button-primary side-new-button" onClick={onNewTask}>
          <Plus size={15} />
          {t("newSession")}
        </button>
      </div>

      <UsageCard usage={usage} />

      <div className="side-sessions">
        <div className="side-group">{t("mainSession")}</div>
        {mainSession ? (
          <SessionItem
            task={mainSession}
            active={selectedTaskId === mainSession.taskId}
            onSelect={onSelectTask}
          />
        ) : (
          <p className="side-note">{t("mainNotice")}</p>
        )}
        <div className="side-group">{t("taskSessions")}</div>
        <div className="side-session-list">
          {children.length ? (
            children.map((task) => (
              <SessionItem
                key={task.taskId}
                task={task}
                active={selectedTaskId === task.taskId}
                onSelect={onSelectTask}
              />
            ))
          ) : (
            <p className="side-note">{t("noSessions")}</p>
          )}
        </div>
      </div>

      <nav className="side-nav" aria-label="功能面板">
        {navItems.map((item) => {
          const Icon = item.icon;
          const active = view === item.key;
          return (
            <button
              key={item.key}
              className={`side-nav-item ${active ? "side-nav-item-active" : ""}`}
              onClick={() => onNavigate(item.key)}
            >
              <Icon size={15} />
              <span>{item.label}</span>
              {item.count ? <span className="side-nav-count">{item.count}</span> : null}
            </button>
          );
        })}
      </nav>

      <div className="side-user">
        <span className="avatar" aria-hidden="true">
          {user.username.slice(0, 1).toUpperCase()}
        </span>
        <span className="side-user-copy">
          <strong>{user.username}</strong>
          <PermissionBadge level={user.permission} />
        </span>
        <button className="icon-button" onClick={onLogout} title={t("logout")} aria-label={t("logout")}>
          <LogOut size={15} />
        </button>
      </div>
    </aside>
  );
}

function UsageCard({ usage }: { usage: UsageSummary }) {
  const { t } = useI18n();
  const monthlyBudget = usage.monthlyBudgetUsd > 0
    ? ` / ${formatUsd(usage.monthlyBudgetUsd)}`
    : "";
  const taskBudgetValue = usage.tokenBudgetPerTask;
  const taskBudget = taskBudgetValue === null
    ? t("unlimited")
    : `${formatTokenCount(taskBudgetValue)} ${t("tokens")}`;
  return (
    <details className="side-usage">
      <summary className="side-usage-summary">
        <span>{t("usageSummary")}</span>
        <ChevronDown size={13} aria-hidden="true" />
      </summary>
      <div className="side-usage-body" aria-label={t("usageSummary")}>
        <div className="side-usage-head">
          <span>{t("monthCost")}</span>
          <strong>{formatUsd(usage.monthSpentUsd)}{monthlyBudget}</strong>
        </div>
        <div className="side-usage-line">
          <span>{t("monthTokens")}</span>
          <strong>{formatTokenCount(usage.totalTokensMonth)}</strong>
        </div>
        <div className="side-usage-line">
          <span>{t("taskTokenBudget")}</span>
          <strong>{taskBudget}</strong>
        </div>
        <small title={t("pricingHint")}>
          {t("pricingShort", {
            input: usage.inputPricePerMillionTokens,
            cached: usage.cachedInputPricePerMillionTokens,
            output: usage.outputPricePerMillionTokens,
          })}
        </small>
      </div>
    </details>
  );
}

function formatTokenCount(tokens: number): string {
  return new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 }).format(tokens);
}

function formatUsd(value: number): string {
  return `$${value.toFixed(4)}`;
}

function SessionItem({
  task,
  active,
  onSelect,
}: {
  task: TaskSummary;
  active: boolean;
  onSelect: (taskId: string) => void;
}) {
  const { t, locale } = useI18n();
  const meta = statusMeta[task.status];
  return (
    <button
      className={`session-item ${active ? "session-item-active" : ""}`}
      onClick={() => onSelect(task.taskId)}
      title={labelFor(meta.label, locale)}
    >
      <span className={`session-dot ${meta.dot} ${task.status === "Running" ? "dot-pulse" : ""}`} />
      <span className="session-copy">
        <strong>{task.title}</strong>
        <small>{task.isMain ? t("mainSession") : shortId(task.taskId)}</small>
      </span>
    </button>
  );
}
