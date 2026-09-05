import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";
import {
  Bot,
  CheckCircle2,
  CircleDot,
  CircleStop,
  Clock3,
  LoaderCircle,
  Menu,
  MessageSquare,
  Pause,
  Settings2,
  ShieldAlert,
  ShieldCheck,
  Terminal,
  XCircle,
  Zap,
} from "lucide-react";
import { useI18n } from "../i18n";
import type { Locale } from "../i18n";
import { formatRelative } from "./format";
import type { EventKind, PermissionLevel, TaskEvent, TaskStatus, ToolSideEffect } from "../api/types";

/** 主界面只有会话工作台一个核心视图，其余为功能性面板。 */
export type ViewKey = "conversation" | "approvals" | "tools" | "audit";

type Label = { zh: string; en: string };

export function useLabel(label: Label): string {
  const { locale } = useI18n();
  return locale === "en" ? label.en : label.zh;
}

/** 纯函数版本：用于 map 回调等不能调用 hook 的位置。 */
export function labelFor(label: Label, locale: Locale): string {
  return locale === "en" ? label.en : label.zh;
}

export const statusMeta: Record<
  TaskStatus,
  { label: Label; className: string; dot: string; icon: LucideIcon }
> = {
  New: { label: { zh: "新建", en: "New" }, className: "status-neutral", dot: "dot-neutral", icon: CircleDot },
  Created: { label: { zh: "已创建", en: "Created" }, className: "status-neutral", dot: "dot-neutral", icon: CircleDot },
  Queued: { label: { zh: "排队中", en: "Queued" }, className: "status-info", dot: "dot-info", icon: Clock3 },
  Running: { label: { zh: "运行中", en: "Running" }, className: "status-running", dot: "dot-running", icon: LoaderCircle },
  WaitingApproval: { label: { zh: "待审批", en: "Waiting approval" }, className: "status-warning", dot: "dot-warning", icon: ShieldAlert },
  Paused: { label: { zh: "已暂停", en: "Paused" }, className: "status-paused", dot: "dot-paused", icon: Pause },
  Cancelling: { label: { zh: "取消中", en: "Cancelling" }, className: "status-warning", dot: "dot-warning", icon: CircleStop },
  Completed: { label: { zh: "已完成", en: "Completed" }, className: "status-success", dot: "dot-success", icon: CheckCircle2 },
  Failed: { label: { zh: "失败", en: "Failed" }, className: "status-danger", dot: "dot-danger", icon: XCircle },
  Cancelled: { label: { zh: "已取消", en: "Cancelled" }, className: "status-neutral", dot: "dot-neutral", icon: CircleStop },
  Expired: { label: { zh: "已过期", en: "Expired" }, className: "status-neutral", dot: "dot-neutral", icon: Clock3 },
};

export const permissionMeta: Record<PermissionLevel, { label: Label; className: string }> = {
  None: { label: { zh: "无", en: "None" }, className: "perm-none" },
  User: { label: { zh: "User", en: "User" }, className: "perm-user" },
  Operator: { label: { zh: "Operator", en: "Operator" }, className: "perm-operator" },
  Admin: { label: { zh: "Admin", en: "Admin" }, className: "perm-admin" },
  System: { label: { zh: "System", en: "System" }, className: "perm-system" },
};

export const eventKindMeta: Record<EventKind, { label: Label; className: string; icon: LucideIcon }> = {
  ingress: { label: { zh: "输入", en: "Input" }, className: "kind-ingress", icon: MessageSquare },
  model: { label: { zh: "模型", en: "Model" }, className: "kind-model", icon: Bot },
  tool: { label: { zh: "工具", en: "Tool" }, className: "kind-tool", icon: Terminal },
  approval: { label: { zh: "授权", en: "Approval" }, className: "kind-approval", icon: ShieldCheck },
  control: { label: { zh: "控制", en: "Control" }, className: "kind-control", icon: Settings2 },
  system: { label: { zh: "系统", en: "System" }, className: "kind-system", icon: Zap },
};

export const sideEffectMeta: Record<ToolSideEffect, { label: Label; className: string }> = {
  ReadOnly: { label: { zh: "只读", en: "Read-only" }, className: "effect-readonly" },
  Notification: { label: { zh: "通知", en: "Notify" }, className: "effect-notification" },
  Stateful: { label: { zh: "有状态", en: "Stateful" }, className: "effect-stateful" },
  Destructive: { label: { zh: "高风险", en: "Destructive" }, className: "effect-destructive" },
};

const suggestedPermissionLevels: PermissionLevel[] = ["User", "Operator", "Admin"];

export const permissionRank: Record<PermissionLevel, number> = {
  None: 0,
  User: 1,
  Operator: 2,
  Admin: 3,
  System: 4,
};

export function suggestedPermissionOptions(maximum: PermissionLevel): PermissionLevel[] {
  return suggestedPermissionLevels.filter(
    (permission) => permissionRank[permission] <= permissionRank[maximum],
  );
}

export function StatusBadge({ status }: { status: TaskStatus }) {
  const meta = statusMeta[status];
  const label = useLabel(meta.label);
  const Icon = meta.icon;
  return (
    <span className={`badge ${meta.className}`}>
      <Icon size={12} className={status === "Running" ? "spin" : undefined} />
      {label}
    </span>
  );
}

export function PermissionBadge({ level }: { level: PermissionLevel }) {
  const meta = permissionMeta[level];
  const label = useLabel(meta.label);
  return <span className={`badge badge-mono ${meta.className}`}>{label}</span>;
}

export function EffectBadge({ effect }: { effect: ToolSideEffect }) {
  const meta = sideEffectMeta[effect];
  const label = useLabel(meta.label);
  return <span className={`badge badge-mono ${meta.className}`}>{label}</span>;
}

/** 事件种类的着色小图标。 */
export function EventIcon({ kind, size = 15 }: { kind: EventKind; size?: number }) {
  const meta = eventKindMeta[kind] ?? eventKindMeta.system;
  const Icon = meta.icon;
  return (
    <span className={`kind-icon ${meta.className}`}>
      <Icon size={size} />
    </span>
  );
}

/**
 * 事件模式下的一行事件；audit 视图会传入 onClick 使其可点击跳转。
 */
export function EventLine({
  event,
  onClick,
}: {
  event: TaskEvent;
  onClick?: () => void;
}) {
  const { locale } = useI18n();
  const content = (
    <>
      <EventIcon kind={event.kind} />
      <div className="event-line-main">
        <div className="event-line-top">
          <strong>
            {event.sequence.toString().padStart(3, "0")} · {event.title}
          </strong>
          <time>{formatRelative(event.occurredAt, locale)}</time>
        </div>
        <p>{event.summary}</p>
      </div>
      <span className="event-line-meta">
        {event.source} · {event.permission}
      </span>
    </>
  );
  return onClick ? (
    <button className="event-line event-line-click" onClick={onClick}>
      {content}
    </button>
  ) : (
    <article className="event-line">{content}</article>
  );
}

export function EmptyState({
  icon: Icon,
  title,
  description,
  action,
}: {
  icon: LucideIcon;
  title: string;
  description?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty-state">
      <span className="empty-icon">
        <Icon size={20} />
      </span>
      <strong>{title}</strong>
      {description ? <p>{description}</p> : null}
      {action}
    </div>
  );
}

/** 功能面板（审批/工具/审计）共用的页头；移动端提供侧栏入口。 */
export function PanelHeader({
  title,
  description,
  onMenu,
  actions,
}: {
  title: string;
  description: string;
  onMenu: () => void;
  actions?: ReactNode;
}) {
  return (
    <header className="panel-head">
      <button className="icon-button mobile-only" onClick={onMenu} aria-label="打开导航">
        <Menu size={18} />
      </button>
      <div className="panel-head-copy">
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      {actions ? <div className="panel-head-actions">{actions}</div> : null}
    </header>
  );
}

/**
 * 建议授权选择器：只表示本次 Web 操作携带的权限上限建议，
 * 服务端仍会按当前身份与核心权限规则重新核定。
 */
export function PermissionSelector({
  value,
  maximum,
  onChange,
}: {
  value: PermissionLevel;
  maximum: PermissionLevel;
  onChange: (permission: PermissionLevel) => void;
}) {
  const { t, locale } = useI18n();
  const options = suggestedPermissionOptions(maximum);
  const selected = options.includes(value) ? value : options[0] ?? "User";
  return (
    <label className="permission-selector" title={t("suggestedPermissionHint")}>
      <ShieldCheck size={14} />
      <select
        aria-label={t("suggestedPermission")}
        value={selected}
        onChange={(event) => onChange(event.target.value as PermissionLevel)}
      >
        {options.map((permission) => (
          <option value={permission} key={permission}>
            {labelFor(permissionMeta[permission].label, locale)}
          </option>
        ))}
      </select>
    </label>
  );
}
