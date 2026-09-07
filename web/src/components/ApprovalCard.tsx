import { Check, LoaderCircle, ShieldAlert, X } from "lucide-react";
import type { ApprovalGrant, ApprovalRequest, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";
import { formatRelative, scopeLabel } from "../lib/format";
import { PermissionBadge } from "../lib/ui";

/**
 * 审批卡片同时用于审批面板与会话内联展示；compact 模式去掉次要信息。
 */
export function ApprovalCard({
  approval,
  task,
  onApproval,
  busy,
  compact = false,
}: {
  approval: ApprovalRequest;
  task?: TaskSummary;
  onApproval: (approval: ApprovalRequest, approved: boolean, grant?: ApprovalGrant) => void;
  busy: boolean;
  compact?: boolean;
}) {
  const { locale } = useI18n();
  return (
    <article className="approval-card">
      <div className="approval-card-head">
        <span className="approval-flag">
          <ShieldAlert size={13} />
          {locale === "en" ? "Confirmation required" : "需要确认"}
        </span>
        <span className="approval-time">{formatRelative(approval.requestedAt, locale)}</span>
      </div>
      <div className="approval-card-title">
        <strong>{approval.toolName}</strong>
        <PermissionBadge level={approval.requiredPermission} />
      </div>
      <p className="approval-card-desc">{approval.toolDescription}</p>
      {compact ? null : (
        <div className="approval-card-meta">
          <span>{scopeLabel(approval)}</span>
          {task ? <span>{task.title}</span> : null}
          <code>{approval.argumentsHash}</code>
        </div>
      )}
      <div className="approval-card-actions">
        <button className="button button-danger-ghost" onClick={() => onApproval(approval, false)} disabled={busy}>
          <X size={14} />
          {locale === "en" ? "Deny" : "拒绝"}
        </button>
        <button
          className="button button-approve"
          onClick={() => onApproval(approval, true, {
            current_operation: { original_event_payload_id: approval.toolProposalEventId },
          })}
          disabled={busy}
        >
          {busy ? <LoaderCircle className="spin" size={14} /> : <Check size={14} />}
          {locale === "en" ? "Approve once" : "仅允许当前操作"}
        </button>
        <button className="button button-quiet" onClick={() => onApproval(approval, true, "any_operation")} disabled={busy}>
          <Check size={14} />
          {locale === "en" ? "Approve any" : "允许任意操作"}
        </button>
      </div>
      {compact && task ? <span className="approval-card-task">{task.title}</span> : null}
    </article>
  );
}
