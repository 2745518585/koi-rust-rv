import { useState } from "react";
import type { FormEvent } from "react";
import { LoaderCircle, Plus, X } from "lucide-react";
import type { KoiApiClient } from "../api/client";
import type { PermissionLevel, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";
import { suggestedPermissionOptions } from "../lib/ui";

/**
 * 新建会话弹窗：只创建隔离会话与其最低控制权限，首条消息在会话创建后再提交。
 */
export function TaskComposerModal({
  api,
  isLive,
  permission,
  onClose,
  onCreated,
  onToast,
}: {
  api: KoiApiClient;
  isLive: boolean;
  permission: PermissionLevel;
  onClose: () => void;
  onCreated: (task: TaskSummary) => void;
  onToast: (message: string) => void;
}) {
  const { t, locale } = useI18n();
  const [minimumPermission, setMinimumPermission] = useState<PermissionLevel>("User");
  const [busy, setBusy] = useState(false);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!isLive) {
      onToast(t("backendOffline"));
      return;
    }
    setBusy(true);
    try {
      const created = await api.createTask({
        minimumPermission,
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
            ? "Create an empty, isolated session. Send the first message after it opens."
            : "创建一个空的隔离会话；打开后再发送第一条消息。"}
        </p>
        <label className="field-label" htmlFor="task-minimum-permission">
          {locale === "en" ? "Minimum control permission" : "最低控制权限"}
        </label>
        <select
          id="task-minimum-permission"
          value={minimumPermission}
          onChange={(event) => setMinimumPermission(event.target.value as PermissionLevel)}
        >
          {suggestedPermissionOptions(permission).map((level) => (
            <option value={level} key={level}>{level}</option>
          ))}
        </select>
        <footer className="modal-foot">
          <span className="modal-identity">
            {locale === "en" ? "Identity" : "当前身份"}：{permission}
          </span>
          <div className="modal-foot-actions">
            <button type="button" className="button button-secondary" onClick={onClose}>
              {locale === "en" ? "Cancel" : "取消"}
            </button>
            <button className="button button-primary" type="submit" disabled={busy}>
              {busy ? <LoaderCircle className="spin" size={15} /> : <Plus size={15} />}
              {locale === "en" ? "Create" : "创建会话"}
            </button>
          </div>
        </footer>
      </form>
    </div>
  );
}
