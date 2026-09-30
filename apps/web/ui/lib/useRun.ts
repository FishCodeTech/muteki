"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { DeckState, EventType, MutekiEvent, emptyDeck, reduce } from "./events";
import { deleteRunProjection, loadRunProjection, saveRunProjection } from "./runProjectionCache";
import { setConversationStorageScope } from "./conversationStorageScope";

/**
 * API base. Empty string = same-origin: `run.sh web` serves the production
 * Next UI and proxies /api to the FastAPI backend. NEXT_PUBLIC_MUTEKI_API is
 * still available for manual experiments that intentionally bypass that proxy.
 */
const WEB_API = process.env.NEXT_PUBLIC_MUTEKI_API || "";
export let API = WEB_API;
const CONTROL_CAS_ACTIONS = new Set([
  "pause", "freeze", "resume", "thaw", "stop", "complete",
]);

export class RunStartError extends Error {
  status: number;
  payload: Record<string, unknown>;

  constructor(message: string, status: number, payload: Record<string, unknown> = {}) {
    super(message);
    this.name = "RunStartError";
    this.status = status;
    this.payload = payload;
  }
}

// ---------------------------------------------------------------------------
// Auth (P3): single-password gate. The operator types a password once; the
// backend returns a signed session token we keep in localStorage and attach to
// every /api request. The password itself is never stored. SSE/WS connections
// (which can't carry a header) use a one-time ticket minted via apiFetch.
// ---------------------------------------------------------------------------
const TOKEN_KEY = "muteki_auth_token";
let serviceOrigin = API || (typeof window !== "undefined" ? window.location.origin : "");
let tokenScope = "";
function tokenStorageKey(): string { return `${TOKEN_KEY}:${serviceOrigin}:${tokenScope}`; }
function scopeStorageKey(): string { return `muteki_auth_scope:${serviceOrigin}`; }
if (typeof window !== "undefined") {
  try { tokenScope = window.localStorage.getItem(scopeStorageKey()) || ""; } catch { /* metadata is revalidated */ }
}
let memoryToken: string | undefined;
let authGeneration = 0;
let authScope = "";
let authPersistenceWarning = "";
function updateAuthScope(scope: string): void {
  authScope = scope;
  setConversationStorageScope(scope);
}

export function currentAuthGeneration(): number { return authGeneration; }
export function currentAuthScope(): string { return authScope; }
export function currentAuthPersistenceWarning(): string { return authPersistenceWarning; }

function getToken(): string {
  if (memoryToken !== undefined) return memoryToken;
  if (typeof window === "undefined") return "";
  try { memoryToken = window.localStorage.getItem(tokenStorageKey()) || ""; }
  catch { memoryToken = ""; }
  return memoryToken;
}

function setToken(token: string): void {
  memoryToken = token;
  authGeneration += 1;
  authPersistenceWarning = "";
  if (typeof window === "undefined") return;
  try {
    if (token) window.localStorage.setItem(tokenStorageKey(), token);
    else window.localStorage.removeItem(tokenStorageKey());
  } catch {
    authPersistenceWarning = token
      ? "登录仅保存在当前窗口，存储不可写；重新打开后需要再次登录。"
      : "登录存储不可写；请在关闭窗口后检查本地存储。";
  }
}

// A response may revoke only the authentication generation that issued it.
type AuthListener = (reason?: "expired" | "service_changed" | "peer_changed") => void;
const authListeners = new Set<AuthListener>();
export function onAuthRequired(fn: AuthListener): () => void {
  authListeners.add(fn);
  return () => authListeners.delete(fn);
}
function fireAuthRequired(): void {
  updateAuthScope("");
  setToken("");
  authListeners.forEach((fn) => { try { fn("expired"); } catch { /* listeners are independent */ } });
}

if (typeof window !== "undefined") {
  window.addEventListener("storage", (event) => {
    if (event.key !== tokenStorageKey()) return;
    memoryToken = event.newValue || "";
    updateAuthScope("");
    authGeneration += 1;
    // Revalidate on a different view's login/logout; cached auth is no longer authoritative.
    authListeners.forEach((fn) => { try { fn("peer_changed"); } catch { /* independent listener */ } });
  });
}

export function resetAuthForServiceChange(origin: string, transportOrigin?: string): void {
  const nextApi = transportOrigin ?? WEB_API;
  if (serviceOrigin === origin && API === nextApi) return;
  API = nextApi;
  serviceOrigin = origin;
  tokenScope = "";
  if (typeof window !== "undefined") {
    try { tokenScope = window.localStorage.getItem(scopeStorageKey()) || ""; } catch { /* verified metadata follows */ }
  }
  memoryToken = undefined;
  updateAuthScope("");
  authGeneration += 1;
  authListeners.forEach((fn) => { try { fn("service_changed"); } catch { /* independent listener */ } });
}

export async function apiFetch(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers || {});
  const token = getToken();
  const generation = authGeneration;
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const res = await fetch(`${API}${path}`, { ...init, headers });
  if (res.status === 401 && generation === authGeneration && token === getToken()) fireAuthRequired();
  return res;
}

export async function login(password: string): Promise<{ ok: boolean; authRequired: boolean }> {
  const generation = authGeneration;
  const res = await fetch(`${API}/api/auth/login`, {
    method: "POST", headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
    signal: AbortSignal.timeout(15_000),
  });
  if (res.status === 401) return { ok: false, authRequired: true };
  if (!res.ok) throw new Error(`登录服务不可用（HTTP ${res.status}），请稍后重试。`);
  const data = await res.json() as Record<string, unknown>;
  if (data.ok !== true || typeof data.auth_required !== "boolean"
      || typeof data.service_id !== "string" || !data.service_id
      || typeof data.identity_id !== "string" || !data.identity_id
      || (data.auth_required && (typeof data.token !== "string" || !data.token.trim()))) {
    throw new Error("登录服务返回了无效响应，请重试。");
  }
  if (generation !== authGeneration) throw new Error("登录状态已改变，请重新确认当前连接。");
  tokenScope = `${data.service_id}:${data.identity_id}`;
  updateAuthScope("");
  if (typeof window !== "undefined") {
    try { window.localStorage.setItem(scopeStorageKey(), tokenScope); } catch { /* session metadata remains in memory */ }
  }
  setToken(data.auth_required ? String(data.token) : "");
  return { ok: true, authRequired: data.auth_required };
}

export async function checkAuth(): Promise<{ authenticated: boolean; authRequired: boolean; inContainer: boolean }> {
  const generation = authGeneration;
  try {
    const res = await apiFetch("/api/auth/me", { signal: AbortSignal.timeout(15_000) });
    if (res.status === 401) return { authenticated: false, authRequired: true, inContainer: false };
    if (!res.ok) throw new Error(`认证校验失败（HTTP ${res.status}）`);
    const data = await res.json() as Record<string, unknown>;
    if (data.authenticated !== true || typeof data.auth_required !== "boolean"
        || typeof data.service_id !== "string" || !data.service_id
        || typeof data.identity_id !== "string" || !data.identity_id) {
      throw new Error("认证服务返回了无效响应，请重试。");
    }
    if (generation !== authGeneration) throw new Error("认证状态已改变，请重新校验。");
    if (typeof data.service_id === "string" && typeof data.identity_id === "string") {
      const nextScope = `${data.service_id}:${data.identity_id}`;
      if (tokenScope && tokenScope !== nextScope && data.auth_required) {
        tokenScope = nextScope;
        fireAuthRequired();
        throw new Error("服务身份已改变，请登录当前服务；原服务草稿已隔离保留。");
      }
      const token = getToken();
      tokenScope = nextScope;
      updateAuthScope(nextScope);
      if (typeof window !== "undefined") {
        try { window.localStorage.setItem(scopeStorageKey(), tokenScope); } catch { /* session scope remains authoritative */ }
        if (token) setToken(token);
        window.dispatchEvent(new CustomEvent("muteki:auth-scope", { detail: authScope }));
      }
    }
    return { authenticated: true, authRequired: data.auth_required, inContainer: data.in_container === true };
  } catch (error) {
    if (generation === authGeneration) updateAuthScope("");
    throw error;
  }
}

/**
 * Mint a one-time ticket for opening an SSE/WS connection (no header possible).
 * Returns "" when auth is disabled or the mint fails — callers append it as a
 * query param only when non-empty.
 */
export async function authTicket(): Promise<string> {
  try {
    const res = await apiFetch("/api/auth/ticket", { method: "POST" });
    if (!res.ok) return "";
    const data = await res.json().catch(() => ({} as any));
    return data?.ticket ? String(data.ticket) : "";
  } catch {
    return "";
  }
}

export type RunStatus = "draft" | "running" | "paused" | "solved" | "finished" | "failed";

const isDraftRunId = (id: string) => id.startsWith("draft-");

/** One run as the thread rail lists it (matches RunManager.Run.summary()). */
export interface RunSummary {
  run_id: string;
  mode?: "ctf" | "pentest";
  name: string;
  category: string;
  started: boolean;
  finished: boolean;
  solved: boolean;
  paused: boolean;
  status: RunStatus;
  flag?: string | null;
  pinned: boolean;
  pinned_at?: number | null;
  archived: boolean;
  folder_id?: string | null;
  order: number;
  updated: number;
  updated_at?: number;
}

/** An operator-created rail folder (state/_folders.json). */
export interface Folder {
  id: string;
  name: string;
  order: number;
}

/**
 * Subscribe to a run's SSE event stream and fold it into DeckState. Reconnects
 * with a persisted projection cursor so each new one-time auth ticket resumes
 * after the latest committed event. The conversation-first deck swaps
 * `runId` when the operator opens a new solve — the stream re-subscribes and the
 * deck resets. Returns the live deck + controls.
 */
export function useRun(runId: string) {
  const [deck, setDeck] = useState<DeckState>(() => emptyDeck(runId));
  const [connected, setConnected] = useState(false);
  const esRef = useRef<EventSource | null>(null);
  const deckRef = useRef(deck);
  const lastSeqRef = useRef(0);

  useEffect(() => {
    esRef.current?.close();
    esRef.current = null;
    const initialDeck = emptyDeck(runId);
    deckRef.current = initialDeck;
    lastSeqRef.current = 0;
    setDeck(initialDeck);
    setConnected(false);
    // runId is briefly "" on first mount (the page mints the real draft id in a
    // post-hydration effect to avoid an SSR/client random-id mismatch). No id →
    // no stream to open; the next runId change re-runs this.
    //
    // Draft ids are local UI placeholders. Opening an EventSource for them creates
    // empty backend runs and long-lived idle SSE sockets; enough refreshes/tabs can
    // exhaust the browser's per-origin connection pool and starve real run streams.
    if (!runId || isDraftRunId(runId)) return;

    let cancelled = false;
    let replayDeck: DeckState | null = null;
    let replaySeq = 0;
    let replayProtocolReady = false;
    let persistedSeq = 0;
    let reconnectDelay = 500;
    let reconnectTimer: number | null = null;
    let cacheTimer: number | null = null;
    let renderFrame: number | null = null;
    let connectionVersion = 0;

    const publishDeck = (next: DeckState, immediate = false) => {
      deckRef.current = next;
      if (immediate) {
        if (renderFrame !== null) window.cancelAnimationFrame(renderFrame);
        renderFrame = null;
        setDeck(next);
        return;
      }
      if (renderFrame !== null) return;
      renderFrame = window.requestAnimationFrame(() => {
        renderFrame = null;
        setDeck(deckRef.current);
      });
    };
    const persist = () => {
      const seq = lastSeqRef.current;
      if (seq <= 0 || seq === persistedSeq) return;
      persistedSeq = seq;
      void saveRunProjection(runId, seq, deckRef.current);
    };
    const schedulePersist = (delay = 5000) => {
      if (cacheTimer !== null) return;
      cacheTimer = window.setTimeout(() => {
        cacheTimer = null;
        persist();
      }, delay);
    };
    // every EventType is a named SSE event; one generic handler folds them all
    const handler = (e: MessageEvent) => {
      try {
        const ev = JSON.parse(e.data) as MutekiEvent;
        if (!replayProtocolReady) {
          replayProtocolReady = true;
          if (lastSeqRef.current > 0) {
            lastSeqRef.current = 0;
            replaySeq = 0;
            persistedSeq = 0;
            void deleteRunProjection(runId);
            publishDeck(emptyDeck(runId), true);
          }
        }
        lastSeqRef.current = Math.max(lastSeqRef.current, Number(ev.seq) || 0);
        publishDeck(reduce(deckRef.current, ev));
        schedulePersist();
      } catch {
        /* ignore malformed frame */
      }
    };
    const replayResetHandler = () => {
      replayDeck = emptyDeck(runId);
      replaySeq = 0;
      replayProtocolReady = false;
      lastSeqRef.current = 0;
      persistedSeq = 0;
      void deleteRunProjection(runId);
      publishDeck(replayDeck, true);
    };
    const replayBatchHandler = (e: MessageEvent) => {
      try {
        const frame = JSON.parse(e.data) as { events?: MutekiEvent[]; last_seq?: number };
        if (!Array.isArray(frame.events)) return;
        let next = replayDeck ?? deckRef.current;
        for (const ev of frame.events) next = reduce(next, ev);
        replayDeck = next;
        replaySeq = Math.max(replaySeq, Number(frame.last_seq) || 0);
      } catch {}
    };
    const replayCompleteHandler = (e: MessageEvent) => {
      try {
        const frame = JSON.parse(e.data) as { last_seq?: number };
        replaySeq = Math.max(replaySeq, Number(frame.last_seq) || 0);
      } catch {}
      if (replayDeck) publishDeck(replayDeck, true);
      lastSeqRef.current = Math.max(lastSeqRef.current, replaySeq);
      replayProtocolReady = true;
      replayDeck = null;
      replaySeq = lastSeqRef.current;
      schedulePersist(0);
    };

    // EventSource can't send an Authorization header, so when auth is on we mint
    // a one-time ticket first and pass it as ?ticket=. authTicket() returns ""
    // when auth is disabled (or on failure) — then we open the stream plainly,
    // exactly as before. `cancelled` guards the await: if runId changes (or the
    // component unmounts) before the ticket resolves, we must not open a now-
    // orphaned EventSource.
    const connect = async () => {
      const version = ++connectionVersion;
      const ticket = await authTicket();
      if (cancelled || version !== connectionVersion) return;
      const params = new URLSearchParams({ replay: "batch" });
      if (lastSeqRef.current > 0) params.set("after", String(lastSeqRef.current));
      if (ticket) params.set("ticket", ticket);
      const es = new EventSource(`${API}/api/runs/${runId}/events?${params}`);
      esRef.current = es;
      replayDeck = null;
      replaySeq = lastSeqRef.current;
      replayProtocolReady = false;
      es.onopen = () => {
        reconnectDelay = 500;
        setConnected(true);
      };
      es.onerror = () => {
        if (cancelled || esRef.current !== es) return;
        es.close();
        esRef.current = null;
        replayDeck = null;
        replaySeq = lastSeqRef.current;
        setConnected(false);
        const delay = reconnectDelay;
        reconnectDelay = Math.min(reconnectDelay * 2, 10_000);
        reconnectTimer = window.setTimeout(() => {
          reconnectTimer = null;
          void connect();
        }, delay);
      };
      es.addEventListener("replay.reset", replayResetHandler as EventListener);
      es.addEventListener("replay.batch", replayBatchHandler as EventListener);
      es.addEventListener("replay.complete", replayCompleteHandler as EventListener);
      // listen to all known event names plus the default. Derived directly from
      // the EventType enum (single source of truth) — a hand-copied list silently
      // dropped any newly-added SSE event whose name was forgotten.
      Object.values(EventType).forEach((name) => es.addEventListener(name, handler as EventListener));
      es.onmessage = handler;
    };

    (async () => {
      const cached = await loadRunProjection(runId);
      if (cancelled) return;
      if (cached) {
        lastSeqRef.current = cached.seq;
        persistedSeq = cached.seq;
        publishDeck(cached.deck, true);
      }
      await connect();
    })();

    return () => {
      cancelled = true;
      connectionVersion += 1;
      if (reconnectTimer !== null) window.clearTimeout(reconnectTimer);
      if (cacheTimer !== null) window.clearTimeout(cacheTimer);
      if (renderFrame !== null) window.cancelAnimationFrame(renderFrame);
      persist();
      esRef.current?.close();
      esRef.current = null;
    };
  }, [runId]);

  const start = useCallback(
    async (body: Record<string, any>, overrideRunId?: string) => {
      // overrideRunId lets the caller dispatch to a freshly-minted id without
      // waiting for the runId state update to flush (avoids a one-render race
      // where a draft is promoted to a real run id at send time).
      const target = overrideRunId || runId;
      const res = await apiFetch(`/api/runs/${target}/start`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        let detail = "";
        let payload: Record<string, unknown> = {};
        try {
          const body = await res.json();
          payload = body && typeof body === "object" ? body as Record<string, unknown> : {};
          detail = payload.detail ? String(payload.detail) : "";
        } catch {
          try {
            detail = await res.text();
          } catch {
            detail = "";
          }
        }
        throw new RunStartError(detail || `start failed (${res.status})`, res.status, payload);
      }
      return res.json().catch(() => ({}));
    },
    [runId]
  );

  const sendHitl = useCallback(
    async (target: string, action: string, text: string, opts?: {
      preemption?: string;
      requestId?: string;
      commandId?: string;
      expectedGeneration?: number;
    }) => {
      // A redirect can carry a NEW target URL ("the challenge moved here") — pull
      // the first URL out of the text and send it as `url` so the worker retargets
      // its next turn. A message prefixed with "standing:" (or 常驻:) is persistent
      // background guidance (VPS/SSH creds) injected into every future worker.
      const body: Record<string, unknown> = { target, action, text };
      // B: an explicit directive carries a preemption policy (how aggressively it
      // overrides in-flight work). Default soft_rebind (rebind next batch, no kill).
      if (opts?.preemption) body.preempt_policy = opts.preemption;
      if (opts?.requestId) body.request_id = opts.requestId;
      if (opts?.commandId) body.command_id = opts.commandId;
      if (opts?.expectedGeneration != null) {
        body.expected_generation = opts.expectedGeneration;
      } else if (CONTROL_CAS_ACTIONS.has(action)) {
        // Stable run-state mutations use the latest generation observed over SSE.
        // A stale tab gets a clean 409 instead of silently overwriting newer intent.
        body.expected_generation = deck.controlGeneration;
      }
      if (action === "directive" && !opts?.preemption) body.preempt_policy = "soft_rebind";
      const m = text.match(/https?:\/\/[^\s"'<>]+/);
      if ((action === "redirect" || action === "directive" || action === "focus") && m) {
        body.url = m[0].replace(/[.,;)]+$/, "");
      }
      // Explicit "standing:" / "常驻:" prefix → persistent guidance.
      const sm = text.match(/^\s*(standing|常驻|standing guidance)\s*[:：]\s*(.*)$/i);
      if (sm) { body.standing = true; body.text = sm[2]; }
      if (action === "mark_false" && text.trim()) {
        body.flag = text.trim();
      }
      if (action === "hint") {
        body.standing = true;
      }
      const res = await apiFetch(`/api/runs/${runId}/control`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const data: any = await res.json().catch(() => ({}));
      // The control endpoint can return a structured admission failure with an
      // HTTP 2xx status. Do not let callers interpret `{ok:false}` as a recorded
      // answer and lock the decision card.
      if (!res.ok || data?.ok === false) {
        throw new Error(data?.detail || `control failed (${res.status})`);
      }
      return data;
    },
    [runId, deck.controlGeneration]
  );

  const requestProgress = useCallback(async () => {
    const res = await apiFetch(`/api/runs/${runId}/progress`, {
      method: "POST",
    });
    const data: any = await res.json().catch(() => ({}));
    if (!res.ok || data?.ok === false) {
      throw new Error(data?.detail || `progress request failed (${res.status})`);
    }
    return data as { ok: true; status: "published" | "unchanged"; brief_id?: string | null };
  }, [runId]);

  // "继续做题": relaunch the FULL swarm on a finished run (reuses its workspace so
  // verified facts carry over). Optional `text` folds an operator hint into the
  // re-solve's challenge description.
  const resolve = useCallback(
    async (text?: string) => {
      const body: Record<string, unknown> = {};
      if (text && text.trim()) body.challenge = { description: text.trim() };
      await apiFetch(`/api/runs/${runId}/resolve`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
    },
    [runId]
  );

  return { deck, connected, start, sendHitl, requestProgress, resolve };
}

/**
 * Poll the run list for the thread rail. Runs are cheap summaries (no event
 * replay). `bump` forces an immediate refetch (e.g. right after a dispatch).
 */
export function useRunList(pollMs = 4000, bump = 0) {
  const [runs, setRuns] = useState<RunSummary[]>([]);
  useEffect(() => {
    let alive = true;
    let inFlight: AbortController | null = null;
    const load = async () => {
      if (inFlight) return;
      const ctrl = new AbortController();
      inFlight = ctrl;
      const timeout = window.setTimeout(() => ctrl.abort(), Math.max(3000, Math.min(10000, pollMs * 2)));
      try {
        // ?archived=1 returns ALL runs (archived + active) so the rail can render
        // its Archived section — without it the backend hides archived rows and
        // the section is always empty (the archive-view bug).
        const r = await apiFetch(`/api/runs?archived=1`, { signal: ctrl.signal });
        const j = await r.json();
        if (alive) setRuns(j.runs ?? []);
      } catch {
        /* offline — keep last list */
      } finally {
        window.clearTimeout(timeout);
        if (inFlight === ctrl) inFlight = null;
      }
    };
    load();
    const id = setInterval(load, pollMs);
    return () => {
      alive = false;
      inFlight?.abort();
      inFlight = null;
      clearInterval(id);
    };
  }, [pollMs, bump]);
  return runs;
}

/** One file the backend saved into the run's uploads folder (server.py upload
 *  endpoint). `path` is the ABSOLUTE on-disk path the worker will stage. */
export interface SavedFile {
  name: string;
  path: string;
  size: number;
}

/**
 * Upload challenge files into a run's folder (sessions/{runId}/uploads/). Posts
 * multipart/form-data — do NOT set Content-Type, the browser adds the boundary.
 * The form field name ("files") MUST match the endpoint's `files` param. Returns
 * the saved files (with absolute paths) to thread into challenge.attachments at
 * dispatch. Returns [] on any failure (the deck just shows no chips).
 */
export async function uploadFiles(
  runId: string,
  files: FileList | File[]
): Promise<SavedFile[]> {
  const fd = new FormData();
  Array.from(files).forEach((f) => fd.append("files", f));
  try {
    const r = await apiFetch(`/api/runs/${runId}/uploads`, {
      method: "POST",
      body: fd,
    });
    if (!r.ok) return [];
    const j = await r.json();
    return (j.files ?? []) as SavedFile[];
  } catch {
    return [];
  }
}

/** Mint a fresh run id for "+ New solve". Falls back to a local id if offline. */
export async function newRun(): Promise<string> {
  try {
    const r = await apiFetch(`/api/runs`, { method: "POST" });
    const j = await r.json();
    if (j.run_id) return j.run_id as string;
  } catch {
    /* offline */
  }
  return `run-${Date.now().toString(36)}`;
}

/** Operator rail mutations — pin/unpin, archive/unarchive, rename, move to a
 *  folder (folder_id=null → top-level), drag-order. */
export async function patchRun(
  runId: string,
  patch: { pinned?: boolean; archived?: boolean; name?: string; folder_id?: string | null; order?: number; now?: number }
): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    });
    return r.ok;
  } catch {
    return false;
  }
}

/** Hard-delete a run (irreversible — the caller confirms first). */
export async function deleteRun(runId: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}`, { method: "DELETE" });
    if (r.ok) await deleteRunProjection(runId);
    return r.ok;
  } catch {
    return false;
  }
}

/** Open the run's workspace dir in the host file manager (operator-local). */
export async function openWorkspace(runId: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}/open`, { method: "POST" });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}

// ── engine status ────────────────────────────────────────────────────────────

/** Per-dispatched-worker availability + health. */
export interface EngineStatus {
  engine: string;
  bin: string;
  available: boolean;
  healthy?: boolean;
  health_detail?: string;
  profile_id?: string;
  profile_name?: string;
}

/** Deep per-engine self-check result (FE-healthcheck-page). */
export interface EngineHealth {
  engine: string;
  bin: string;
  version: string;
  healthy: boolean;
  detail: string;
  backend?: string;
  /** Where the bin path came from: "env" (pinned via MUTEKI_<E>_BIN), "known-good",
   *  "path" (auto-discovered on PATH — may be the wrong version), or "fallback". */
  bin_source?: "env" | "known-good" | "path" | "fallback";
  /** The env var that pins this engine's bin (e.g. MUTEKI_CLAUDE_BIN). */
  bin_env?: string;
}

export function useEngines(pollMs = 300000): EngineStatus[] {
  const [engines, setEngines] = useState<EngineStatus[]>([]);
  const inFlight = useRef(false);
  useEffect(() => {
    let alive = true;
    const load = async () => {
      if (inFlight.current) return;
      inFlight.current = true;
      try {
        const r = await apiFetch(`/api/engines`);
        const j = await r.json();
        if (alive) setEngines(j.engines ?? []);
      } catch { /* offline — keep last */ }
      finally { inFlight.current = false; }
    };
    load();
    const id = setInterval(load, pollMs);
    return () => { alive = false; clearInterval(id); };
  }, [pollMs]);
  return engines;
}

// ── rail folders (FE-session-folder) ────────────────────────────────────────

async function listFolders(): Promise<Folder[]> {
  try {
    const r = await apiFetch(`/api/folders`);
    const j = await r.json();
    return (j.folders ?? []) as Folder[];
  } catch {
    return [];
  }
}

export async function createFolder(name: string): Promise<Folder | null> {
  try {
    const r = await apiFetch(`/api/folders`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    const j = await r.json();
    return (j.folder ?? null) as Folder | null;
  } catch {
    return null;
  }
}

export async function renameFolder(id: string, name: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/folders/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name }),
    });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}

export async function deleteFolder(id: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/folders/${id}`, { method: "DELETE" });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}

/** Poll the folder list for the rail (cheap; bump forces an immediate refetch). */
export function useFolders(pollMs = 8000, bump = 0): Folder[] {
  const [folders, setFolders] = useState<Folder[]>([]);
  useEffect(() => {
    let alive = true;
    const load = async () => {
      const f = await listFolders();
      if (alive) setFolders(f);
    };
    load();
    const id = setInterval(load, pollMs);
    return () => { alive = false; clearInterval(id); };
  }, [pollMs, bump]);
  return folders;
}

// ── worker-roster management (BE-worker-management) ─────────────────────────

export type LlmTemperatureMode = "default" | "custom" | "omit";

export type LlmProfile = {
  endpoint_id?: string;
  /** Read-only migration input from releases before model endpoints were split. */
  credential_id?: string;
  provider: string;
  model: string;
  base_url?: string;
  connection?: "default" | "custom_endpoint" | "endpoint";
  temperature_mode?: LlmTemperatureMode;
  temperature?: number;
  api_key?: string;
  clear_api_key?: boolean;
  credential_source?: "saved" | "environment" | "missing";
};

/** The default worker roster the dispatch path falls back to. Mirrors the
 *  backend WorkerConfigStore (apps/web/worker_config.py). */
export interface WorkerSettings {
  engines: string[];
  start_workers: number;
  max_workers: number;
  worker_backend: "local" | "container";
  worker_network: "bridge" | "host" | "none";
  /** Docker network actually applied (#171); may differ when MUTEKI_WORKER_NETWORK remaps bridge. */
  effective_network?: string;
  network_requested?: string;
  network_reason?: string;
  network_error?: string;
  worker_container_scope: "run" | "shared";
  worker_privilege?: "default" | "elevated";
  worker_vpn_enabled: boolean;
  race_scout: boolean;
  race_timeout: number;
  wall_clock_budget: number;
  race_engines: string[];
  max_total_workers: number;
  cost_budget_usd: number;
  stage_policy: {
    prepare: Record<string, unknown>;
    race: { enabled: boolean; timeout: number; engines: string[] };
    coordinator: {
      dispatch_mode?: "fixed" | "auto";
      wall_clock_budget: number;
      review?: {
        enabled?: boolean;
        engine?: string;
        timeout?: number;
        after_race?: boolean;
        after_fruitless_workers?: number;
        after_duplicate_intents?: number;
        on_course_correct?: boolean;
        on_candidate_spike?: boolean;
        on_operator_hint?: boolean;
        allow_review_fallback?: boolean;
        every_completed_workers?: number;
        candidate_spike_threshold?: number;
        max_concurrent?: number;
        cooldown_events?: number;
        max_review_workers?: number;
        reasoning_effort?: string;
      };
      verifier?: {
        enabled?: boolean;
        engine?: string;
        timeout?: number;
        max_concurrent?: number;
        max_verifier_workers?: number;
        allow_verifier_fallback?: boolean;
        reasoning_effort?: string;
      };
    };
    budgets: { max_total_workers: number; cost_budget_usd: number };
  };
  llm_profiles: {
    planner: LlmProfile;
    titler: LlmProfile;
  };
  worker_profiles: {
    id: string;
    name?: string;
    engine: string;
    transport: string;
    auth: string;
    credential_mode?: string;
    credential_account: string;
    credential_id?: string;
    api_key_ref?: string;
    base_url?: string;
    wire_api?: string;
    roles: string[];
    race: boolean;
    max_running: number;
    max_review_running?: number;
    priority: number;
    model?: string;
    reasoning_effort?: string;
    enabled: boolean;
  }[];
  /** Canonical settings identity model. The legacy worker_profiles projection is
   * still returned for the scheduler and health routes, but new settings UI must
   * edit these objects so disabled seats and explicit credential bindings survive. */
  seats?: {
    id: string;
    label: string;
    engine: string;
    credential_id: string;
    model?: string;
    reasoning_effort?: string;
    roles: string[];
    race: boolean;
    capacity: { max_running: number; max_review_running: number };
    priority: number;
    enabled: boolean;
  }[];
  credentials?: {
    id: string;
    label: string;
    engine: string;
    kind: "system_inherit" | "engine_key" | "custom_endpoint";
    secret_ref: string;
    target_engine?: string;
    endpoint?: { base_url?: string; wire_api?: string };
    updated_at?: number | null;
  }[];
  seat_alias?: Record<string, string>;
  credential_alias?: Record<string, string>;
  overrides: Record<string, { engines: string[]; start_workers: number }>;
}

export interface CredentialAccount {
  account_id: string;
  revision?: string;
  engine: string;
  worker_engine?: string;
  connection?: "official" | "custom_endpoint";
  base_url?: string;
  provider?: string;
  suggested_model?: string;
  credential_format?: "oauth_token" | "auth_json" | "auth_home" | "api_key" | "unknown";
  mode: string;
  present: boolean;
  writable_state: boolean;
  updated_at?: number | null;
  details: Record<string, unknown>;
}

export interface GlobalCredentialUsage {
  kind?: "worker" | "review" | "verifier" | "conversation" | "llm_profile" | string;
  id?: string;
  profile_id?: string;
  label?: string;
  engine?: string;
  model?: string;
  role?: string;
  enabled?: boolean;
}

export interface GlobalCredentialLastTest {
  ok?: boolean;
  status?: string;
  detail?: string;
  tested_at?: number | string;
  backend?: string;
  model?: string;
}

export interface GlobalCredentialModelCatalog {
  credential_id?: string;
  engine?: string;
  environment?: "local" | "container" | string;
  runtime_instance?: string;
  source?: string;
  refresh_status?: "missing" | "fresh" | "stale" | "failed" | string;
  refreshed_at?: number | null;
  expires_at?: number | null;
  error_code?: string;
  last_error?: string;
  discovered_models?: Array<{ id?: string; label?: string; provider?: string }>;
  configured_models?: string[];
  verified_models?: string[];
  default_model?: string;
}

/** Unified, read-only credential inventory consumed by every settings surface.
 * Mutations continue to use the credential-account endpoints in the dedicated
 * credential center; Worker/Profile editors only select one of these rows. */
export interface GlobalCredential {
  status_detail?: string;
  discovery_code?: string;
  id: string;
  revision?: string;
  label: string;
  engine: string;
  source: "stored" | "system";
  account_id?: string;
  connection: "official" | "custom_endpoint" | "system" | string;
  credential_format?: "oauth_token" | "auth_json" | "auth_home" | "api_key" | "unknown";
  provider?: string;
  base_url?: string;
  present: boolean;
  status: string;
  models: string[];
  candidate_models?: string[];
  default_model?: string;
  model_catalog?: GlobalCredentialModelCatalog;
  last_test?: GlobalCredentialLastTest | null;
  usage: GlobalCredentialUsage[];
}

/** HTTP model connection used directly by Reason/Titler. It deliberately has
 * no Agent engine field because this path never starts a CLI Agent. */
export interface ModelEndpoint {
  id: string;
  label: string;
  provider?: string;
  base_url: string;
  present: boolean;
  status: string;
  models: string[];
  default_model?: string;
  catalog?: GlobalCredentialModelCatalog;
  usage: GlobalCredentialUsage[];
}

export async function getGlobalCredentials(environment?: "local" | "container"): Promise<GlobalCredential[]> {
  const query = environment ? `?environment=${environment}` : "";
  const response = await apiFetch(`/api/settings/credentials${query}`);
  const payload = await response.json();
  if (!response.ok) throw new Error(payload.error?.message || payload.detail?.message || payload.detail || `凭据目录读取失败（HTTP ${response.status}）`);
  if (!Array.isArray(payload.credentials)) throw new Error("凭据目录响应无效，请重试。");
  return payload.credentials as GlobalCredential[];
}

export async function getModelEndpoints(): Promise<ModelEndpoint[]> {
  try {
    const response = await apiFetch(`/api/settings/model-endpoints`);
    if (!response.ok) return [];
    const payload = await response.json();
    return Array.isArray(payload.endpoints)
      ? payload.endpoints as ModelEndpoint[]
      : [];
  } catch {
    return [];
  }
}

export async function createModelEndpoint(body: {
  id: string;
  provider?: string;
  base_url: string;
  /** Required for create; omit or leave blank when editing to keep the stored key. */
  api_key?: string;
  model: string;
}): Promise<{ ok: boolean; detail: string; endpoint?: ModelEndpoint; updated?: boolean }> {
  try {
    const response = await apiFetch(`/api/settings/model-endpoints`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const payload = await response.json().catch(() => ({}));
    return {
      ok: response.ok && Boolean(payload.ok),
      detail: String(payload.detail ?? (response.ok ? "" : `请求失败（HTTP ${response.status}）`)),
      endpoint: payload.endpoint as ModelEndpoint | undefined,
      updated: Boolean(payload.updated),
    };
  } catch (error) {
    return { ok: false, detail: String(error) };
  }
}

export async function refreshCredentialModels(
  credentialId: string,
  body: {
    engine?: string;
    connection?: "official" | "custom_endpoint";
    base_url?: string;
    secret?: string;
    backend?: "local" | "container";
    runtime_instance?: string;
  } = {},
): Promise<{ ok: boolean; detail: string; models: string[]; source?: string }> {
  try {
    const response = await apiFetch(`/api/settings/credentials/${encodeURIComponent(credentialId)}/models/refresh`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(60_000),
    });
    const payload = await response.json().catch(() => ({}));
    const models = Array.isArray(payload.models)
      ? payload.models.map((item: unknown) => String(item || "").trim()).filter(Boolean)
      : [];
    return {
      ok: response.ok && Boolean(payload.ok) && models.length > 0,
      detail: String(payload.detail ?? (response.ok ? "" : `HTTP ${response.status}`)),
      models,
      source: typeof payload.source === "string" ? payload.source : undefined,
    };
  } catch (error) {
    const detail = (error as Error)?.name === "TimeoutError"
      ? "读取模型列表超时（>60s）"
      : String(error);
    return { ok: false, detail, models: [] };
  }
}

export async function testGlobalCredential(
  credentialId: string,
  engine: string,
  backend: "local" | "container",
  model = "",
  runtimeInstance = "default",
): Promise<WorkerModelTestResult> {
  try {
    const response = await apiFetch(`/api/settings/credentials/${encodeURIComponent(credentialId)}/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ engine, backend, model, runtime_instance: runtimeInstance }),
      signal: AbortSignal.timeout(180_000),
    });
    const payload = await response.json().catch(() => ({}));
    return parseWorkerModelTestResult(payload, {
      ok: response.ok && Boolean(payload.ok),
      detail: String(payload.detail ?? (response.ok ? "" : `HTTP ${response.status}`)),
      engine,
      backend,
      model,
    });
  } catch (error) {
    const detail = (error as Error)?.name === "TimeoutError"
      ? "测试超时（>180s），请检查引擎状态后重试"
      : String(error);
    return parseWorkerModelTestResult(null, { ok: false, detail, engine, backend, model });
  }
}

export type WorkerModelOption = {
  id: string;
  label: string;
  reasoning?: {
    supported: boolean;
    levels: string[];
    default?: string;
  };
};

export interface WorkerModelOptions {
  allow_custom: boolean;
  manual_updated_at?: string | null;
  manual_models: Record<string, WorkerModelOption[]>;
  discovered_models: Record<string, WorkerModelOption[]>;
  models_by_profile: Record<string, WorkerModelOption[]>;
  discovery: Record<string, { engine: string; source: string; updated_at?: number | null; count: number }>;
  discovery_results?: { profile_id: string; engine: string; ok: boolean; detail: string; source: string; models: WorkerModelOption[] }[];
  discovery_ok?: boolean;
  models: Record<string, WorkerModelOption[]>;
}

const emptyWorkerModelOptions = (): WorkerModelOptions => ({
  allow_custom: false,
  manual_models: {},
  discovered_models: {},
  models_by_profile: {},
  discovery: {},
  models: {},
});

export async function getWorkerSettings(): Promise<WorkerSettings | null> {
  try {
    const r = await apiFetch(`/api/settings/workers`);
    if (!r.ok) return null;
    const j = await r.json();
    return (j.config ?? null) as WorkerSettings | null;
  } catch {
    return null;
  }
}

export interface PlatformUpdateStatus {
  status: "idle" | "checking" | "available" | "current" | "downloading" | "preparing" | "switching" | "installed" | "rolled_back" | "error" | string;
  current_version: string;
  active_version?: string | null;
  latest_version?: string | null;
  previous_version?: string | null;
  available: boolean;
  progress?: number | null;
  message?: string | null;
  error?: string | null;
  checked_at?: string | null;
  updated_at?: string | null;
  restart_required: boolean;
  install_kind: "source" | "managed" | "compose";
  install_root: string;
  channel: string;
  deployment: string;
  running?: boolean;
}

async function platformUpdateRequest(path: string, init?: RequestInit): Promise<PlatformUpdateStatus | null> {
  try {
    const headers = new Headers(init?.headers || {});
    const method = (init?.method || "GET").toUpperCase();
    if (method !== "GET" && method !== "HEAD" && !headers.has("Content-Type")) {
      headers.set("Content-Type", "application/json");
    }
    const response = await apiFetch(path, {
      ...init,
      headers,
      body: init?.body ?? (method === "GET" || method === "HEAD" ? undefined : "{}"),
    });
    if (!response.ok) return null;
    const payload = await response.json();
    return (payload.update ?? null) as PlatformUpdateStatus | null;
  } catch {
    return null;
  }
}

export async function getPlatformUpdateStatus(): Promise<PlatformUpdateStatus | null> {
  return platformUpdateRequest("/api/settings/system-update");
}

export async function checkPlatformUpdate(): Promise<PlatformUpdateStatus | null> {
  return platformUpdateRequest("/api/settings/system-update/check", { method: "POST" });
}

export async function installPlatformUpdate(): Promise<PlatformUpdateStatus | null> {
  return platformUpdateRequest("/api/settings/system-update/install", { method: "POST" });
}

export async function rollbackPlatformUpdate(): Promise<PlatformUpdateStatus | null> {
  return platformUpdateRequest("/api/settings/system-update/rollback", { method: "POST" });
}

export async function getWorkerModelOptions(): Promise<WorkerModelOptions> {
  try {
    const r = await apiFetch(`/api/settings/worker-models`);
    if (!r.ok) return emptyWorkerModelOptions();
    const j = await r.json();
    return {
      allow_custom: Boolean(j.allow_custom ?? false),
      manual_updated_at: j.manual_updated_at ?? null,
      manual_models: (j.manual_models ?? j.models ?? {}) as WorkerModelOptions["manual_models"],
      discovered_models: (j.discovered_models ?? {}) as WorkerModelOptions["discovered_models"],
      models_by_profile: (j.models_by_profile ?? {}) as WorkerModelOptions["models_by_profile"],
      discovery: (j.discovery ?? {}) as WorkerModelOptions["discovery"],
      models: (j.models ?? {}) as WorkerModelOptions["models"],
    };
  } catch {
    return emptyWorkerModelOptions();
  }
}

// ── P2-v3: worker image health (daemon / pulled / version) ──────────────────
export type WorkerImageStatus = {
  image: string;
  daemon: { ok: boolean; detail: string };
  pulled: { ok: boolean; detail: string };
  version: { status: "match" | "mismatch" | "unknown"; expected: string | null; actual: string | null; detail: string };
  overall: "green" | "yellow" | "red";
};

export async function getWorkerImageStatus(): Promise<WorkerImageStatus | null> {
  try {
    const r = await apiFetch(`/api/settings/worker-image`);
    if (!r.ok) return null;
    return (await r.json()) as WorkerImageStatus;
  } catch {
    return null;
  }
}

export async function pullWorkerImage(): Promise<{ ok: boolean; detail: string; version?: string | null }> {
  try {
    const r = await apiFetch(`/api/settings/worker-image/pull`, { method: "POST" });
    const j = await r.json().catch(() => ({}));
    return { ok: Boolean(j?.ok), detail: String(j?.detail ?? (r.ok ? "" : "pull failed")), version: j?.version ?? null };
  } catch (e) {
    return { ok: false, detail: String(e) };
  }
}

export type WorkerModelTestLog = {
  stream: "system" | "command" | "stdout" | "stderr" | "success" | "error";
  message: string;
  elapsed_ms: number;
};

export type WorkerModelTestResult = {
  profile_id?: string;
  ok: boolean;
  detail: string;
  model: string;
  engine: string;
  backend?: "local" | "container";
  command?: string;
  stdout?: string;
  stderr?: string;
  exit_code?: number | null;
  elapsed_ms?: number;
  layer?: string;
  tested_at?: number;
  logs: WorkerModelTestLog[];
};

function parseWorkerModelTestResult(
  payload: Record<string, unknown> | null | undefined,
  fallback: {
    ok?: boolean;
    detail?: string;
    model?: string;
    engine?: string;
    backend?: "local" | "container";
  } = {},
): WorkerModelTestResult {
  const row = payload && typeof payload === "object" ? payload : {};
  const detail = String(row.detail ?? fallback.detail ?? "");
  const ok = Boolean(row.ok ?? fallback.ok);
  const elapsedMs = typeof row.elapsed_ms === "number" ? row.elapsed_ms : undefined;
  return {
    ok,
    detail,
    model: String(row.model ?? fallback.model ?? ""),
    engine: String(row.engine ?? fallback.engine ?? ""),
    backend: row.backend === "container" || fallback.backend === "container" ? "container" : "local",
    command: typeof row.command === "string" ? row.command : undefined,
    stdout: typeof row.stdout === "string" ? row.stdout : undefined,
    stderr: typeof row.stderr === "string" ? row.stderr : undefined,
    exit_code: typeof row.exit_code === "number" ? row.exit_code : null,
    elapsed_ms: elapsedMs,
    layer: typeof row.layer === "string" ? row.layer : undefined,
    tested_at: typeof row.tested_at === "number" ? row.tested_at : undefined,
    logs: Array.isArray(row.logs)
      ? row.logs.map((item) => {
          const log = item && typeof item === "object" ? item as Record<string, unknown> : {};
          return {
            stream: String(log.stream || "system") as WorkerModelTestLog["stream"],
            message: String(log.message || ""),
            elapsed_ms: Number(log.elapsed_ms) || 0,
          };
        })
      : detail
        ? [{ stream: ok ? "success" as const : "error" as const, message: detail, elapsed_ms: elapsedMs || 0 }]
        : [],
  };
}

export async function getWorkerModelTestResults(): Promise<Record<string, WorkerModelTestResult>> {
  try {
    const response = await apiFetch(`/api/settings/worker-model/test-results`);
    if (!response.ok) return {};
    const payload = await response.json().catch(() => ({}));
    const rawResults = payload?.results && typeof payload.results === "object"
      ? payload.results as Record<string, Record<string, unknown>>
      : {};
    return Object.fromEntries(Object.entries(rawResults).map(([profileId, row]) => {
      const detail = String(row.detail ?? "");
      return [profileId, {
        profile_id: profileId,
        ok: Boolean(row.ok),
        detail,
        model: String(row.model ?? ""),
        engine: String(row.engine ?? ""),
        backend: row.backend === "container" ? "container" as const : "local" as const,
        exit_code: typeof row.exit_code === "number" ? row.exit_code : null,
        elapsed_ms: typeof row.elapsed_ms === "number" ? row.elapsed_ms : undefined,
        layer: typeof row.layer === "string" ? row.layer : undefined,
        tested_at: typeof row.tested_at === "number" ? row.tested_at : undefined,
        logs: Array.isArray(row.logs)
          ? row.logs.map((item: Record<string, unknown>) => ({
              stream: String(item.stream || "system") as WorkerModelTestLog["stream"],
              message: String(item.message || ""),
              elapsed_ms: Number(item.elapsed_ms) || 0,
            }))
          : [{
              stream: row.ok ? "success" as const : "error" as const,
              message: detail,
              elapsed_ms: Number(row.elapsed_ms) || 0,
            }],
      } satisfies WorkerModelTestResult];
    }));
  } catch {
    return {};
  }
}

export async function testWorkerProfileModel(
  profile: WorkerSettings["worker_profiles"][number],
  model: string,
  backend: "local" | "container"
): Promise<WorkerModelTestResult> {
  try {
    const r = await apiFetch(`/api/settings/worker-model/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        profile,
        model,
        reasoning_effort: profile.reasoning_effort ?? "default",
        backend,
      }),
    });
    const j = await r.json().catch(() => ({}));
    return {
      ok: !!j.ok,
      detail: String(j.detail ?? ""),
      model: String(j.model ?? model),
      engine: String(j.engine ?? profile.engine),
      backend: j.backend === "container" ? "container" : "local",
      command: typeof j.command === "string" ? j.command : undefined,
      stdout: typeof j.stdout === "string" ? j.stdout : undefined,
      stderr: typeof j.stderr === "string" ? j.stderr : undefined,
      exit_code: typeof j.exit_code === "number" ? j.exit_code : null,
      elapsed_ms: typeof j.elapsed_ms === "number" ? j.elapsed_ms : undefined,
      layer: typeof j.layer === "string" ? j.layer : undefined,
      tested_at: typeof j.tested_at === "number" ? j.tested_at : undefined,
      logs: Array.isArray(j.logs)
        ? j.logs.map((item: Record<string, unknown>) => ({
            stream: String(item.stream || "system") as WorkerModelTestLog["stream"],
            message: String(item.message || ""),
            elapsed_ms: Number(item.elapsed_ms) || 0,
          }))
        : [],
    };
  } catch (e) {
    const detail = String(e);
    return {
      ok: false,
      detail,
      model,
      engine: profile.engine,
      backend,
      logs: [{ stream: "error", message: detail, elapsed_ms: 0 }],
    };
  }
}

export type WorkerModelBatchTestItem = {
  profile_id: string;
  profile: WorkerSettings["worker_profiles"][number];
  model: string;
  reasoning_effort?: string;
};

export type WorkerModelBatchTestResult = {
  backend: "local" | "container";
  container_count: number;
  results: WorkerModelTestResult[];
};

export async function testWorkerProfileModelsBatch(
  items: WorkerModelBatchTestItem[],
  backend: "local" | "container"
): Promise<WorkerModelBatchTestResult> {
  try {
    const response = await apiFetch(`/api/settings/worker-model/test-batch`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ items, backend }),
    });
    const payload = await response.json().catch(() => ({}));
    const rawResults = Array.isArray(payload?.results) ? payload.results : [];
    const results = items.map((item, index): WorkerModelTestResult => {
      const row = rawResults[index] || {};
      const detail = String(row.detail ?? (response.ok ? "" : `批量模型测试请求失败（HTTP ${response.status}）`));
      return {
        ok: !!row.ok,
        detail,
        model: String(row.model ?? item.model),
        engine: String(row.engine ?? item.profile.engine),
        backend: row.backend === "container" ? "container" : "local",
        command: typeof row.command === "string" ? row.command : undefined,
        stdout: typeof row.stdout === "string" ? row.stdout : undefined,
        stderr: typeof row.stderr === "string" ? row.stderr : undefined,
        exit_code: typeof row.exit_code === "number" ? row.exit_code : null,
        elapsed_ms: typeof row.elapsed_ms === "number" ? row.elapsed_ms : undefined,
        layer: typeof row.layer === "string" ? row.layer : undefined,
        tested_at: typeof row.tested_at === "number" ? row.tested_at : undefined,
        logs: Array.isArray(row.logs)
          ? row.logs.map((log: Record<string, unknown>) => ({
              stream: String(log.stream || "system") as WorkerModelTestLog["stream"],
              message: String(log.message || ""),
              elapsed_ms: Number(log.elapsed_ms) || 0,
            }))
          : [{ stream: "error", message: detail || "批量模型测试没有返回结果", elapsed_ms: 0 }],
      };
    });
    return {
      backend: payload?.backend === "container" ? "container" : "local",
      container_count: Number(payload?.container_count) || 0,
      results,
    };
  } catch (error) {
    const detail = String(error);
    return {
      backend,
      container_count: 0,
      results: items.map((item) => ({
        ok: false,
        detail,
        model: item.model,
        engine: item.profile.engine,
        backend,
        layer: "request",
        logs: [{ stream: "error", message: detail, elapsed_ms: 0 }],
      })),
    };
  }
}

// ── per-profile health (single source of truth shared with the dispatch precheck) ──
export type ProfileHealth = {
  profile_id: string;
  engine: string;
  backend: string;
  status: "ok" | "blocked" | "auth_failed" | "disabled";
  layer: string | null;
  blocker: string | null;
  detail: string;
  model: string;
  account_id: string;
  // SINGLE SOURCE OF TRUTH for "bound?" — read these instead of the literal
  // credential_account field (which caused the "未绑定 vs 已绑定" contradiction).
  // explicit = profile named the account; inherited = empty → fell back to the
  // default/host login (show "自动: <id>", NOT "未绑定"); missing = no credential.
  binding_kind?: "explicit" | "inherited" | "missing";
  effective_credential_id?: string;
};

/** Batch readiness for every profile at the CHEAP binding depth (zero network /
 *  zero docker) — drives the settings badge + account rows. Backend is resolved
 *  server-side (same per-profile runtime→backend mapping dispatch uses). */
export async function fetchProfilesHealth(): Promise<ProfileHealth[]> {
  try {
    const r = await apiFetch(`/api/settings/profiles/health`);
    if (!r.ok) return [];
    const j = await r.json();
    return (j.profiles ?? []) as ProfileHealth[];
  } catch {
    return [];
  }
}

/** Update the default roster. Returns the persisted config, or null on
 *  failure (e.g. 400 for an invalid roster). */
export async function putWorkerSettings(
  patch: Partial<WorkerSettings>
): Promise<WorkerSettings | null> {
  const r = await apiFetch(`/api/settings/workers`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) {
    const raw = j?.detail ?? j?.error?.message ?? `HTTP ${r.status}`;
    throw new Error(String(raw));
  }
  return (j.config ?? null) as WorkerSettings | null;
}

export type OpenVpnStatus = {
  present: boolean;
  filename?: string;
  size?: number;
  sha256?: string;
};

export async function getOpenVpnStatus(): Promise<OpenVpnStatus> {
  const r = await apiFetch(`/api/settings/runtime/openvpn`);
  if (!r.ok) return { present: false };
  return (await r.json()) as OpenVpnStatus;
}

export async function uploadOpenVpnConfig(file: File): Promise<OpenVpnStatus> {
  const r = await apiFetch(`/api/settings/runtime/openvpn`, {
    method: "POST",
    headers: { "Content-Type": "application/x-openvpn-profile" },
    body: file,
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(String(j?.detail || `HTTP ${r.status}`));
  return j as OpenVpnStatus;
}

export async function putCredentialAccount(
  accountId: string,
  body: {
    engine: string;
    create_only?: boolean;
    expected_revision?: string;
    worker_engine?: string;
    connection?: "official" | "custom_endpoint";
    secret?: string;
    codex_auth_json?: string;
    base_url?: string;
    provider?: string;
    target_engine?: string;
    target_model?: string;
    models?: string[];
  }
): Promise<CredentialAccount | null> {
  const r = await apiFetch(`/api/settings/credential-accounts/${encodeURIComponent(accountId)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) {
    throw new Error(String(j?.detail?.message ?? j?.detail ?? j?.error?.message ?? `HTTP ${r.status}`));
  }
  if (j.ok !== true || !j.account) throw new Error("服务未确认凭据保存成功。");
  return j.account as CredentialAccount;
}

export async function deleteCredentialAccount(accountId: string, detachReferences = false): Promise<boolean> {
  const suffix = detachReferences ? "?detach_references=true" : "";
  const r = await apiFetch(`/api/settings/credential-accounts/${encodeURIComponent(accountId)}${suffix}`, {
    method: "DELETE",
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) {
    throw new Error(String(j?.detail ?? j?.error?.message ?? `HTTP ${r.status}`));
  }
  return Boolean(j.ok);
}

/** One-click refresh of a codex account from the HOST's ~/.codex/auth.json (after
 *  `codex login`). Returns {ok, detail} — detail carries the server's error (e.g.
 *  host file missing, or unavailable when web runs in a container). */
export async function importHostCodexAuth(
  accountId: string
): Promise<{ ok: boolean; detail: string; account: CredentialAccount | null }> {
  try {
    const r = await apiFetch(
      `/api/settings/credential-accounts/${encodeURIComponent(accountId)}/import-host-codex`,
      { method: "POST" }
    );
    const j = await r.json().catch(() => ({}));
    return {
      ok: r.ok && Boolean(j.ok),
      detail: String(j.detail ?? (r.ok ? "" : "import failed")),
      account: (j.account ?? null) as CredentialAccount | null,
    };
  } catch (e) {
    return { ok: false, detail: String(e), account: null };
  }
}

/** Import the host's existing Claude gateway, Kimi Code login, or Grok login
 * into a durable Worker account. Only the required credential/config material
 * is copied; caches, sessions and binaries are not included. */
export async function importHostWorkerLogin(
  accountId: string,
  engine: "claude" | "kimi" | "grok",
): Promise<{ ok: boolean; detail: string; account: CredentialAccount | null }> {
  try {
    const r = await apiFetch(
      `/api/settings/credential-accounts/${encodeURIComponent(accountId)}/import-host-login`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ engine }),
      },
    );
    const j = await r.json().catch(() => ({}));
    return {
      ok: r.ok && Boolean(j.ok),
      detail: String(j.detail ?? (r.ok ? "" : "import failed")),
      account: (j.account ?? null) as CredentialAccount | null,
    };
  } catch (e) {
    return { ok: false, detail: String(e), account: null };
  }
}

export type SystemLoginStatus = "present" | "absent" | "unknown";

/** Test the planner/titler endpoint the operator is editing. */
export async function testLlmEndpoint(
  which: "planner" | "titler",
  endpoint_id: string,
  model: string,
  temperature_mode: LlmTemperatureMode = "default",
  temperature?: number,
): Promise<{ ok: boolean; detail: string; model: string }> {
  try {
    const r = await apiFetch(`/api/settings/llm/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ which, endpoint_id, model, temperature_mode, temperature }),
    });
    const j = await r.json().catch(() => ({}));
    return {
      ok: r.ok && !!j.ok,
      detail: String(j.detail ?? (r.ok ? "" : `请求失败（HTTP ${r.status}）`)),
      model: String(j.model ?? model),
    };
  } catch (e) {
    return { ok: false, detail: String(e), model };
  }
}

/** Operator runtime control: add a worker for an engine to a LIVE run
 *  (omit engine → coordinator picks heterogeneity-aware). */
export async function spawnWorker(runId: string, engine?: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}/workers`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(engine ? { engine } : {}),
    });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}

/** Operator runtime control: stop a specific worker by its solver_id. */
export async function killWorker(runId: string, solverId: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}/workers`, {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ solver_id: solverId }),
    });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}
