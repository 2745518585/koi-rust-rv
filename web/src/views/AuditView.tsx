import { useState } from "react";
import { FileClock } from "lucide-react";
import type { EventKind, TaskEvent } from "../api/types";
import { useI18n } from "../i18n";
import { EmptyState, EventLine, labelFor, PanelHeader, eventKindMeta } from "../lib/ui";

const kindOrder: Array<EventKind | "all"> = ["all", "ingress", "control", "model", "tool"];

export function AuditView({
  events,
  onSelectTask,
  onMenu,
}: {
  events: TaskEvent[];
  onSelectTask: (taskId: string) => void;
  onMenu: () => void;
}) {
  const { locale } = useI18n();
  const copy = locale === "en"
    ? {
        title: "Event audit",
        subtitle: "The event stream is the source of truth; statuses, model output and approvals are all traceable.",
        all: "All",
        empty: "No events",
        emptyHint: "Events appear here as tasks and sessions produce them.",
      }
    : {
        title: "事件审计",
        subtitle: "事件流是事实来源；任务状态、模型输出与工具授权都可追溯。",
        all: "全部",
        empty: "暂无事件",
        emptyHint: "任务与会话产生事件后会出现在这里。",
      };

  const [kind, setKind] = useState<EventKind | "all">("all");
  const visible = events.filter((event) => kind === "all" || event.kind === kind);

  return (
    <section className="panel">
      <PanelHeader title={copy.title} description={copy.subtitle} onMenu={onMenu} />
      <div className="panel-body">
        <div className="panel-card">
          <div className="chip-row audit-chip-row">
            {kindOrder.map((value) => (
              <button
                key={value}
                className={`chip ${kind === value ? "chip-active" : ""}`}
                onClick={() => setKind(value)}
              >
                {value === "all" ? copy.all : labelFor(eventKindMeta[value].label, locale)}
              </button>
            ))}
          </div>
          {visible.length ? (
            <div className="event-line-list">
              {visible.map((event) => (
                <EventLine
                  key={event.id}
                  event={event}
                  onClick={() => onSelectTask(event.taskId)}
                />
              ))}
            </div>
          ) : (
            <EmptyState icon={FileClock} title={copy.empty} description={copy.emptyHint} />
          )}
        </div>
      </div>
    </section>
  );
}
