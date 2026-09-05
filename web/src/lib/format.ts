import type { Locale } from "../i18n";
import type { TaskSummary } from "../api/types";

/** 相对时间展示；文案随界面语言切换。 */
export function formatRelative(date: string, locale: Locale = "zh-CN"): string {
  const minutes = Math.max(0, Math.floor((Date.now() - new Date(date).getTime()) / 60_000));
  if (locale === "en") {
    if (minutes < 1) return "just now";
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    return `${Math.floor(hours / 24)}d ago`;
  }
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}

export function formatNumber(value: number, locale: Locale = "zh-CN"): string {
  return new Intl.NumberFormat(locale).format(value);
}

/** 任务 ID 的短展示形式；主会话等语义化名称由调用方决定。 */
export function shortId(value: string): string {
  return `${value.slice(0, 8)}…${value.slice(-4)}`;
}

export function scopeLabel(task: Pick<TaskSummary, "scope">): string {
  return `${task.scope.kind}:${task.scope.id}`;
}
