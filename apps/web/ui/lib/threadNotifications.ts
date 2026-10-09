import { conversationStorageKey, conversationStorageScope, subscribeConversationStorageScope } from "./conversationStorageScope";
import { desktopChatBridge, type DesktopNativeState, type DesktopNotificationEvent, type DesktopNotificationScope, type DesktopNotificationStage, type DesktopNotificationStatus } from "./desktopChatBridge";
import { API } from "./useRun";
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
  /** Thread that owns pending_kind: this Thread, or a subagent descendant. */
  pending_thread_id?: string | null;
  /** Thread whose row carries this Thread's attention (the root for subagent children). */
  attention_owner_id?: string;
  /** Blocking pending items of active subagent descendants, surfaced on the root. */
  subagent_attention?: SubagentAttention[];
  title?: string;
  preview?: string;
  updated_at?: string;
  fingerprint?: string;
}

export interface SubagentAttention {
  thread_id: string;
  parent_thread_id?: string | null;
  depth?: number;
  title?: string;
  pending_kind: ThreadPendingKind;
  pending_id?: string | null;
}

export interface ConversationInboxEvent {
  event_id: string;
  broker_epoch?: string;
  seq: number;
  kind: "attention.updated" | "attention.cleared" | string;
  summary: ThreadAttentionSummary;
  thread?: { state?: { read_stream_seq?: number } };
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
let memoryPrefs: NotificationPrefs | null = null;
let prefsDirty = false;
const prefsListeners = new Set<(prefs: NotificationPrefs) => void>();
let prefsStorageListener = false;
export interface NotificationDeliveryDiagnostic {
  status: DesktopNotificationStage | "submitting"; host: "desktop-client";
  threadId: string; eventId: string; dedupeKey: string; id?: string; seq?: number;
  shown?: boolean; code?: string; message?: string;
  connectionVersion?: number; serviceId?: string; identityId?: string;
  history: Array<{ status: DesktopNotificationStage | "submitting"; at: string; code?: string; message?: string }>;
}
let deliveryDiagnostic: NotificationDeliveryDiagnostic | null = null;
const deliveryListeners = new Set<(diagnostic: NotificationDeliveryDiagnostic | null) => void>();
const pendingDesktop = new Map<string, { owner: string; transport: string; epoch: number; input?: DesktopNotificationScope; id?: string; outcomeUnknown?: boolean; admitted?: boolean; shown?: boolean }>();
const soundPlayed = new Set<string>();
let nativeState: DesktopNativeState | null = null;
let nativeEpoch = 0, lastNativeSeq = 0;
let subscribedBridge: ReturnType<typeof desktopChatBridge>;
let offNativeState: (() => void) | undefined, offNativeNotification: (() => void) | undefined;
export function readNotificationDelivery(): NotificationDeliveryDiagnostic | null { return deliveryDiagnostic; }
export function subscribeNotificationDelivery(listener: (diagnostic: NotificationDeliveryDiagnostic | null) => void): () => void {
  deliveryListeners.add(listener); return () => { deliveryListeners.delete(listener); };
}
function publishDelivery(value: NotificationDeliveryDiagnostic | null): void {
  deliveryDiagnostic = value;
  for (const listener of deliveryListeners) listener(value);
  if (value?.status === "failed" || value?.status === "outcome_unknown") console.error("muteki.notification", JSON.stringify(value));
}
function nativeStateKey(state: DesktopNativeState | null): string {
  return JSON.stringify([state?.connectionVersion, state?.serviceId, state?.identityId, state?.transportOrigin]);
}
function acceptNativeState(state: DesktopNativeState): void {
  if (nativeState && nativeStateKey(nativeState) !== nativeStateKey(state)) {
    nativeEpoch++; pendingDesktop.clear(); soundPlayed.clear(); lastNativeSeq = 0; publishDelivery(null);
  }
  nativeState = state;
}
const deliveryStages: DesktopNotificationStage[] = ["submitted", "awaiting_show", "shown", "failed", "outcome_unknown", "clicked", "closed"];
function validNativeDelivery(value: unknown): value is DesktopNotificationEvent {
  const row = value as DesktopNotificationEvent | null;
  return Boolean(row && typeof row.id === "string" && row.id && typeof row.threadId === "string" && typeof row.eventId === "string"
    && typeof row.dedupeKey === "string" && Number.isSafeInteger(row.seq) && row.seq > 0 && typeof row.shown === "boolean"
    && deliveryStages.includes(row.status) && Array.isArray(row.history) && row.history.every(item => item && deliveryStages.includes(item.status)
      && typeof item.at === "string" && (item.message == null || typeof item.message === "string")));
}
function currentNativeDelivery(value: unknown): value is DesktopNotificationEvent {
  const row = value as DesktopNotificationEvent | null;
  return Boolean(row && nativeState && nativeState.transportOrigin === API && conversationStorageScope() === `${row.serviceId}:${row.identityId}`
    && nativeState.connectionVersion === row.connectionVersion && nativeState.serviceId === row.serviceId && nativeState.identityId === row.identityId);
}
function acceptNativeDelivery(value: DesktopNotificationEvent): void {
  if (!currentNativeDelivery(value)) return;
  if (!validNativeDelivery(value)) throw new Error(`desktop.notification_reply_invalid: ${JSON.stringify(value)}`);
  if (value.seq <= lastNativeSeq) return;
  lastNativeSeq = value.seq;
  const pending = pendingDesktop.get(value.dedupeKey);
  if (pending?.input && pending.input.connectionVersion === value.connectionVersion && pending.input.serviceId === value.serviceId
    && pending.input.identityId === value.identityId && (!pending.id || pending.id === value.id)) {
    pending.id = value.id;
    if (value.status !== "outcome_unknown") pending.admitted = true;
    if (value.shown) pending.shown = true;
    if (pending.outcomeUnknown && (value.status === "submitted" || value.status === "awaiting_show")) return;
    if (value.status === "outcome_unknown") pending.outcomeUnknown = true;
    if (value.shown) { rememberNotificationDedupe(value.dedupeKey); pendingDesktop.delete(value.dedupeKey); }
    else if (value.status === "failed" || value.status === "closed") {
      // A closed receipt is final. Known policy suppression handles the episode,
      // but never asserts that a notification was displayed or delivered.
      if (value.status === "closed" && ["desktop.notifications_system_denied", "desktop.notifications_workspace_denied", "desktop.notifications_unsupported", "desktop.notification_thread_visible"].includes(value.code || "")) rememberNotificationDedupe(value.dedupeKey);
      pendingDesktop.delete(value.dedupeKey);
    }
  }
  publishDelivery({ ...value, host: "desktop-client", history: value.history.map(item => ({ ...item })) });
}
function reportNativeDeliveryError(value: unknown, error: unknown): void {
  const message = error instanceof Error ? error.stack || error.message : String(error);
  console.error("muteki.notification", message);
  // Only a receipt carrying this exact owner and connection can be attributed
  // to the visible workspace. Unattributable payloads remain in local logs.
  if (!currentNativeDelivery(value)) return;
  if (Number.isSafeInteger(value.seq) && value.seq > 0) {
    if (value.seq <= lastNativeSeq) return;
    lastNativeSeq = value.seq;
  }
  const pending = pendingDesktop.get(value.dedupeKey);
  if (pending?.input && pending.input.connectionVersion === value.connectionVersion && pending.input.serviceId === value.serviceId
    && pending.input.identityId === value.identityId) pending.outcomeUnknown = true;
  const code = "desktop.notification_reply_invalid";
  publishDelivery({ host: "desktop-client", status: "outcome_unknown", shown: false,
    connectionVersion: value.connectionVersion, serviceId: value.serviceId, identityId: value.identityId,
    threadId: typeof value.threadId === "string" ? value.threadId : "",
    eventId: typeof value.eventId === "string" ? value.eventId : "",
    dedupeKey: typeof value.dedupeKey === "string" ? value.dedupeKey : "", code, message,
    history: [{ status: "outcome_unknown", at: new Date().toISOString(), code, message }] });
}
function subscribeNativeNotifications(): void {
  const bridge = desktopChatBridge();
  if (bridge === subscribedBridge) return;
  offNativeState?.(); offNativeNotification?.(); subscribedBridge = bridge;
  offNativeState = bridge?.onState?.(acceptNativeState);
  offNativeNotification = bridge?.onNotification?.(value => {
    try { acceptNativeDelivery(value); }
    catch (error) { reportNativeDeliveryError(value, error); }
  });
}
function queueDesktopNotification(event: ConversationInboxEvent, dedupeKey: string, audioPlayed: boolean, wantSound = false): Promise<boolean> {
  subscribeNativeNotifications();
  const bridge = desktopChatBridge(), owner = conversationStorageScope(), transport = API, epoch = nativeEpoch;
  const pending = { owner, transport, epoch } as { owner: string; transport: string; epoch: number; input?: DesktopNotificationScope; id?: string; outcomeUnknown?: boolean; admitted?: boolean; shown?: boolean };
  pendingDesktop.set(dedupeKey, pending);
  const initial: NotificationDeliveryDiagnostic = { host: "desktop-client", status: "submitting", threadId: event.summary.thread_id,
    eventId: event.event_id, dedupeKey, shown: false,
    ...(nativeState?.transportOrigin === transport ? { connectionVersion: nativeState.connectionVersion, serviceId: nativeState.serviceId, identityId: nativeState.identityId } : {}),
    history: [{ status: "submitting", at: new Date().toISOString() }] };
  publishDelivery(initial);
  const sameScope = () => owner === conversationStorageScope() && API === transport && epoch === nativeEpoch;
  const current = () => pendingDesktop.get(dedupeKey) === pending && sameScope();
  const admitted = (async () => {
    try {
      if (!owner || !bridge?.getState || !bridge.sendNotification || !bridge.onNotification || !bridge.onState) throw new Error("desktop.notifications_unavailable: Native notification delivery is unavailable.");
      const state = await bridge.getState();
      if (!current()) return false;
      if (state.transportOrigin !== transport || owner !== `${state.serviceId}:${state.identityId}` || !state.connectionVersion) throw new Error("desktop.notification_scope_changed: Notification workspace changed.");
      acceptNativeState(state);
      if (!current()) return false;
      pending.input = { connectionVersion: state.connectionVersion, serviceId: state.serviceId!, identityId: state.identityId! };
      const reply = await bridge.sendNotification({ ...pending.input, threadId: event.summary.thread_id, eventId: event.event_id, dedupeKey,
        title: event.summary.title || "Muteki 会话", body: notificationBody(event.summary), wantSound });
      if (!sameScope()) return false;
      if (!validNativeDelivery(reply) || (reply.eventId !== event.event_id && reply.requestedEventId !== event.event_id) || reply.dedupeKey !== dedupeKey || reply.threadId !== event.summary.thread_id
        || reply.connectionVersion !== pending.input.connectionVersion || reply.serviceId !== pending.input.serviceId || reply.identityId !== pending.input.identityId
        || (pending.id && pending.id !== reply.id)) throw Object.assign(new Error(`desktop.notification_reply_invalid: ${JSON.stringify(reply)}`), { code: "desktop.notification_reply_invalid" });
      pending.id = reply.id; acceptNativeDelivery(reply);
      return true;
    } catch (error) {
      if (!sameScope()) return false;
      const code = (error as { code?: string })?.code || "desktop.notification_send_failed";
      const policy = code === 'desktop.notifications_workspace_denied' || code === 'desktop.notifications_unsupported';
      if (policy) {
        rememberNotificationDedupe(dedupeKey);
        pendingDesktop.delete(dedupeKey);
        const message = error instanceof Error ? error.message : String(error);
        publishDelivery({ ...initial, ...pending.input, status: 'closed', code, message, history: [...initial.history, { status: 'closed', at: new Date().toISOString(), code, message }] });
        console.info('muteki.notification.suppressed', JSON.stringify({ code, threadId: event.summary.thread_id, eventId: event.event_id }));
        return true;
      }
      const uncertain = code === "desktop.notification_reply_invalid" && !pending.admitted;
      if (uncertain) pending.outcomeUnknown = true;
      else pendingDesktop.delete(dedupeKey);
      const message = error instanceof Error ? error.stack || error.message : String(error);
      const status = uncertain ? "outcome_unknown" : "failed";
      publishDelivery({ ...initial, ...pending.input, status, shown: pending.shown || false, code, message,
        history: [...initial.history, { status, at: new Date().toISOString(), code, message }] });
      return pending.admitted === true;
    }
  })();
  // A sound cue is independent evidence; it is never an OS display receipt.
  if (audioPlayed) soundPlayed.add(dedupeKey);
  return admitted;
}

function normalizePrefs(parsed: Partial<NotificationPrefs>): NotificationPrefs {
  const mode = parsed.mode;
  const quiet = parsed.quietHours;
  return {
    mode: mode === "off" || mode === "notifications" || mode === "sound" || mode === "notifications-and-sound" ? mode : DEFAULT_PREFS.mode,
    quietHours: { enabled: Boolean(quiet?.enabled), start: typeof quiet?.start === "string" ? quiet.start : DEFAULT_PREFS.quietHours.start, end: typeof quiet?.end === "string" ? quiet.end : DEFAULT_PREFS.quietHours.end },
  };
}

export function subscribeNotificationPrefs(listener: (prefs: NotificationPrefs) => void): () => void {
  prefsListeners.add(listener);
  if (typeof window !== "undefined" && !prefsStorageListener) {
    prefsStorageListener = true;
    window.addEventListener("storage", (event) => {
      if (event.key !== conversationStorageKey(NOTIFICATION_PREFS_KEY)) return;
      // Another window's confirmed preference becomes the shared authority.
      prefsDirty = false;
      memoryPrefs = normalizePrefs(safeParse<Partial<NotificationPrefs>>(event.newValue) || defaultNotificationPrefs());
      for (const callback of prefsListeners) callback(memoryPrefs);
    });
  }
  return () => { prefsListeners.delete(listener); };
}

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
  if (prefsDirty && memoryPrefs) return normalizePrefs(memoryPrefs);
  if (typeof window === "undefined" || !conversationStorageScope()) return normalizePrefs(memoryPrefs || defaultNotificationPrefs());
  try {
    const parsed = safeParse<Partial<NotificationPrefs>>(window.localStorage.getItem(conversationStorageKey(NOTIFICATION_PREFS_KEY)));
    memoryPrefs = normalizePrefs(parsed || defaultNotificationPrefs());
  } catch { /* Preserve the last known preference if optional storage is unavailable. */ }
  return normalizePrefs(memoryPrefs || defaultNotificationPrefs());
}

export function updateNotificationPrefs(patch: { mode?: NotificationMode; quietHours?: Partial<NotificationQuietHours> }): { prefs: NotificationPrefs; persisted: boolean; error?: string } {
  const current = readNotificationPrefs();
  const next = normalizePrefs({ ...current, ...patch, quietHours: { ...current.quietHours, ...patch.quietHours } });
  memoryPrefs = next; prefsDirty = true;
  let error: string | undefined;
  try {
    if (typeof window === "undefined" || !conversationStorageScope()) throw new Error("notification.storage.unavailable: 通知偏好仅保留在当前窗口");
    window.localStorage.setItem(conversationStorageKey(NOTIFICATION_PREFS_KEY), JSON.stringify(next));
    prefsDirty = false;
  } catch (failure) { error = failure instanceof Error ? `${failure.name}: ${failure.message}` : String(failure); }
  for (const listener of prefsListeners) listener(next);
  return { prefs: next, persisted: !prefsDirty, ...(error ? { error } : {}) };
}

export function writeNotificationPrefs(prefs: NotificationPrefs): void { updateNotificationPrefs(prefs); }

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
  // Unread and failure attention last until the user reads the thread, so one
  // read watermark is one episode: later events in it must not notify again.
  const readSeq = event.thread?.state?.read_stream_seq;
  if ((kind === "failed" || (kind === "none" && summary.unread)) && typeof readSeq === "number") {
    return `${summary.thread_id}|${kind === "failed" ? "failed" : "unread"}|read:${readSeq}`;
  }
  if (kind !== "none") {
    return `${summary.thread_id}|${kind}|rev:${summary.revision}`;
  }
  return `${summary.thread_id}|${event.kind}|${event.event_id}`;
}

function loadDedupeRing(): string[] {
  if (typeof window === "undefined") return [];
  let parsed: string[] | null = null;
  try { parsed = safeParse<string[]>(window.localStorage.getItem(conversationStorageKey(NOTIFICATION_DEDUPE_KEY))); } catch { return []; }
  return Array.isArray(parsed)
    ? parsed.filter((item): item is string => typeof item === "string")
    : [];
}

function persistDedupeRing(keys: string[]): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(
      conversationStorageKey(NOTIFICATION_DEDUPE_KEY),
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
  if (summary.attention_owner_id && summary.attention_owner_id !== summary.thread_id) {
    // A subagent child: its root Thread's row carries this attention.
    return { notify: false, reason: "delegated-to-root", dedupeKey };
  }
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
      summary.pending_thread_id || summary.thread_id,
      options.activeThreadId || "",
      !desktopChatBridge() && typeof document !== "undefined" && !document.hasFocus() ? "hidden"
        : options.visibilityState || (typeof document !== "undefined" ? document.visibilityState : "visible"),
    )
  ) {
    return { notify: false, reason: "active-thread", dedupeKey };
  }
  if (memoryDedupe.has(dedupeKey) || loadDedupeRing().includes(dedupeKey)) {
    return { notify: false, reason: "deduped", dedupeKey };
  }
  if (pendingDesktop.has(dedupeKey)) return { notify: false, reason: "desktop-pending", dedupeKey };
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

function playCue(kind: "complete" | "input"): boolean {
  if (!audioUnlocked || !audioCtx) return false;
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
    return true;
  } catch { return false; }
}

export function subagentPendingLabel(item: Pick<SubagentAttention, "title" | "pending_kind">): string {
  const action = item.pending_kind === "approval" ? "等待审批" : "等待你的输入";
  return `子代理 ${item.title || "未命名子代理"} ${action}`;
}

function notificationBody(summary: ThreadAttentionSummary): string {
  if (summary.pending_thread_id && summary.pending_thread_id !== summary.thread_id) {
    const child = summary.subagent_attention?.find((item) => item.thread_id === summary.pending_thread_id);
    if (child) return subagentPendingLabel(child);
  }
  if (summary.pending_kind === "approval") return "需要审批";
  if (summary.pending_kind === "user_input") return "等待你的输入";
  if (summary.pending_kind === "failed") return "回合失败";
  if (summary.unread) return "有新消息";
  return summary.preview || "会话状态已更新";
}

async function requestDesktopSound(event: ConversationInboxEvent, dedupeKey: string): Promise<boolean> {
  const bridge = desktopChatBridge(), owner = conversationStorageScope(), transport = API;
  try {
    if (!bridge?.getState || !bridge.playNotificationSound) throw new Error('desktop.notification_sound_unavailable: Native notification sound is unavailable.');
    const state = await bridge.getState();
    if (owner !== conversationStorageScope() || API !== transport || state.transportOrigin !== transport || owner !== `${state.serviceId}:${state.identityId}` || !state.connectionVersion) return false;
    const input = { connectionVersion: state.connectionVersion, serviceId: state.serviceId!, identityId: state.identityId!, threadId: event.summary.thread_id, eventId: event.event_id, dedupeKey };
    const reply = await bridge.playNotificationSound(input);
    if (reply.connectionVersion !== input.connectionVersion || reply.serviceId !== input.serviceId || reply.identityId !== input.identityId || reply.dedupeKey !== dedupeKey || reply.threadId !== input.threadId || reply.eventId !== input.eventId || !['sound-requested', 'already-requested', 'suppressed'].includes(reply.status)) throw new Error(`desktop.notification_sound_reply_invalid: ${JSON.stringify(reply)}`);
    rememberNotificationDedupe(dedupeKey);
    return true;
  } catch (error) {
    const policy = (error as {code?: string})?.code === 'desktop.notifications_workspace_denied';
    if (policy) { rememberNotificationDedupe(dedupeKey); console.info('muteki.notification.suppressed', JSON.stringify({ code: 'desktop.notifications_workspace_denied', threadId: event.summary.thread_id })); }
    else console.error('muteki.notification.sound', error instanceof Error ? error.stack || error.message : String(error));
    return policy;
  }
}

export function dispatchThreadNotification(
  event: ConversationInboxEvent,
  options: {
    prefs?: NotificationPrefs;
    activeThreadId?: string;
    visibilityState?: DocumentVisibilityState;
    onOpenThread?: (threadId: string) => void;
  } = {},
): { notified: boolean; reason: string; admitted?: Promise<boolean> } {
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

  if (desktopChatBridge()) {
    if (wantDesktop) return { notified: false, reason: "desktop-submitting", admitted: queueDesktopNotification(event, decision.dedupeKey, false, wantSound) };
    if (wantSound) return { notified: false, reason: "desktop-sound-requesting", admitted: requestDesktopSound(event, decision.dedupeKey) };
  }

  let delivered = false;
  if (wantSound) {
    delivered = !soundPlayed.has(decision.dedupeKey) && playCue(
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
      delivered = true;
      note.onclick = () => {
        window.focus();
        options.onOpenThread?.(summary.thread_id);
        note.close();
      };
    } catch {
      // Desktop notifications may be blocked by the browser even after grant.
    }
  }

  if (!delivered) return { notified: false, reason: "delivery-unavailable" };
  rememberNotificationDedupe(decision.dedupeKey);
  return { notified: true, reason: "ok" };
}

export interface NotificationPermissionStatus {
  permission: NotificationPermission | "unsupported";
  host: "desktop-client" | "browser";
  workspacePermission?: NotificationPermission;
  systemPermission?: "unknown" | "default" | "granted" | "denied" | "provisional" | "ephemeral";
  systemNotificationSettings?: { authorizationStatus: string; authorizationStatusRaw: number; alertSettingRaw?: number; soundSettingRaw?: number };
  systemPermissionError?: { code: string; message: string; detail?: string };
  code?: string;
}

async function desktopNotificationPermission(request: boolean): Promise<DesktopNotificationStatus> {
  subscribeNativeNotifications();
  const bridge = desktopChatBridge(), owner = conversationStorageScope(), transport = API, epoch = nativeEpoch;
  const unchanged = () => owner === conversationStorageScope() && transport === API && epoch === nativeEpoch;
  if (!bridge?.getState || !bridge.notificationStatus || !bridge.requestNotifications) {
    throw new Error("desktop.notifications_unavailable: 桌面通知授权接口不可用 / Desktop notification permission API is unavailable");
  }
  const state = await bridge.getState();
  if (!owner || !unchanged() || state.transportOrigin !== transport || owner !== `${state.serviceId}:${state.identityId}`
    || !Number.isSafeInteger(state.connectionVersion) || !state.connectionVersion) {
    throw new Error("desktop.notification_scope_changed: 通知所属工作台已改变 / Notification workspace changed");
  }
  acceptNativeState(state);
  if (!unchanged()) throw new Error("desktop.notification_scope_changed: 通知所属工作台已改变 / Notification workspace changed");
  const input: DesktopNotificationScope = { connectionVersion: state.connectionVersion!, serviceId: state.serviceId!, identityId: state.identityId! };
  const result = await (request ? bridge.requestNotifications(input) : bridge.notificationStatus(input));
  if (!unchanged()) throw new Error("desktop.notification_scope_changed: 通知所属工作台已改变 / Notification workspace changed");
  const current = await bridge.getState();
  if (!unchanged() || current.transportOrigin !== transport || input.connectionVersion !== current.connectionVersion
    || input.serviceId !== current.serviceId || input.identityId !== current.identityId) {
    throw new Error("desktop.notification_scope_changed: 通知所属工作台已改变 / Notification workspace changed");
  }
  if (!result || result.host !== "desktop-client" || !["unknown", "default", "granted", "denied", "provisional", "ephemeral"].includes(result.systemPermission)
    || result.connectionVersion !== input.connectionVersion || result.serviceId !== input.serviceId || result.identityId !== input.identityId
    || !["default", "granted", "denied", "unsupported"].includes(result.permission)
    || !["default", "granted", "denied"].includes(result.workspacePermission) || typeof result.code !== "string"
    || (result.permission !== "unsupported" && result.permission !== result.workspacePermission)) {
    throw new Error(`desktop.notification_reply_invalid: ${JSON.stringify(result)}`);
  }
  acceptNativeState(current);
  if (!unchanged()) throw new Error("desktop.notification_scope_changed: 通知所属工作台已改变 / Notification workspace changed");
  if (result.delivery) {
    try { acceptNativeDelivery(result.delivery); }
    catch (error) { reportNativeDeliveryError(result.delivery, error); throw error; }
  }
  return result;
}

export async function readNotificationPermissionStatus(): Promise<NotificationPermissionStatus> {
  if (desktopChatBridge()) return desktopNotificationPermission(false);
  return { host: "browser", permission: typeof window === "undefined" || !("Notification" in window) ? "unsupported" : Notification.permission };
}

export async function requestNotificationPermissionStatus(): Promise<NotificationPermissionStatus> {
  if (desktopChatBridge()) return desktopNotificationPermission(true);
  return { host: "browser", permission: await requestNotificationPermission() };
}

export async function requestNotificationPermission(): Promise<NotificationPermission | "unsupported"> {
  // Desktop workspace consent is explicit IPC. Chromium's default denial is
  // not the user's workspace decision and must never bypass that request.
  if (desktopChatBridge()) return (await desktopNotificationPermission(true)).permission;
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

subscribeConversationStorageScope(() => { nativeEpoch++; pendingDesktop.clear(); soundPlayed.clear(); lastNativeSeq = 0; publishDelivery(null); memoryPrefs = null; prefsDirty = false; memoryDedupe.clear(); for (const listener of prefsListeners) listener(defaultNotificationPrefs()); });
