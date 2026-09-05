import type { TaskEvent } from "../api/types";

export type ConversationFeedItem =
  | { type: "event"; event: TaskEvent }
  | { type: "tool-group"; proposalEventId: string; events: TaskEvent[] };

/**
 * 将属于同一个原始工具提议的事件合并为一个对话项。
 *
 * 事件审计需要保留逐条事件，因此这里只提供对话视图使用的投影；原始数组不会被
 * 修改，无法解析关联关系的旧事件也会作为独立事件保留。
 */
export function groupConversationEvents(events: TaskEvent[]): ConversationFeedItem[] {
  const items: ConversationFeedItem[] = [];
  const groups = new Map<string, Extract<ConversationFeedItem, { type: "tool-group" }>>();

  for (const event of events) {
    const proposalEventId = event.toolProposalEventId ?? undefined;
    if (!proposalEventId) {
      items.push({ type: "event", event });
      continue;
    }

    const existing = groups.get(proposalEventId);
    if (existing) {
      existing.events.push(event);
      continue;
    }

    const group: Extract<ConversationFeedItem, { type: "tool-group" }> = {
      type: "tool-group",
      proposalEventId,
      events: [event],
    };
    groups.set(proposalEventId, group);
    items.push(group);
  }

  return items;
}
