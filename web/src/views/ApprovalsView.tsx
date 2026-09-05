import { Check, RefreshCw, ShieldCheck, X } from "lucide-react";
import type { ApprovalRequest, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";
import { formatRelative, scopeLabel } from "../lib/format";
import { EmptyState, PanelHeader } from "../lib/ui";
import { ApprovalCard } from "../components/ApprovalCard";

export function ApprovalsView({
  approvals,
  tasks,
  onApproval,
  approvalBusy,
  onRefresh,
  refreshing,
  onMenu,
}: {
  approvals: ApprovalRequest[];
  tasks: TaskSummary[];
  onApproval: (approval: ApprovalRequest, approved: boolean) => void;
  approvalBusy: string | null;
  onRefresh: () => void;
  refreshing: boolean;
  onMenu: () => void;
}) {
  const { t, locale } = useI18n();
  const copy = locale === "en"
    ? {
        title: "Approvals",
        subtitle: "Every high-risk tool call needs explicit human confirmation.",
        pending: "Pending",
        history: "Recent decisions",
        allDone: "All clear",
        allDoneHint: "New authorization requests will appear here in real time.",
        noHistory: "No decisions yet",
        noHistoryHint: "Processed requests are kept here for audit.",
      }
    : {
        title: "审批中心",
        subtitle: "每一次高风险工具调用都必须有明确的人工决定。",
        pending: "待处理",
        history: "最近决定",
        allDone: "全部处理完毕",
        allDoneHint: "新的授权请求会实时出现在这里。",
        noHistory: "暂无审批记录",
        noHistoryHint: "处理过的请求会保留在这里。",
      };

  // 已终态任务上的待处理请求无法再写入审批事件，界面将其视为过期并隐藏，避免用户
  // 点击后得到无法恢复的提交失败提示。
  const visibleApprovals = approvals.filter((approval) => {
    const task = tasks.find((item) => item.taskId === approval.taskId);
    return approval.status !== "Expired"
      && !["Completed", "Cancelled", "Failed", "Expired"].includes(task?.status ?? "");
  });
  const pending = visibleApprovals.filter((item) => item.status === "Pending");
  const history = visibleApprovals.filter((item) => item.status !== "Pending");

  return (
    <section className="panel">
      <PanelHeader
        title={copy.title}
        description={copy.subtitle}
        onMenu={onMenu}
        actions={
          <button className="button button-secondary" onClick={onRefresh} disabled={refreshing}>
            <RefreshCw className={refreshing ? "spin" : undefined} size={15} />
            {t("refresh")}
          </button>
        }
      />
      <div className="panel-body panel-body-narrow">
        <div className="panel-card">
          <div className="panel-card-head">
            <h2>{copy.pending}</h2>
            {pending.length ? <span className="count-chip">{pending.length}</span> : null}
          </div>
          {pending.length ? (
            <div className="approval-stack">
              {pending.map((approval) => (
                <ApprovalCard
                  key={approval.approvalRequestEventId}
                  approval={approval}
                  task={tasks.find((task) => task.taskId === approval.taskId)}
                  onApproval={onApproval}
                  busy={approvalBusy === approval.approvalRequestEventId}
                />
              ))}
            </div>
          ) : (
            <EmptyState icon={ShieldCheck} title={copy.allDone} description={copy.allDoneHint} />
          )}
        </div>
        <div className="panel-card">
          <div className="panel-card-head">
            <h2>{copy.history}</h2>
          </div>
          {history.length ? (
            <div className="history-list">
              {history.map((approval) => (
                <ApprovalHistoryRow key={approval.approvalRequestEventId} approval={approval} />
              ))}
            </div>
          ) : (
            <EmptyState icon={Check} title={copy.noHistory} description={copy.noHistoryHint} />
          )}
        </div>
      </div>
    </section>
  );
}

function ApprovalHistoryRow({ approval }: { approval: ApprovalRequest }) {
  const { locale } = useI18n();
  const approved = approval.status === "Approved";
  const statusText =
    locale === "en"
      ? approved
        ? "Approved"
        : approval.status === "Expired"
          ? "Expired"
          : "Denied"
      : approved
        ? "已批准"
        : approval.status === "Expired"
          ? "已过期"
          : "已拒绝";
  return (
    <div className="history-row">
      <span className={`history-icon ${approved ? "history-approved" : "history-denied"}`}>
        {approved ? <Check size={14} /> : <X size={14} />}
      </span>
      <div className="history-main">
        <strong>{approval.toolName}</strong>
        <span>
          {scopeLabel(approval)} · {approval.requester}
        </span>
      </div>
      <div className="history-end">
        <span className={`badge badge-mono ${approved ? "status-approved" : "status-denied"}`}>{statusText}</span>
        <small>{formatRelative(approval.requestedAt, locale)}</small>
      </div>
    </div>
  );
}
