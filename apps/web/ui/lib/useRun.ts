"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { DeckState, EventType, MutekiEvent, emptyDeck, reduce } from "./events";

/**
 * API base. Empty string = same-origin (prod: the static UI is served by the
 * FastAPI backend, so /api works directly). In dev, set NEXT_PUBLIC_MUTEKI_API
 * to the backend (e.g. http://127.0.0.1:8000) so the browser's EventSource
 * connects STRAIGHT to it — the Next dev rewrite proxy buffers SSE and makes a
 * live run look frozen until it ends. CORS on the backend allows localhost.
 */
export const API = process.env.NEXT_PUBLIC_MUTEKI_API || "";

export type RunStatus = "draft" | "running" | "paused" | "solved" | "finished" | "failed";

export const isDraftRunId = (id: string) => id.startsWith("draft-");

/** One run as the thread rail lists it (matches RunManager.Run.summary()). */
export interface RunSummary {
  run_id: string;
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

/** An operator-created rail folder (sessions/_folders.json). */
export interface Folder {
  id: string;
  name: string;
  order: number;
}

/**
 * Subscribe to a run's SSE event stream and fold it into DeckState. Reconnects
 * with Last-Event-ID (the browser EventSource sets this automatically on
 * reconnect, and our backend honors it). The conversation-first deck swaps
 * `runId` when the operator opens a new solve — the stream re-subscribes and the
 * deck resets. Returns the live deck + controls.
 */
export function useRun(runId: string) {
  const [deck, setDeck] = useState<DeckState>(() => emptyDeck(runId));
  const [connected, setConnected] = useState(false);
  const esRef = useRef<EventSource | null>(null);

  useEffect(() => {
    esRef.current?.close();
    esRef.current = null;
    setDeck(emptyDeck(runId));
    setConnected(false);
    // runId is briefly "" on first mount (the page mints the real draft id in a
    // post-hydration effect to avoid an SSR/client random-id mismatch). No id →
    // no stream to open; the next runId change re-runs this.
    //
    // Draft ids are local UI placeholders. Opening an EventSource for them creates
    // empty backend runs and long-lived idle SSE sockets; enough refreshes/tabs can
    // exhaust the browser's per-origin connection pool and starve real run streams.
    if (!runId || isDraftRunId(runId)) return;
    const es = new EventSource(`${API}/api/runs/${runId}/events`);
    esRef.current = es;
    es.onopen = () => setConnected(true);
    es.onerror = () => setConnected(false);
    // every EventType is a named SSE event; one generic handler folds them all
    const handler = (e: MessageEvent) => {
      try {
        const ev = JSON.parse(e.data) as MutekiEvent;
        setDeck((prev) => reduce(prev, ev));
      } catch {
        /* ignore malformed frame */
      }
    };
    // listen to all known event names plus the default. Derived directly from the
    // EventType enum (single source of truth) — a hand-copied list silently dropped
    // any newly-added SSE event whose name was forgotten (e.g. node.summarized).
    Object.values(EventType).forEach((name) => es.addEventListener(name, handler as EventListener));
    es.onmessage = handler;
    return () => {
      es.close();
      esRef.current = null;
    };
  }, [runId]);

  const start = useCallback(
    async (body: Record<string, any>, overrideRunId?: string) => {
      // overrideRunId lets the caller dispatch to a freshly-minted id without
      // waiting for the runId state update to flush (avoids a one-render race
      // where a draft is promoted to a real run id at send time).
      const target = overrideRunId || runId;
      const res = await fetch(`${API}/api/runs/${target}/start`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) {
        let detail = "";
        try {
          const body = await res.json();
          detail = body?.detail ? String(body.detail) : "";
        } catch {
          try {
            detail = await res.text();
          } catch {
            detail = "";
          }
        }
        throw new Error(detail || `start failed (${res.status})`);
      }
      return res.json().catch(() => ({}));
    },
    [runId]
  );

  const sendHitl = useCallback(
    async (target: string, action: string, text: string, opts?: { preemption?: string }) => {
      // A redirect can carry a NEW target URL ("the challenge moved here") — pull
      // the first URL out of the text and send it as `url` so the worker retargets
      // its next turn. A message prefixed with "standing:" (or 常驻:) is persistent
      // background guidance (VPS/SSH creds) injected into every future worker.
      const body: Record<string, unknown> = { target, action, text };
      // B: an explicit directive carries a preemption policy (how aggressively it
      // overrides in-flight work). Default soft_rebind (rebind next batch, no kill).
      if (opts?.preemption) body.preempt_policy = opts.preemption;
      if (action === "directive" && !opts?.preemption) body.preempt_policy = "soft_rebind";
      const m = text.match(/https?:\/\/[^\s"'<>]+/);
      if ((action === "redirect" || action === "directive") && m) body.url = m[0].replace(/[.,;)]+$/, "");
      // Explicit "standing:" / "常驻:" prefix → persistent guidance.
      const sm = text.match(/^\s*(standing|常驻|standing guidance)\s*[:：]\s*(.*)$/i);
      if (sm) { body.standing = true; body.text = sm[2]; }
      if (action === "mark_false" && text.trim()) {
        body.flag = text.trim();
      }
      // Auto-detect: a hint that hands over a RESOURCE (VPS / SSH / creds / a
      // reverse-shell host) is almost always meant to apply to ALL workers for the
      // rest of the run, not just the one turn — mark it standing so late-spawned
      // workers inherit it too (operators kept forgetting the "standing:" prefix and
      // the VPS hint never reached new workers). Heuristic, conservative: only fires
      // on clear resource-handover signals.
      else if (action === "hint" &&
               /\b(ssh|vps|反弹|reverse[- ]?shell|root@|端口转发|port[- ]?forward|credential|凭证|账号|密码|password|跳板|中转)\b/i.test(text)) {
        body.standing = true;
      }
      await fetch(`${API}/api/runs/${runId}/hitl`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
    },
    [runId]
  );

  // "继续做题": relaunch the FULL swarm on a finished run (reuses its workspace so
  // verified facts carry over). Optional `text` folds an operator hint into the
  // re-solve's challenge description.
  const resolve = useCallback(
    async (text?: string) => {
      const body: Record<string, unknown> = {};
      if (text && text.trim()) body.challenge = { description: text.trim() };
      await fetch(`${API}/api/runs/${runId}/resolve`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
    },
    [runId]
  );

  return { deck, connected, start, sendHitl, resolve };
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
        const r = await fetch(`${API}/api/runs?archived=1`, { signal: ctrl.signal });
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
    const r = await fetch(`${API}/api/runs/${runId}/uploads`, {
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
    const r = await fetch(`${API}/api/runs`, { method: "POST" });
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
  patch: { pinned?: boolean; archived?: boolean; name?: string; folder_id?: string | null; order?: number }
): Promise<boolean> {
  try {
    const r = await fetch(`${API}/api/runs/${runId}`, {
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
    const r = await fetch(`${API}/api/runs/${runId}`, { method: "DELETE" });
    return r.ok;
  } catch {
    return false;
  }
}

/** Open the run's workspace dir in the host file manager (operator-local). */
export async function openWorkspace(runId: string): Promise<boolean> {
  try {
    const r = await fetch(`${API}/api/runs/${runId}/open`, { method: "POST" });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}

// ── engine status ────────────────────────────────────────────────────────────

/** Per-engine availability + health. */
export interface EngineStatus {
  engine: string;
  bin: string;
  available: boolean;
  healthy?: boolean;
  health_detail?: string;
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

/** Run the DEEP self-check (slow — exercises auth). `backend` picks local (host
 *  CLI + auth) vs container (docker run --rm: image + CLI launchable inside the
 *  worker image). On-demand, not polled. */
export async function getEngineHealth(backend: "local" | "container" = "local"): Promise<EngineHealth[]> {
  try {
    const r = await fetch(`${API}/api/engines/health?backend=${backend}`);
    const j = await r.json();
    return (j.engines ?? []) as EngineHealth[];
  } catch {
    return [];
  }
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
        const r = await fetch(`${API}/api/engines`);
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

export async function listFolders(): Promise<Folder[]> {
  try {
    const r = await fetch(`${API}/api/folders`);
    const j = await r.json();
    return (j.folders ?? []) as Folder[];
  } catch {
    return [];
  }
}

export async function createFolder(name: string): Promise<Folder | null> {
  try {
    const r = await fetch(`${API}/api/folders`, {
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
    const r = await fetch(`${API}/api/folders/${id}`, {
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
    const r = await fetch(`${API}/api/folders/${id}`, { method: "DELETE" });
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

/** The default worker roster the dispatch path falls back to. Mirrors the
 *  backend WorkerConfigStore (apps/web/worker_config.py). */
export interface WorkerSettings {
  engines: string[];
  start_workers: number;
  max_workers: number;
  worker_backend: "local" | "container";
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
      wall_clock_budget: number;
      review?: {
        enabled?: boolean;
        engine?: string;
        timeout?: number;
        after_race?: boolean;
        after_fruitless_workers?: number;
        after_duplicate_intents?: number;
        on_course_correct?: boolean;
        on_reason_dry?: boolean;
        on_candidate_spike?: boolean;
        on_operator_hint?: boolean;
        allow_review_fallback?: boolean;
        every_completed_workers?: number;
        candidate_spike_threshold?: number;
        max_concurrent?: number;
        cooldown_events?: number;
        max_review_workers?: number;
      };
    };
    budgets: { max_total_workers: number; cost_budget_usd: number };
  };
  llm_profiles: {
    planner: { provider: string; model: string; base_url?: string };
    titler: { provider: string; model: string; base_url?: string };
  };
  runtime_profiles: {
    id: string;
    backend: "local" | "container";
    label: string;
    network?: string;
    memory?: string;
    cpus?: string;
    pids_limit?: number;
  }[];
  worker_profiles: {
    id: string;
    name?: string;
    engine: string;
    transport: string;
    auth: string;
    credential_mode?: string;
    credential_account: string;
    api_key_ref?: string;
    base_url?: string;
    wire_api?: string;
    runtime: string;
    roles: string[];
    race: boolean;
    max_running: number;
    max_review_running?: number;
    priority: number;
    model?: string;
    enabled: boolean;
  }[];
  overrides: Record<string, { engines: string[]; start_workers: number }>;
}

export interface CredentialAccount {
  account_id: string;
  engine: string;
  mode: string;
  present: boolean;
  writable_state: boolean;
  updated_at?: number | null;
  details: Record<string, unknown>;
}

export interface WorkerModelOptions {
  allow_custom: boolean;
  models: Record<string, { id: string; label: string }[]>;
}

export async function getWorkerSettings(): Promise<WorkerSettings | null> {
  try {
    const r = await fetch(`${API}/api/settings/workers`);
    if (!r.ok) return null;
    const j = await r.json();
    return (j.config ?? null) as WorkerSettings | null;
  } catch {
    return null;
  }
}

export async function getWorkerModelOptions(): Promise<WorkerModelOptions> {
  try {
    const r = await fetch(`${API}/api/settings/worker-models`);
    if (!r.ok) return { allow_custom: true, models: {} };
    const j = await r.json();
    return {
      allow_custom: Boolean(j.allow_custom ?? true),
      models: (j.models ?? {}) as WorkerModelOptions["models"],
    };
  } catch {
    return { allow_custom: true, models: {} };
  }
}

export async function testWorkerProfileModel(
  profile: WorkerSettings["worker_profiles"][number],
  model: string,
  backend: "local" | "container"
): Promise<{ ok: boolean; detail: string; model: string; engine: string }> {
  try {
    const r = await fetch(`${API}/api/settings/worker-model/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ profile, model, backend }),
    });
    const j = await r.json().catch(() => ({}));
    return {
      ok: !!j.ok,
      detail: String(j.detail ?? ""),
      model: String(j.model ?? model),
      engine: String(j.engine ?? profile.engine),
    };
  } catch (e) {
    return { ok: false, detail: String(e), model, engine: profile.engine };
  }
}

/** Update the default roster. Returns the persisted config, or null on
 *  failure (e.g. 400 for an invalid roster). */
export async function putWorkerSettings(
  patch: Partial<WorkerSettings>
): Promise<WorkerSettings | null> {
  try {
    const r = await fetch(`${API}/api/settings/workers`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    });
    if (!r.ok) return null;
    const j = await r.json();
    return (j.config ?? null) as WorkerSettings | null;
  } catch {
    return null;
  }
}

export async function listCredentialAccounts(): Promise<CredentialAccount[]> {
  try {
    const r = await fetch(`${API}/api/settings/credential-accounts`);
    if (!r.ok) return [];
    const j = await r.json();
    return (j.accounts ?? []) as CredentialAccount[];
  } catch {
    return [];
  }
}

export async function putCredentialAccount(
  accountId: string,
  body: { engine: string; secret?: string; codex_auth_json?: string; base_url?: string; target_engine?: string }
): Promise<CredentialAccount | null> {
  try {
    const r = await fetch(`${API}/api/settings/credential-accounts/${encodeURIComponent(accountId)}`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!r.ok) return null;
    const j = await r.json();
    return (j.account ?? null) as CredentialAccount | null;
  } catch {
    return null;
  }
}

export async function deleteCredentialAccount(accountId: string): Promise<boolean> {
  try {
    const r = await fetch(`${API}/api/settings/credential-accounts/${encodeURIComponent(accountId)}`, {
      method: "DELETE",
    });
    if (!r.ok) return false;
    const j = await r.json();
    return Boolean(j.ok);
  } catch {
    return false;
  }
}

export type SystemLoginStatus = "present" | "absent" | "unknown";

/** Host-side login presence per engine (drives the local-mode credentials UI). */
export async function getSystemLogin(): Promise<Record<string, SystemLoginStatus>> {
  try {
    const r = await fetch(`${API}/api/settings/system-login`);
    if (!r.ok) return {};
    const j = await r.json();
    return (j.logins ?? {}) as Record<string, SystemLoginStatus>;
  } catch {
    return {};
  }
}

/** Test the planner/titler endpoint the operator is editing (key from .env). */
export async function testLlmEndpoint(
  which: "planner" | "titler",
  base_url: string,
  model: string
): Promise<{ ok: boolean; detail: string; model: string }> {
  try {
    const r = await fetch(`${API}/api/settings/llm/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ which, base_url, model }),
    });
    const j = await r.json().catch(() => ({}));
    return { ok: !!j.ok, detail: String(j.detail ?? ""), model: String(j.model ?? model) };
  } catch (e) {
    return { ok: false, detail: String(e), model };
  }
}

/** Test a registered credential account. local → host probe with the account's
 *  env; container → real `docker run --rm` plumbing test. Never host-fallback. */
export async function testCredentialAccount(
  accountId: string,
  engine: string,
  backend: "local" | "container"
): Promise<{ ok: boolean; detail: string; layer?: string }> {
  try {
    const r = await fetch(
      `${API}/api/settings/credential-accounts/${encodeURIComponent(accountId)}/test`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ engine, backend }),
      }
    );
    const j = await r.json().catch(() => ({}));
    return { ok: !!j.ok, detail: String(j.detail ?? ""), layer: j.layer };
  } catch (e) {
    return { ok: false, detail: String(e) };
  }
}

/** Unify backend + runtime across all enabled profiles (one-container-per-run). */
export async function putRuntimeEnvironment(
  backend: "local" | "container",
  runtime_id: string
): Promise<WorkerSettings | null> {
  try {
    const r = await fetch(`${API}/api/settings/runtime-environment`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ backend, runtime_id }),
    });
    if (!r.ok) return null;
    const j = await r.json();
    return (j.config ?? null) as WorkerSettings | null;
  } catch {
    return null;
  }
}

/** Operator runtime control: add a worker for an engine to a LIVE run
 *  (omit engine → coordinator picks heterogeneity-aware). */
export async function spawnWorker(runId: string, engine?: string): Promise<boolean> {
  try {
    const r = await fetch(`${API}/api/runs/${runId}/workers`, {
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
    const r = await fetch(`${API}/api/runs/${runId}/workers`, {
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
