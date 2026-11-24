"use client";

import { useMemo, useState } from "react";
import type { ConversationEvent, ConversationView } from "@/lib/useConversation";
import { useConversationServerPreferences } from "@/lib/conversationServerPreferences";
import { Button, Callout, Switch } from "@muteki/ui";
import { useNow } from "@/components/chat/surfaces/shared";

interface QuotaHold {
  resetsAt: number;
  kind: string;
}

export function latestQuotaHold(events: ConversationEvent[]): QuotaHold | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index];
    if (event.event_type !== "core.runtime.warning") continue;
    const limit = (event.payload as Record<string, unknown> | undefined)?.rate_limit as
      | { limited?: boolean; resets_at?: number | null; kind?: string | null }
      | undefined;
    if (!limit) continue;
    if (!limit.limited || typeof limit.resets_at !== "number") return null;
    return { resetsAt: limit.resets_at, kind: String(limit.kind || "") };
  }
  return null;
}

function formatClock(ms: number): string {
  const date = new Date(ms);
  const sameDay = date.toDateString() === new Date().toDateString();
  return date.toLocaleString("zh-CN", sameDay
    ? { hour: "2-digit", minute: "2-digit", hour12: false }
    : { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });
}

function formatRemaining(ms: number): string {
  const minutes = Math.max(1, Math.ceil(ms / 60_000));
  if (minutes < 60) return `${minutes} 分钟`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest ? `${hours} 小时 ${rest} 分钟` : `${hours} 小时`;
}

export function ConversationQuotaResume({
  view,
  events,
  busy,
  onResume,
  onSchedule,
}: {
  view: ConversationView;
  events: ConversationEvent[];
  busy: boolean;
  onResume: () => void;
  onSchedule: (input: { enabled: boolean; turnId: string; resetsAt: number; kind: string }) => Promise<void>;
}) {
  const serverPrefs = useConversationServerPreferences();
  const hold = useMemo(() => latestQuotaHold(events), [events]);
  const lastTurn = view.turns.at(-1);
  const blocked = Boolean(hold && !view.state.running_turn_id && lastTurn?.status === "failed");
  const schedule = view.state.quota_resume;
  const scheduled = Boolean(blocked && schedule && schedule.turn_id === lastTurn?.turn_id);
  const serverHold = view.state.quota_hold;
  const autoPending = Boolean(
    blocked && !scheduled
    && serverPrefs.status === "ready" && serverPrefs.prefs.autoResumeOnQuotaReset
    && serverHold && serverHold.turn_id === lastTurn?.turn_id && !serverHold.handled,
  );
  const now = useNow(blocked, 15_000);
  const [error, setError] = useState("");
  const [pending, setPending] = useState(false);

  const submit = async (enabled: boolean) => {
    if (!hold || !lastTurn) return;
    setPending(true);
    setError("");
    try {
      await onSchedule({ enabled, turnId: lastTurn.turn_id, resetsAt: hold.resetsAt, kind: hold.kind });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPending(false);
    }
  };

  const [dismissedUnreadableCount, setDismissedUnreadableCount] = useState(0);
  const unreadableRecords = useMemo(() => {
    const rows: string[] = [];
    for (const event of events) {
      if (event.event_type !== "core.runtime.unreadable_record") continue;
      const line = (event.payload as Record<string, unknown> | undefined)?.line;
      rows.push(typeof line === "string" ? line : JSON.stringify(event.payload ?? {}));
    }
    return rows;
  }, [events]);
  const showUnreadable = unreadableRecords.length > dismissedUnreadableCount;
  const unreadableBanner = showUnreadable ? (
    <Callout
      role="status"
      tone="warning"
      icon="alert"
      testId="unreadable-record-banner"
      className="mb-2"
      title={`检测到 ${unreadableRecords.length} 条无法解析的引擎记录`}
      action={(
        <Button
          size="xs"
          variant="ghost"
          onClick={() => setDismissedUnreadableCount(unreadableRecords.length)}
        >
          忽略
        </Button>
      )}
    >
      <span className="block truncate font-cx-mono text-cx-xs">
        {unreadableRecords[unreadableRecords.length - 1]?.slice(0, 180)}
      </span>
    </Callout>
  ) : null;

  if (!blocked || !hold) return unreadableBanner;

  const resetsAtMs = hold.resetsAt * 1000;
  const remaining = resetsAtMs - now;
  const title = remaining > 0
    ? `额度已用尽，将于 ${formatClock(resetsAtMs)} 重置（约 ${formatRemaining(remaining)}后）`
    : `额度已于 ${formatClock(resetsAtMs)} 重置`;
  const detail = error
    ? `安排自动继续失败：${error}`
    : scheduled
      ? remaining > 0
        ? "已在 Muteki 服务上安排：重置后自动继续本对话，关闭窗口也会执行（服务需保持运行）。"
        : "正在等待服务自动继续…"
      : autoPending
        ? "已开启“额度重置后自动继续”，服务将在 30 秒内安排本对话。"
        : "可开启自动继续，或在重置后手动继续。";

  return (
    <>
      {unreadableBanner}
      <Callout
      role="status"
      tone={error ? "danger" : "warning"}
      icon="clock"
      testId="quota-resume-banner"
      className="mb-2"
      title={title}
      action={(
        <div className="flex items-center gap-2">
          <label className="flex items-center gap-1.5 text-cx-xs text-cx-fg-2">
            <Switch
              ariaLabel="额度重置后自动继续"
              checked={scheduled || autoPending}
              disabled={pending}
              onCheckedChange={(next) => { void submit(next); }}
            />
            自动继续
          </label>
          {remaining <= 0 ? (
            <Button size="xs" variant="secondary" icon="play" disabled={busy} onClick={onResume}>继续</Button>
          ) : null}
        </div>
      )}
    >
      {detail}
    </Callout>
    </>
  );
}
