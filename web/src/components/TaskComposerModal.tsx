import { useState } from "react";
import type { FormEvent } from "react";
import { LoaderCircle, Plus, X } from "lucide-react";
import type { KoiApiClient } from "../api/client";
import type { PermissionLevel, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";

/**
 * 新建会话弹窗：描述现象并绑定作用域；建议授权沿用全局选择器当前值。
 */
export function TaskComposerModal({
  api,
  isLive,
  permission,
  suggestedPermission,
  onClose,
  onCreated,
  onToast,
}: {
  api: KoiApiClient;
  isLive: boolean;
  permission: PermissionLevel;
  suggestedPermission: PermissionLevel;
  onClose: () => void;
  onCreated: (task: TaskSummary) => void;
  onToast: (message: string) => void;
}) {
  const { t, locale } = useI18n();
  const [message, setMessage] = useState("");
  const [scope, setScope] = useState("service:order-api");
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!message.trim()) return;
    if (!isLive) {
      onToast(t("backendOffline"));
      return;
    }
    const [kind = "service", id = "order-api"] = scope.split(":");
    setBusy(true);
    try {
      const created = await api.createTask({
        message: message.trim(),
        scope: { kind, id },
        suggestedPermission,
      });
      onCreated(created);
    } catch (error) {
      const detail = error instanceof Error ? error.message : "";
      onToast(detail ? `${t("taskCreateFailed")}：${detail}` : t("taskCreateFailed"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="modal-layer" role="presentation">
      <button className="modal-backdrop" onClick={onClose} aria-label="关闭新建会话" />
      <form className="modal" onSubmit={submit}>
        <header className="modal-head">
          <h2>{locale === "en" ? "New session" : "新建会话"}</h2>
          <button type="button" className="icon-button" onClick={onClose} aria-label="关闭">
            <X size={17} />
          </button>
        </header>
        <p className="modal-copy">
          {locale === "en"
            ? "Describe what you observed; the agent enters an auditable task flow with the scope below."
            : "描述你观察到的现象，Agent 会携带来源与作用域进入可审计的任务流。"}
        </p>
        <label className="field-label" htmlFor="task-message">
          {locale === "en" ? "Problem description" : "问题描述"}
        </label>
        <textarea
          id="task-message"
          autoFocus
          value={message}
          onChange={(event) => setMessage(event.target.value)}
          placeholder={
            locale === "en"
              ? "e.g. check order-api 5xx rate and connection pool for the last 10 minutes"
              : "例如：检查 order-api 最近 10 分钟的 5xx 与连接池状态"
          }
          rows={4}
        />
        <label className="field-label" htmlFor="task-scope">
          {locale === "en" ? "Scope" : "作用域"}
        </label>
        <div className="scope-input">
          <span>scope</span>
          <input id="task-scope" value={scope} onChange={(event) => setScope(event.target.value)} />
        </div>
        <footer className="modal-foot">
          <span className="modal-identity">
            {locale === "en" ? "Identity" : "当前身份"}：{permission} ·{" "}
            {locale === "en" ? "suggested" : "建议授权"}：{suggestedPermission}
          </span>
          <div className="modal-foot-actions">
            <button type="button" className="button button-secondary" onClick={onClose}>
              {locale === "en" ? "Cancel" : "取消"}
            </button>
            <button className="button button-primary" type="submit" disabled={busy || !message.trim()}>
              {busy ? <LoaderCircle className="spin" size={15} /> : <Plus size={15} />}
              {locale === "en" ? "Create" : "创建会话"}
            </button>
          </div>
        </footer>
      </form>
    </div>
  );
}
