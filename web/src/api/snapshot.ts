import type { SystemSnapshot } from "./types";

/**
 * 主会话使用固定的全零任务 ID；后端以同一约定投影主会话事件流。
 */
export const MAIN_TASK_ID = "00000000-0000-0000-0000-000000000000";

/**
 * 后端不可用时的空壳数据。不能回退到演示任务，否则真实部署故障会被伪装成
 * 一套看似可操作的界面。
 */
export function createEmptySnapshot(): SystemSnapshot {
  return {
    generatedAt: new Date().toISOString(),
    health: {
      api: "offline",
      eventStore: "offline",
      modelProvider: "offline",
      lastHeartbeatAt: new Date().toISOString(),
    },
    tasks: [],
    approvals: [],
    recentEvents: [],
    tools: [],
    models: [],
    defaultModel: null,
    usage: {
      inputTokensToday: 0,
      outputTokensToday: 0,
      monthSpentUsd: 0,
      monthlyBudgetUsd: 0,
      daily: [],
    },
  };
}
