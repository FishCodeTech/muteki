/**
 * Settle / snooze rules for the conversation sidebar.
 *
 * Settled threads leave the active list until they produce new activity.
 * Snoozed threads hide until their wake time, or earlier when they need the
 * user (approval / input). A woken thread keeps a "已唤醒" marker until opened.
 */

import type { ConversationThread } from "./useConversation";
import { threadNeedsAction } from "./threadAttention";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;
/** Server timestamps and local clocks drift; activity must clearly postdate the settle. */
const SETTLE_ACTIVITY_SLACK_MS = 2_000;

export type InboxPlacement = "active" | "snoozed" | "settled";

export interface InboxState {
  placement: InboxPlacement;
  /** Snooze elapsed (or ended early) and the user has not opened the thread yet. */
  woke: boolean;
  snoozedUntil?: number;
  settledAt?: number;
}

function updatedMs(thread: ConversationThread): number {
  return Date.parse(String(thread.updated_at || thread.created_at || "")) || 0;
}

export function inboxStateOf(
  thread: ConversationThread,
  settledAt: Record<string, number>,
  snoozedUntil: Record<string, number>,
  nowMs: number,
  /** A subagent descendant is waiting on the user; it resurfaces this root Thread. */
  subagentNeedsAction = false,
): InboxState {
  const needsAction = threadNeedsAction(thread) || subagentNeedsAction;
  const until = snoozedUntil[thread.thread_id];
  if (until) {
    const due = until <= nowMs || needsAction;
    return due
      ? { placement: "active", woke: true, snoozedUntil: until }
      : { placement: "snoozed", woke: false, snoozedUntil: until };
  }
  const settled = settledAt[thread.thread_id];
  if (settled) {
    const resurfaced = Boolean(thread.state.running_turn_id)
      || needsAction
      || updatedMs(thread) > settled + SETTLE_ACTIVITY_SLACK_MS;
    if (!resurfaced) return { placement: "settled", woke: false, settledAt: settled };
  }
  return { placement: "active", woke: false };
}

/** Earliest future wake time, so the sidebar can re-render exactly then. */
export function nextWakeAt(snoozedUntil: Record<string, number>, nowMs: number): number | null {
  let next: number | null = null;
  for (const until of Object.values(snoozedUntil)) {
    if (until > nowMs && (next === null || until < next)) next = until;
  }
  return next;
}

/** Drop entries for threads that no longer exist in the list. */
export function pruneTimeMap(map: Record<string, number>, liveIds: Set<string>): Record<string, number> {
  const result: Record<string, number> = {};
  for (const [id, value] of Object.entries(map)) if (liveIds.has(id)) result[id] = value;
  return result;
}

export interface SnoozePreset {
  id: string;
  label: string;
  until: number;
}

function at(base: Date, dayOffset: number, hour: number): number {
  const next = new Date(base.getFullYear(), base.getMonth(), base.getDate() + dayOffset, hour, 0, 0, 0);
  return next.getTime();
}

export function snoozePresets(now: Date = new Date()): SnoozePreset[] {
  const nowMs = now.getTime();
  const presets: SnoozePreset[] = [
    { id: "1h", label: "1 小时后", until: nowMs + HOUR },
    { id: "3h", label: "3 小时后", until: nowMs + 3 * HOUR },
  ];
  if (now.getHours() < 17) presets.push({ id: "evening", label: "今晚", until: at(now, 0, 18) });
  presets.push({ id: "tomorrow", label: "明天", until: at(now, 1, 9) });
  const daysToMonday = ((8 - now.getDay()) % 7) || 7;
  presets.push({ id: "next-week", label: "下周", until: at(now, daysToMonday, 9) });
  return presets;
}

const WEEKDAYS = ["周日", "周一", "周二", "周三", "周四", "周五", "周六"];

function clock(date: Date): string {
  return `${String(date.getHours()).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}

/** "今天 18:00" / "明天 09:00" / "周一 09:00" / "10月12日 09:00". */
export function formatWakeTime(until: number, now: Date = new Date()): string {
  const date = new Date(until);
  const dayStart = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const diffDays = Math.floor((new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime() - dayStart) / DAY);
  if (diffDays === 0) return `今天 ${clock(date)}`;
  if (diffDays === 1) return `明天 ${clock(date)}`;
  if (diffDays > 1 && diffDays < 7) return `${WEEKDAYS[date.getDay()]} ${clock(date)}`;
  return `${date.getMonth() + 1}月${date.getDate()}日 ${clock(date)}`;
}

/** Value for <input type="datetime-local"> in local time. */
export function toLocalInputValue(ms: number): string {
  const date = new Date(ms);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}
