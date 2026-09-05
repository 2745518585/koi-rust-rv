import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent, UIEvent } from "react";
import {
  CircleStop,
  FileClock,
  Inbox,
  LoaderCircle,
  Menu,
  MessageSquare,
  Pause,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  Send,
  Terminal,
  Trash2,
} from "lucide-react";
import type { KoiApiClient } from "../api/client";
import type { ApprovalRequest, ModelSelection, PermissionLevel, TaskEvent, TaskSummary } from "../api/types";
import { useI18n } from "../i18n";
import { groupConversationEvents, type ConversationFeedItem } from "../lib/events";
import { formatRelative, scopeLabel } from "../lib/format";
import { EmptyState, EventIcon, EventLine, EventSummary, PermissionSelector, StatusBadge } from "../lib/ui";
import { ApprovalCard } from "./ApprovalCard";

type FeedMode = "conversation" | "events";

export interface ConversationProps {
  api: KoiApiClient;
  task?: TaskSummary;
  eventsRevision: number;
  isLive: boolean;
  approvals: ApprovalRequest[];
  models: ModelSelection[];
  /** 当前身份权限，作为建议授权选择器的上限。 */
  maximumPermission: PermissionLevel;
  suggestedPermission: PermissionLevel;
  onPermissionChange: (permission: PermissionLevel) => void;
  onApproval: (approval: ApprovalRequest, approved: boolean) => void;
  approvalBusy: string | null;
  onTaskUpdated: (task: TaskSummary) => void;
  onTaskDeleted: (taskId: string) => void;
  onRefresh: () => Promise<void>;
  onToast: (message: string) => void;
  onNewTask: () => void;
  onMenu: () => void;
}

/**
 * 会话工作台：与 Agent 对话的主界面。
 * 对话模式只显示可读的输入、最终答复与工具调用；事件模式展示完整事件流。
 */
export function Conversation({
  api,
  task,
  eventsRevision,
  isLive,
  approvals,
  models,
  maximumPermission,
  suggestedPermission,
  onPermissionChange,
  onApproval,
  approvalBusy,
  onTaskUpdated,
  onTaskDeleted,
  onRefresh,
  onToast,
  onNewTask,
  onMenu,
}: ConversationProps) {
  const { t, locale } = useI18n();
  const [mode, setMode] = useState<FeedMode>("conversation");
  const [events, setEvents] = useState<TaskEvent[]>([]);
  const [message, setMessage] = useState("");
  const [loading, setLoading] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const feedRef = useRef<HTMLDivElement>(null);
  const shouldStickToBottom = useRef(true);
  const previousView = useRef<string | null>(null);
  const wasLoading = useRef(false);

  const taskId = task?.taskId;

  useEffect(() => {
    if (!taskId || !isLive) {
      setEvents([]);
      return;
    }
    let active = true;
    setLoading(true);
    api.getTaskEvents(taskId)
      .then((next) => {
        if (active) setEvents(next.sort((a, b) => a.sequence - b.sequence));
      })
      .catch(() => {
        if (active) setEvents([]);
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [api, eventsRevision, isLive, taskId]);

  const conversationEvents =
    mode === "conversation"
      ? events.filter((event) => {
          if (event.kind === "ingress" || event.kind === "tool") return true;
          // 旧版本持久化过 token 级 Delta；对话视图只显示可读的最终答复和失败信息。
          return event.kind === "model" && (event.title === "Agent 回复" || event.title === "模型调用失败");
        })
      : events;
  const conversationItems = useMemo(
    () => (mode === "conversation" ? groupConversationEvents(conversationEvents) : []),
    [conversationEvents, mode],
  );
  const eventItems = useMemo(() => groupConversationEvents(events), [events]);
  // 使用原始事件数量触发滚动更新；同一个工具组追加内部步骤时，聚合项数量不会变化。
  const visibleCount = mode === "events" ? events.length : conversationEvents.length;

  // 会话切换、加载完成或新事件到达时，保持视图贴在底部；用户上滚后不再打扰。
  useLayoutEffect(() => {
    const feed = feedRef.current;
    if (!feed) return;
    const view = `${taskId ?? "none"}:${mode}`;
    const viewChanged = previousView.current !== view;
    const finishedLoading = wasLoading.current && !loading;
    if (viewChanged || finishedLoading || shouldStickToBottom.current) {
      feed.scrollTop = feed.scrollHeight;
      shouldStickToBottom.current = true;
    }
    previousView.current = view;
    wasLoading.current = loading;
  }, [visibleCount, loading, mode, taskId]);

  function trackFeedScroll(event: UIEvent<HTMLDivElement>) {
    const feed = event.currentTarget;
    shouldStickToBottom.current = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 72;
  }

  async function refreshSession() {
    await onRefresh();
    if (taskId && isLive) {
      try {
        setEvents((await api.getTaskEvents(taskId)).sort((a, b) => a.sequence - b.sequence));
      } catch {
        // 快照刷新仍是权威来源，局部事件失败不阻塞界面。
      }
    }
  }

  async function sendMessage(event?: FormEvent<HTMLFormElement>) {
    event?.preventDefault();
    if (!task || !message.trim() || submitting) return;
    setSubmitting(true);
    try {
      await api.appendTaskContext(task.taskId, {
        message: message.trim(),
        suggestedPermission,
      });
      setMessage("");
      await refreshSession();
    } catch {
      onToast(t("inputFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  function onComposerKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      void sendMessage();
    }
  }

  async function control(action: "pause" | "resume") {
    if (!task) return;
    setSubmitting(true);
    try {
      onTaskUpdated(await api.controlTask(task.taskId, { action }));
      await refreshSession();
    } catch {
      onToast(t("controlFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  async function stop() {
    if (!task) return;
    setSubmitting(true);
    try {
      await api.requestCancellation(task.taskId, t("requestStopReason"), suggestedPermission);
      await refreshSession();
    } catch {
      onToast(t("controlFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  async function rename() {
    if (!task || task.isMain) return;
    const name = window.prompt(t("renamePrompt"), task.title)?.trim();
    if (!name) return;
    setSubmitting(true);
    try {
      onTaskUpdated(await api.nameTask(task.taskId, name));
      onToast(t("renamed"));
    } catch {
      onToast(t("controlFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  async function remove() {
    if (!task || task.isMain) return;
    if (!window.confirm(`${t("deleteConfirm")} “${task.title}”`)) return;
    setSubmitting(true);
    try {
      await api.deleteTask(task.taskId);
      onTaskDeleted(task.taskId);
      onToast(t("deleted"));
    } catch {
      onToast(t("controlFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  async function changeModel(key: string) {
    if (!task || submitting) return;
    const [provider, modelId] = key.split(":");
    if (!provider || !modelId) return;
    setSubmitting(true);
    try {
      onTaskUpdated(await api.controlTask(task.taskId, { action: "select_model", provider, modelId }));
    } catch {
      onToast(t("controlFailed"));
    } finally {
      setSubmitting(false);
    }
  }

  if (!task) {
    return (
      <section className="conv conv-empty-wrap">
        <button className="icon-button mobile-only conv-menu" onClick={onMenu} aria-label="打开导航">
          <Menu size={18} />
        </button>
        <EmptyState
          icon={Inbox}
          title={t("noTaskTitle")}
          description={t("noTaskHint")}
          action={
            <button className="button button-primary" onClick={onNewTask}>
              <Plus size={15} />
              {t("newSession")}
            </button>
          }
        />
      </section>
    );
  }

  const pendingForTask = approvals.filter(
    (approval) =>
      approval.taskId === task.taskId
      && approval.status === "Pending"
      && !["Completed", "Cancelled", "Failed", "Expired"].includes(task.status),
  );
  const terminal = ["Completed", "Cancelled", "Failed", "Expired"].includes(task.status);
  const modelValue = task.selectedModel
    ? `${task.selectedModel.provider}:${task.selectedModel.modelId}`
    : "";

  return (
    <section className="conv">
      <header className="conv-head">
        <button className="icon-button mobile-only" onClick={onMenu} aria-label="打开导航">
          <Menu size={18} />
        </button>
        <div className="conv-head-main">
          <strong className="conv-title">{task.title}</strong>
          <StatusBadge status={task.status} />
          <span className="conv-scope">{scopeLabel(task)}</span>
        </div>
        {models.length ? (
          <select
            className="model-select hide-narrow"
            aria-label={t("defaultModel")}
            value={modelValue}
            onChange={(event) => void changeModel(event.target.value)}
            disabled={submitting}
          >
            <option value="">{t("defaultModel")}</option>
            {models.map((model) => (
              <option key={`${model.provider}:${model.modelId}`} value={`${model.provider}:${model.modelId}`}>
                {model.provider} / {model.modelId}
              </option>
            ))}
          </select>
        ) : null}
        <div className="conv-mode" role="tablist" aria-label={locale === "en" ? "Feed mode" : "展示形式"}>
          <button
            className={mode === "conversation" ? "conv-mode-active" : ""}
            onClick={() => setMode("conversation")}
            role="tab"
            aria-selected={mode === "conversation"}
          >
            <MessageSquare size={13} />
            {t("modeConversation")}
          </button>
          <button
            className={mode === "events" ? "conv-mode-active" : ""}
            onClick={() => setMode("events")}
            role="tab"
            aria-selected={mode === "events"}
          >
            <FileClock size={13} />
            {t("modeEvents")}
          </button>
        </div>
        <div className="conv-controls">
          {task.status === "Paused" ? (
            <button className="icon-button" onClick={() => void control("resume")} disabled={submitting} title={t("resume")}>
              <Play size={15} />
            </button>
          ) : (
            <button
              className="icon-button"
              onClick={() => void control("pause")}
              disabled={submitting || terminal}
              title={t("pause")}
            >
              <Pause size={15} />
            </button>
          )}
          <button
            className="icon-button conv-control-stop"
            onClick={() => void stop()}
            disabled={submitting || terminal}
            title={t("stop")}
          >
            <CircleStop size={15} />
          </button>
          {!task.isMain && (
            <button className="icon-button hide-narrow" onClick={() => void rename()} disabled={submitting} title={t("rename")}>
              <Pencil size={14} />
            </button>
          )}
          {!task.isMain && (
            <button
              className="icon-button conv-control-danger hide-narrow"
              onClick={() => void remove()}
              disabled={submitting}
              title={t("remove")}
            >
              <Trash2 size={14} />
            </button>
          )}
          <button className="icon-button" onClick={() => void refreshSession()} disabled={loading} title={t("refresh")}>
            <RefreshCw className={loading ? "spin" : undefined} size={15} />
          </button>
        </div>
      </header>

      <div
        ref={feedRef}
        onScroll={trackFeedScroll}
        className={`conv-feed ${mode === "events" ? "conv-feed-events" : ""}`}
      >
        {loading ? (
          <div className="conv-loading">{t("loadingFeed")}</div>
        ) : mode === "events" ? (
          events.length ? (
            eventItems.map((item) =>
              item.type === "tool-group" ? (
                <ToolEventGroupLine key={`tool:${item.proposalEventId}`} events={item.events} />
              ) : (
                <EventLine key={item.event.id} event={item.event} />
              ),
            )
          ) : (
            <EmptyState icon={FileClock} title={t("allEvents")} description={t("noMessages")} />
          )
        ) : conversationEvents.length ? (
          conversationItems.map((item) => <ConversationFeedMessage key={feedItemKey(item)} item={item} />)
        ) : (
          <EmptyState icon={MessageSquare} title={t("noMessages")} description={t("typePlaceholder")} />
        )}
      </div>

      {pendingForTask.length ? (
        <div className="conv-approvals">
          {pendingForTask.map((approval) => (
            <ApprovalCard
              key={approval.approvalRequestEventId}
              approval={approval}
              task={task}
              onApproval={onApproval}
              busy={approvalBusy === approval.approvalRequestEventId}
              compact
            />
          ))}
        </div>
      ) : null}

      <form className="composer" onSubmit={sendMessage}>
        <textarea
          value={message}
          onChange={(event) => setMessage(event.target.value)}
          onKeyDown={onComposerKeyDown}
          onInput={(event) => autoGrow(event.currentTarget)}
          placeholder={t("typePlaceholder")}
          rows={1}
          disabled={submitting}
        />
        <div className="composer-row">
          <PermissionSelector
            value={suggestedPermission}
            maximum={maximumPermission}
            onChange={onPermissionChange}
          />
          <span className="composer-hint hide-narrow">{t("enterHint")}</span>
          <button
            className="button button-primary composer-send"
            type="submit"
            disabled={submitting || !message.trim()}
          >
            {submitting ? <LoaderCircle className="spin" size={15} /> : <Send size={15} />}
            {submitting ? t("sending") : t("send")}
          </button>
        </div>
      </form>
    </section>
  );
}

function feedItemKey(item: ConversationFeedItem): string {
  return item.type === "tool-group" ? `tool:${item.proposalEventId}` : item.event.id;
}

function ConversationFeedMessage({ item }: { item: ConversationFeedItem }) {
  return item.type === "tool-group" ? (
    <ToolEventGroupMessage events={item.events} />
  ) : (
    <ConversationMessage event={item.event} />
  );
}

function ToolEventGroupMessage({ events }: { events: TaskEvent[] }) {
  const { locale } = useI18n();
  const proposal = events.find((event) => event.id === event.toolProposalEventId) ?? events[0];
  const latest = events[events.length - 1] ?? proposal;
  if (!proposal || !latest) return null;

  const detailEvents = events.filter((event) => event.id !== proposal.id);
  return (
    <article className="msg-tool msg-tool-group">
      <span className="msg-tool-icon">
        <Terminal size={13} />
      </span>
      <div className="msg-tool-copy">
        <div className="msg-tool-group-head">
          <strong>{proposal.summary}</strong>
          <span className="msg-tool-group-status">{latest.title}</span>
        </div>
        <p>{latest.id === proposal.id ? proposal.title : latest.summary}</p>
        {detailEvents.length ? (
          <details className="msg-tool-details">
            <summary>已合并 {events.length} 个工具事件</summary>
            <div className="msg-tool-timeline">
              {events.map((event) => (
                <div className="msg-tool-step" key={event.id}>
                  <strong>
                    {event.sequence.toString().padStart(3, "0")} · {event.title}
                  </strong>
                  <span>{event.summary}</span>
                </div>
              ))}
            </div>
          </details>
        ) : null}
      </div>
      <time>{formatRelative(latest.occurredAt, locale)}</time>
    </article>
  );
}

/** 事件模式保留每个原始步骤，但将同一工具调用折叠成一条可展开的记录。 */
function ToolEventGroupLine({ events }: { events: TaskEvent[] }) {
  const { locale } = useI18n();
  const proposal = events.find((event) => event.id === event.toolProposalEventId) ?? events[0];
  const latest = events[events.length - 1] ?? proposal;
  if (!proposal || !latest) return null;

  const stepLabel = locale === "en" ? `${events.length} tool events` : `${events.length} 个工具事件`;
  return (
    <details className="event-line-group">
      <summary className="event-line event-group-summary">
        <EventIcon kind="tool" />
        <div className="event-line-main">
          <div className="event-line-top">
            <strong>{proposal.sequence.toString().padStart(3, "0")} · {proposal.summary}</strong>
            <time>{formatRelative(latest.occurredAt, locale)}</time>
          </div>
          <p>{latest.title} · {latest.summary}</p>
        </div>
        <span className="event-line-meta">{stepLabel}</span>
      </summary>
      <div className="event-group-timeline">
        {events.map((event) => (
          <div className="event-group-step" key={event.id}>
            <span>{event.sequence.toString().padStart(3, "0")}</span>
            <div>
              <strong>{event.title}</strong>
              <EventSummary event={event} />
            </div>
          </div>
        ))}
      </div>
    </details>
  );
}

function ConversationMessage({ event }: { event: TaskEvent }) {
  const { locale } = useI18n();
  if (event.kind === "ingress") {
    return (
      <article className="msg msg-user">
        <p>{event.summary}</p>
        <time>{formatRelative(event.occurredAt, locale)}</time>
      </article>
    );
  }
  if (event.kind === "tool") {
    return (
      <article className="msg-tool">
        <span className="msg-tool-icon">
          <Terminal size={13} />
        </span>
        <div className="msg-tool-copy">
          <strong>{event.title}</strong>
          <p>{event.summary}</p>
        </div>
        <time>{formatRelative(event.occurredAt, locale)}</time>
      </article>
    );
  }
  const failed = event.title === "模型调用失败";
  return (
    <article className={`msg msg-agent ${failed ? "msg-agent-error" : ""}`}>
      {failed ? <strong>{event.title}</strong> : null}
      <p>{event.summary}</p>
      <time>{formatRelative(event.occurredAt, locale)}</time>
    </article>
  );
}

function autoGrow(el: HTMLTextAreaElement) {
  el.style.height = "auto";
  el.style.height = `${Math.min(el.scrollHeight, 160)}px`;
}
