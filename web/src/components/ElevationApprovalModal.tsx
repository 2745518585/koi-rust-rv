import { Check, Clock3, LoaderCircle, ShieldAlert, X } from "lucide-react";
import type { ApprovalRequest, AuthorizationNotification, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";
import { formatRelative, scopeLabel } from "../lib/format";
import { PermissionBadge } from "../lib/ui";

/** Core 发出授权请求后显示；实际放行仍严格绑定持久化的审批事件。 */
export function ElevationApprovalModal({
  request,
  approval,
  task,
  queueSize,
  busy,
  onApprove,
  onDeny,
  onDefer,
}: {
  request: AuthorizationNotification;
  approval?: ApprovalRequest;
  task?: TaskSummary;
  queueSize: number;
  busy: boolean;
  onApprove: () => void;
  onDeny: () => void;
  onDefer: () => void;
}) {
  const { locale, t } = useI18n();
  const details = approval ?? request;

  return (
    <div className="modal-layer elevation-layer">
      <button className="modal-backdrop" onClick={onDefer} aria-label={t("elevationDefer")} />
      <section className="modal elevation-modal" role="dialog" aria-modal="true" aria-labelledby="elevation-title">
        <div className="elevation-head">
          <span className="elevation-mark"><ShieldAlert size={19} /></span>
          <div>
            <span className="elevation-eyebrow">{t("elevationEyebrow")}</span>
            <h2 id="elevation-title">{t("elevationTitle")}</h2>
          </div>
          <button className="icon-button elevation-close" onClick={onDefer} aria-label={t("elevationDefer")}>
            <X size={17} />
          </button>
        </div>

        <p className="elevation-copy">{t("elevationCopy")}</p>
        <div className="elevation-tool">
          <div>
            <span className="elevation-label">{t("elevationTool")}</span>
            <strong>{request.toolName}</strong>
          </div>
          <PermissionBadge level={request.requiredPermission} />
        </div>

        {approval ? (
          <>
            <p className="elevation-description">{approval.toolDescription}</p>
            <dl className="elevation-details">
              <div><dt>{t("elevationSession")}</dt><dd>{task?.title ?? approval.taskId}</dd></div>
              <div><dt>{t("elevationScope")}</dt><dd>{scopeLabel(approval)}</dd></div>
              <div><dt>{t("elevationRequested")}</dt><dd>{formatRelative(approval.requestedAt, locale)}</dd></div>
            </dl>
            {approval.argumentsPreview ? (
              <div className="elevation-arguments">
                <span className="elevation-label">{t("elevationArguments")}</span>
                <pre>{approval.argumentsPreview}</pre>
              </div>
            ) : null}
          </>
        ) : (
          <div className="elevation-loading"><LoaderCircle className="spin" size={16} /><span>{t("elevationLoading")}</span></div>
        )}

        <div className="elevation-evidence"><Clock3 size={14} /><span>{t("elevationNotice")}</span><code>{details.argumentsHash}</code></div>
        <div className="elevation-foot">
          <button className="button button-quiet" onClick={onDefer} disabled={busy}>{t("elevationDefer")}</button>
          <div className="elevation-actions">
            <button className="button button-danger-ghost" onClick={onDeny} disabled={busy || !approval}><X size={14} />{t("elevationDeny")}</button>
            <button className="button button-approve" onClick={onApprove} disabled={busy || !approval}>{busy ? <LoaderCircle className="spin" size={14} /> : <Check size={14} />}{t("elevationApprove")}</button>
          </div>
        </div>
        {queueSize > 1 ? <p className="elevation-queue">{t("elevationQueue", { count: queueSize - 1 })}</p> : null}
      </section>
    </div>
  );
}
