/**
 * Configurable Conversation thread notifications (C29).
 *
 * Modes mirror T3 Code: off / notifications / sound / both.
 * Prefs stay in localStorage (server sync is #17 territory).
 */

export type NotificationMode =
  | "off"
  | "notifications"
  | "sound"
  | "notifications-and-sound";

export type ThreadPendingKind =
  | "none"
  | "approval"
  | "user_input"
  | "failed"
  | string;

export interface NotificationQuietHours {
  enabled: boolean;
  /** Local time HH:MM */
  start: string;
  /** Local time HH:MM */
  end: string;
}

export interface NotificationPrefs {
  mode: NotificationMode;
  quietHours: NotificationQuietHours;
}

export interface ThreadAttentionSummary {
  thread_id: string;
  revision: number;
  status: string;
  running: boolean;
  unread: boolean;
  pending_kind: ThreadPendingKind;
  pending_id?: string | null;
  title?: string;
  preview?: string;
  updated_at?: string;
  fingerprint?: string;
}

export interface ConversationInboxEvent {
  event_id: string;
  seq: number;
  kind: "attention.updated" | "attention.cleared" | string;
  summary: ThreadAttentionSummary;
}

export const NOTIFICATION_PREFS_KEY = "muteki.notifications.v1";
export const NOTIFICATION_DEDUPE_KEY = "muteki.notifications.dedupe.v1";

const DEFAULT_PREFS: NotificationPrefs = {
  mode: "notifications",
  quietHours: { enabled: false, start: "22:00", end: "08:00" },
};

const DEDUPE_RING_LIMIT = 200;

let audioCtx: AudioContext | null = null;
let audioUnlocked = false;
const memoryDedupe = new Set<string>();

function safeParse<T>(raw: string | null): T | null {
  if (!raw) return null;
  try {
    return JSON.parse(raw) as T;
  } catch {
    return null;
  }
}

export function defaultNotificationPrefs(): NotificationPrefs {
  return {
    mode: DEFAULT_PREFS.mode,
    quietHours: { ...DEFAULT_PREFS.quietHours },
  };
}

export function readNotificationPrefs(): NotificationPrefs {
  if (typeof window === "undefined") return defaultNotificationPrefs();
  const parsed = safeParse<Partial<NotificationPrefs>>(
    window.localStorage.getItem(NOTIFICATION_PREFS_KEY),
  );
  if (!parsed || typeof parsed !== "object") return defaultNotificationPrefs();
  const mode = parsed.mode;
  const quiet = parsed.quietHours;
  return {
    mode:
      mode === "off"
      || mode === "notifications"
      || mode === "sound"
      || mode === "notifications-and-sound"
        ? mode
        : DEFAULT_PREFS.mode,
    quietHours: {
      enabled: Boolean(quiet?.enabled),
      start: typeof quiet?.start === "string" ? quiet.start : DEFAULT_PREFS.quietHours.start,
      end: typeof quiet?.end === "string" ? quiet.end : DEFAULT_PREFS.quietHours.end,
    },
  };
}

export function writeNotificationPrefs(prefs: NotificationPrefs): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(NOTIFICATION_PREFS_KEY, JSON.stringify(prefs));
  } catch {
    // Prefs are best-effort when storage is unavailable.
  }
}

function parseHm(value: string): number | null {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value.trim());
  if (!match) return null;
  const hour = Number(match[1]);
  const minute = Number(match[2]);
  if (!Number.isFinite(hour) || !Number.isFinite(minute)) return null;
  if (hour < 0 || hour > 23 || minute < 0 || minute > 59) return null;
  return hour * 60 + minute;
}

export function isWithinQuietHours(
  quiet: NotificationQuietHours,
  now: Date = new Date(),
): boolean {
  if (!quiet.enabled) return false;
  const start = parseHm(quiet.start);
  const end = parseHm(quiet.end);
  if (start === null || end === null) return false;
  const current = now.getHours() * 60 + now.getMinutes();
  if (start === end) return true;
  if (start < end) return current >= start && current < end;
  return current >= start || current < end;
}

export function notificationDedupeKey(event: ConversationInboxEvent): string {
  const summary = event.summary;
  const pendingId = String(summary.pending_id || "").trim();
  const kind = String(summary.pending_kind || "none");
  if (kind !== "none" && pendingId) {
    return `${summary.thread_id}|${kind}|${pendingId}`;
  }
  if (kind !== "none") {
    return `${summary.thread_id}|${kind}|rev:${summary.revision}`;
  }
  return `${summary.thread_id}|${event.kind}|${event.event_id}`;
}

function loadDedupeRing(): string[] {
  if (typeof window === "undefined") return [];
  const parsed = safeParse<string[]>(window.localStorage.getItem(NOTIFICATION_DEDUPE_KEY));
  return Array.isArray(parsed)
    ? parsed.filter((item): item is string => typeof item === "string")
    : [];
}

function persistDedupeRing(keys: string[]): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(
      NOTIFICATION_DEDUPE_KEY,
      JSON.stringify(keys.slice(-DEDUPE_RING_LIMIT)),
    );
  } catch {
    // Ignore quota / private-mode failures.
  }
}

export function rememberNotificationDedupe(key: string): boolean {
  if (!key) return false;
  if (memoryDedupe.has(key)) return false;
  const ring = loadDedupeRing();
  if (ring.includes(key)) {
    memoryDedupe.add(key);
    return false;
  }
  memoryDedupe.add(key);
  ring.push(key);
  persistDedupeRing(ring);
  return true;
}

export function clearNotificationDedupeForTests(): void {
  memoryDedupe.clear();
}

export function shouldSuppressForActiveThread(
  threadId: string,
  activeThreadId: string,
  visibilityState: DocumentVisibilityState = "visible",
): boolean {
  return Boolean(
    threadId
    && activeThreadId
    && threadId === activeThreadId
    && visibilityState === "visible",
  );
}

export function shouldNotifyInboxEvent(
  event: ConversationInboxEvent,
  options: {
    prefs?: NotificationPrefs;
    activeThreadId?: string;
    visibilityState?: DocumentVisibilityState;
    now?: Date;
  } = {},
): { notify: boolean; reason: string; dedupeKey: string } {
  const prefs = options.prefs || readNotificationPrefs();
  const dedupeKey = notificationDedupeKey(event);
  if (event.kind === "attention.cleared") {
    return { notify: false, reason: "cleared", dedupeKey };
  }
  const summary = event.summary;
  const interesting =
    summary.pending_kind === "approval"
    || summary.pending_kind === "user_input"
    || summary.pending_kind === "failed"
    || (summary.unread && !summary.running);
  if (!interesting) {
    return { notify: false, reason: "not-interesting", dedupeKey };
  }
  if (prefs.mode === "off") {
    return { notify: false, reason: "mode-off", dedupeKey };
  }
  if (isWithinQuietHours(prefs.quietHours, options.now)) {
    return { notify: false, reason: "quiet-hours", dedupeKey };
  }
  if (
    shouldSuppressForActiveThread(
      summary.thread_id,
      options.activeThreadId || "",
      options.visibilityState || (
        typeof document !== "undefined" ? document.visibilityState : "visible"
      ),
    )
  ) {
    return { notify: false, reason: "active-thread", dedupeKey };
  }
  if (!rememberNotificationDedupe(dedupeKey)) {
    return { notify: false, reason: "deduped", dedupeKey };
  }
  return { notify: true, reason: "ok", dedupeKey };
}

export function unlockNotificationAudio(): void {
  if (typeof window === "undefined") return;
  try {
    const Ctx = window.AudioContext || (window as unknown as {
      webkitAudioContext?: typeof AudioContext;
    }).webkitAudioContext;
    if (!Ctx) return;
    if (!audioCtx) audioCtx = new Ctx();
    void audioCtx.resume().then(() => {
      audioUnlocked = audioCtx?.state === "running";
    });
  } catch {
    audioUnlocked = false;
  }
}

function playCue(kind: "complete" | "input"): void {
  if (!audioUnlocked || !audioCtx) return;
  try {
    const osc = audioCtx.createOscillator();
    const gain = audioCtx.createGain();
    osc.type = "sine";
    osc.frequency.value = kind === "input" ? 880 : 660;
    gain.gain.value = 0.0001;
    osc.connect(gain);
    gain.connect(audioCtx.destination);
    const now = audioCtx.currentTime;
    gain.gain.exponentialRampToValueAtTime(0.05, now + 0.02);
    gain.gain.exponentialRampToValueAtTime(0.0001, now + 0.18);
    osc.start(now);
    osc.stop(now + 0.2);
  } catch {
    // Audio is best-effort.
  }
}

function notificationBody(summary: ThreadAttentionSummary): string {
  if (summary.pending_kind === "approval") return "需要审批";
  if (summary.pending_kind === "user_input") return "等待你的输入";
  if (summary.pending_kind === "failed") return "回合失败";
  if (summary.unread) return "有新消息";
  return summary.preview || "会话状态已更新";
}

export function dispatchThreadNotification(
  event: ConversationInboxEvent,
  options: {
    prefs?: NotificationPrefs;
    activeThreadId?: string;
    visibilityState?: DocumentVisibilityState;
    onOpenThread?: (threadId: string) => void;
  } = {},
): { notified: boolean; reason: string } {
  const decision = shouldNotifyInboxEvent(event, options);
  if (!decision.notify) {
    return { notified: false, reason: decision.reason };
  }
  const prefs = options.prefs || readNotificationPrefs();
  const summary = event.summary;
  const wantDesktop =
    prefs.mode === "notifications" || prefs.mode === "notifications-and-sound";
  const wantSound =
    prefs.mode === "sound" || prefs.mode === "notifications-and-sound";

  if (wantSound) {
    playCue(
      summary.pending_kind === "approval" || summary.pending_kind === "user_input"
        ? "input"
        : "complete",
    );
  }

  if (
    wantDesktop
    && typeof window !== "undefined"
    && "Notification" in window
    && Notification.permission === "granted"
  ) {
    try {
      const note = new Notification(summary.title || "Muteki 会话", {
        body: notificationBody(summary),
        tag: decision.dedupeKey,
      });
      note.onclick = () => {
        window.focus();
        options.onOpenThread?.(summary.thread_id);
        note.close();
      };
    } catch {
      // Desktop notifications may be blocked by the browser even after grant.
    }
  }

  return { notified: true, reason: "ok" };
}

export async function requestNotificationPermission(): Promise<NotificationPermission | "unsupported"> {
  if (typeof window === "undefined" || !("Notification" in window)) {
    return "unsupported";
  }
  if (Notification.permission === "granted" || Notification.permission === "denied") {
    return Notification.permission;
  }
  try {
    return await Notification.requestPermission();
  } catch {
    return Notification.permission;
  }
}
