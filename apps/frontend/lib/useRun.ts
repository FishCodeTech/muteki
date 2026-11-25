"use client";

import { startTransition, useCallback, useEffect, useRef, useState, useSyncExternalStore } from "react";
import { randomUuid } from "./browserCrypto";
import { DeckState, EventType, GuardApprovalDecisionAction, MutekiEvent, ProductionGuardState, emptyDeck, reduce } from "./events";
import { deleteRunProjection, loadRunProjection, saveRunProjection } from "./runProjectionCache";
import { seedGid } from "./run-events/helpers";
import { normalizeGuardStateSnapshot } from "./run-events/reducers/guard";
import { API, apiFetch, authTicket, currentAuthScope, currentServiceOrigin } from "./serviceAuth";
import { isDesktopWorkspaceActive } from "./desktopEnvironment";
import { readCacheIdentity } from "./readCache";
export { API, apiFetch, authTicket, login, logout, checkAuth, onAuthRequired, currentAuthGeneration, currentAuthScope, currentAuthPersistenceWarning, resetAuthForServiceChange } from "./serviceAuth";

const CONTROL_CAS_ACTIONS = new Set([
  "pause", "freeze", "resume", "thaw", "stop", "complete", "emergency_stop",
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

export type RunStatus = "draft" | "idle" | "running" | "paused" | "stopping" | "interrupted" | "solved" | "finished" | "failed";

const isDraftRunId = (id: string) => id.startsWith("draft-");

export type OperatorTriageLabel = "pending_followup" | "processed" | "ignored";

export interface RunSummary {
  run_id: string;
  mode?: "ctf" | "pentest";
  kind?: "swarm" | "local_pi" | "idle";
  operator_label?: OperatorTriageLabel | null;
  operator_label_updated_at?: string | null;
  local_pi_session?: Record<string, any> | null;
  chat_thread_id?: string | null;
  local_pi_config?: {
    model?: string | null;
    account_id?: string | null;
    endpoint?: string | null;
    access_mode?: string | null;
    offline_env?: boolean | null;
    skills?: string[];
  } | null;
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
  order?: number | null;
  created_seq?: number;
  ts?: number;
  updated: number;
  updated_at?: number;
}

export type FolderSection = "active" | "recent";

export interface Folder {
  id: string;
  name: string;
  order: number;
  mode: "ctf" | "pentest";
  section: FolderSection;
}

const RECENT_DECK_LIMIT = 8;
const recentDecks = new Map<string, { seq: number; deck: DeckState }>();

function recentDeckKey(runId: string): string {
  return `${currentAuthScope()}\n${runId}`;
}

function recentDeck(runId: string): { seq: number; deck: DeckState } | undefined {
  if (!runId || isDraftRunId(runId)) return undefined;
  return recentDecks.get(recentDeckKey(runId));
}

function rememberDeck(runId: string, seq: number, deck: DeckState): void {
  if (!runId || isDraftRunId(runId) || seq <= 0 || deck.runId !== runId) return;
  const key = recentDeckKey(runId);
  recentDecks.delete(key);
  recentDecks.set(key, { seq, deck });
  for (const oldest of recentDecks.keys()) {
    if (recentDecks.size <= RECENT_DECK_LIMIT) break;
    recentDecks.delete(oldest);
  }
}

export function useRun(runId: string) {
  const [deck, setDeck] = useState<DeckState>(() => recentDeck(runId)?.deck ?? emptyDeck(runId));
  const [deckRunId, setDeckRunId] = useState(runId);
  if (deckRunId !== runId) {
    setDeckRunId(runId);
    setDeck(recentDeck(runId)?.deck ?? emptyDeck(runId));
  }
  const [connected, setConnected] = useState(false);
  const [replayEmpty, setReplayEmpty] = useState(false);
  const esRef = useRef<EventSource | null>(null);
  const deckRef = useRef(deck);
  const lastSeqRef = useRef(0);

  useEffect(() => {
    esRef.current?.close();
    esRef.current = null;
    const recent = recentDeck(runId);
    const initialDeck = recent?.deck ?? emptyDeck(runId);
    deckRef.current = initialDeck;
    lastSeqRef.current = recent?.seq ?? 0;
    setDeck(initialDeck);
    setConnected(false);
    setReplayEmpty(false);
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
    let followupRevision = 0;

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
    const handler = (e: MessageEvent) => {
      try {
        const ev = JSON.parse(e.data) as MutekiEvent;
        if (ev.event_type.startsWith("followup.") || ev.event_type === EventType.RUN_REOPENED) followupRevision += 1;
        setReplayEmpty(false);
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
        if (frame.events.length) setReplayEmpty(false);
        let next = replayDeck ?? deckRef.current;
        for (const ev of frame.events) next = reduce(next, ev);
        replayDeck = next;
        replaySeq = Math.max(replaySeq, Number(frame.last_seq) || 0);
      } catch {}
    };
    const syncFollowups = async () => {
      const revision = followupRevision;
      try {
        const res = await apiFetch(`/api/runs/${runId}/followups`);
        const data = await res.json();
        if (!res.ok || data.ok === false) throw new Error(data?.error?.message || data?.detail || `followups failed (${res.status})`);
        if (cancelled) return;
        const active = data.jobs.find((job: { kind: string; state: string }) => ["accepted", "running", "queued"].includes(job.state));
        const current = deckRef.current;
        const lifecycleChanged = followupRevision !== revision;
        publishDeck({ ...current, hasWriteup: current.hasWriteup || data.has_writeup,
          ...(!lifecycleChanged ? {
            followupPending: Boolean(active),
            activeFollowupJobId: active?.id,
            activeFollowupKind: active?.kind,
            followupProgress: current.followupProgress || active?.progress,
          } : {}) }, true);
      } catch (error) { if (!cancelled) console.error("Followup state sync failed", error); }
    };
    const syncGuardState = async () => {
      try {
        const snapshot = await fetchRunGuardState(runId);
        if (cancelled || !snapshot) return;
        const current = deckRef.current;
        if (!snapshot.enabled && !current.productionGuard?.enabled) return;
        publishDeck({ ...current, productionGuard: snapshot }, true);
      } catch (error) {
        if (!cancelled) console.error("Execution guard state sync failed", error);
      }
    };
    const replayCompleteHandler = (e: MessageEvent) => {
      try {
        const frame = JSON.parse(e.data) as { last_seq?: number };
        replaySeq = Math.max(replaySeq, Number(frame.last_seq) || 0);
      } catch {}
      if (replaySeq === 0 && !deckRef.current.started) setReplayEmpty(true);
      const replayed = replayDeck;
      if (replayed) startTransition(() => publishDeck(replayed, true));
      lastSeqRef.current = Math.max(lastSeqRef.current, replaySeq);
      replayProtocolReady = true;
      replayDeck = null;
      replaySeq = lastSeqRef.current;
      schedulePersist(0);
      void syncFollowups();
      void syncGuardState();
    };

    const connect = async () => {
      const version = ++connectionVersion;
      let ticket = "";
      try {
        ticket = await authTicket();
      } catch {
        if (cancelled || version !== connectionVersion) return;
        const delay = reconnectDelay;
        reconnectDelay = Math.min(reconnectDelay * 2, 10_000);
        reconnectTimer = window.setTimeout(() => {
          reconnectTimer = null;
          void connect();
        }, delay);
        return;
      }
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
        void syncGuardState();
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
      Object.values(EventType).forEach((name) => es.addEventListener(name, handler as EventListener));
      es.onmessage = handler;
    };

    (async () => {
      const cached = recent ? null : await loadRunProjection(runId);
      if (cancelled) return;
      if (cached) {
        seedGid(cached.deck.chat.map((message) => message.id));
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
      rememberDeck(runId, lastSeqRef.current, deckRef.current);
      esRef.current?.close();
      esRef.current = null;
    };
  }, [runId]);

  const start = useCallback(
    async (body: Record<string, any>, overrideRunId?: string) => {
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
      contextId?: string;
    }) => {
      const body: Record<string, unknown> = { target, action, text };
      if (opts?.preemption) body.preempt_policy = opts.preemption;
      if (opts?.requestId) body.request_id = opts.requestId;
      if (opts?.contextId) body.context_id = opts.contextId;
      if (opts?.commandId) body.command_id = opts.commandId;
      if (opts?.expectedGeneration != null) {
        body.expected_generation = opts.expectedGeneration;
      } else if (CONTROL_CAS_ACTIONS.has(action)) {
        body.expected_generation = deck.controlGeneration;
      }
      if (action === "directive" && !opts?.preemption) body.preempt_policy = "soft_rebind";
      if (action === "mark_false" && text.trim()) {
        body.flag = text.trim();
      }
      const res = await apiFetch(`/api/runs/${runId}/control`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      const data: any = await res.json().catch(() => ({}));
      if (!res.ok || data?.ok === false) {
        throw new Error(data?.error?.message || data?.detail || data?.effect?.error || data?.effect?.detail || `control failed (${res.status})`);
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
      throw new Error(data?.error?.message || data?.detail || `progress request failed (${res.status})`);
    }
    return data as { ok: true; status: "published" | "unchanged"; brief_id?: string | null };
  }, [runId]);

  const startFollowup = useCallback(async (
    kind: "ask" | "writeup",
    question: string,
    opts?: { templateId?: string; customPrompt?: string },
  ) => {
    const res = await apiFetch(`/api/runs/${runId}/followups`, {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        kind,
        question,
        request_id: randomUuid(),
        ...(opts?.templateId ? { template_id: opts.templateId } : {}),
        ...(opts?.customPrompt ? { custom_prompt: opts.customPrompt } : {}),
      }),
    });
    const data = await res.json();
    if (!res.ok || data.ok === false) throw new Error(data?.error?.message || data?.detail || `followup failed (${res.status})`);
    return data as { ok: true; job: { id: string } };
  }, [runId]);
  const cancelFollowup = useCallback(async (id: string) => {
    const res = await apiFetch(`/api/runs/${runId}/followups/${encodeURIComponent(id)}`, { method: "DELETE" });
    if (!res.ok) {
      const data = await res.json();
      throw new Error(data?.error?.message || data?.detail || `cancel failed (${res.status})`);
    }
  }, [runId]);
  const resolve = useCallback(async (text?: string) => {
    const res = await apiFetch(`/api/runs/${runId}/resolve`, {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ command_id: randomUuid(),
        expected_execution_generation: deckRef.current.executionGeneration,
        expected_generation: deckRef.current.controlGeneration,
        ...(text?.trim() ? { guidance: { action: "hint", text: text.trim() } } : {}),
      }),
    });
    const data = await res.json();
    if (!res.ok || data?.ok === false) throw new Error(data?.error?.message || data?.detail || `resolve failed (${res.status})`);
  }, [runId]);

  const refreshGuardState = useCallback(async () => {
    if (!runId || isDraftRunId(runId)) return null;
    const snapshot = await fetchRunGuardState(runId);
    if (!snapshot) return null;
    const current = deckRef.current;
    if (current.runId !== runId) return snapshot;
    const next = { ...current, productionGuard: snapshot };
    deckRef.current = next;
    setDeck(next);
    return snapshot;
  }, [runId]);

  return { deck, connected, replayEmpty, start, sendHitl, requestProgress, resolve, startFollowup, cancelFollowup, refreshGuardState };

}

export function useRunList(pollMs = 4000, bump = 0) {
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const lastResponse = useRef<string | null>(null);
  useEffect(() => {
    let alive = true;
    let inFlight: AbortController | null = null;
    const load = async () => {
      if (inFlight || !isDesktopWorkspaceActive()) return;
      const ctrl = new AbortController();
      inFlight = ctrl;
      const timeout = window.setTimeout(() => ctrl.abort(), Math.max(3000, Math.min(10000, pollMs * 2)));
      try {
        const r = await apiFetch(`/api/runs?archived=1`, { signal: ctrl.signal });
        const body = await r.text();
        if (!alive || body === lastResponse.current) return;
        const j = JSON.parse(body);
        const next = j.runs ?? [];
        lastResponse.current = body;
        setRuns(next);
      } catch {
      } finally {
        window.clearTimeout(timeout);
        if (inFlight === ctrl) inFlight = null;
      }
    };
    load();
    const id = setInterval(load, pollMs);
    window.addEventListener("muteki:workspace-activated", load);
    return () => {
      alive = false;
      inFlight?.abort();
      inFlight = null;
      clearInterval(id);
      window.removeEventListener("muteki:workspace-activated", load);
    };
  }, [pollMs, bump]);
  return runs;
}

export interface SavedFile {
  name: string;
  path: string;
  size: number;
}

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

export async function newRun(mode: "ctf" | "pentest" = "ctf"): Promise<string> {
  const res = await apiFetch("/api/runs", {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ mode }),
  });
  const body = await res.json().catch(() => null);
  if (!res.ok || typeof body?.run_id !== "string" || !body.run_id) {
    throw new Error(body?.detail || body?.error?.message || `Could not create run (${res.status})`);
  }
  return body.run_id;
}

export async function patchRun(
  runId: string,
  patch: {
    pinned?: boolean;
    archived?: boolean;
    name?: string;
    folder_id?: string | null;
    order?: number;
    now?: number;
    operator_label?: OperatorTriageLabel | null;
  }
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

export interface LocalPiSkillDependency {
  name: string;
  purpose: string;
  available: boolean;
  path?: string | null;
}

export interface LocalPiSkillCatalogItem {
  id: string;
  name: string;
  version: string;
  requires: string;
  completion_basis: string;
  summary: string;
  body?: string;
  stage_path?: string;
  delivery_mode?: string;
  sha256_prefix?: string;
  default_enabled?: boolean;
  dependencies?: LocalPiSkillDependency[];
  missing_dependencies?: string[];
  all_dependencies_met?: boolean;
  path?: string;
}

export interface LocalPiToolManifestItem {
  id?: string;
  name: string;
  category?: string;
  category_label?: string;
  source?: string;
  scope?: string;
  delivery?: string;
  enabled?: boolean;
  description?: string;
  summary?: string;
}

export interface LocalPiDisabledCapabilityItem {
  id: string;
  name?: string;
  source?: string;
  enabled?: boolean;
  reason?: string;
}

export interface LocalPiAccountOption {
  id: string;
  label: string;
  source: string;
  connection?: string;
  endpoint?: string;
  default_model?: string;
  usable: boolean;
  detail?: string;
}

export interface LocalPiDiscoveredModel {
  id: string;
  key?: string;
  source_ref?: string;
  raw_name?: string;
  label: string;
  source: "ollama_local" | "host_pi_config" | "muteki_catalog" | "local_pi_provider" | "credential_account" | string;
  source_label?: string;
  endpoint?: string;
  parameter_size?: string | null;
  quantization_level?: string | null;
  family?: string | null;
  size_bytes?: number | null;
  context_window?: number | null;
  max_tokens?: number | null;
  cost?: unknown;
  readiness?: string;
}

export interface LocalPiProbeResult {
  probe_key: string;
  ok: boolean;
  code?: string;
  detail: string;
  account_id: string;
  requested_model?: string | null;
  served_model?: string | null;
  endpoint?: string;
  elapsed_ms: number;
  first_token_ms?: number | null;
  tested_at: string;
  checks?: {
    text_response?: { status: string; ok?: boolean; detail: string };
    streaming?: { status: string; delta_count?: number; detail: string };
    tool_call?: { status: string; tool_names?: string[]; detail: string };
    usage_reporting?: {
      status: string;
      input_tokens?: number | null;
      output_tokens?: number | null;
      total_tokens?: number | null;
      detail: string;
    };
    context_window?: {
      status: string;
      context_window?: number | null;
      max_tokens?: number | null;
      detail: string;
    };
  };
}

export interface LocalPiProviderModel {
  id: string;
  name?: string;
  context_window?: number;
  max_tokens?: number;
  reasoning?: boolean;
}

export interface LocalPiProvider {
  id: string;
  name: string;
  api: "openai-completions" | "anthropic-messages";
  base_url: string;
  models: LocalPiProviderModel[];
  has_api_key?: boolean;
}

export interface LocalPiProviderSave extends Omit<LocalPiProvider, "id"> {
  id?: string;
  api_key?: string;
  clear_api_key?: boolean;
}

export interface LocalPiSettings {
  enabled: boolean;
  use_agent_credentials: boolean;
  default_account_id: string;
  default_model: string;
  default_endpoint: string;
  default_access_mode: "full-access" | "auto-accept-edits" | "supervised";
  default_skills: string[];
  offline_env: boolean;
  providers: LocalPiProvider[];
  updated_at?: string | null;
}

export interface LocalPiSettingsPatch extends Omit<Partial<LocalPiSettings>, "providers"> {
  providers?: LocalPiProviderSave[];
}

export interface LocalPiCatalogResponse {
  settings?: LocalPiSettings;
  runtime?: {
    adapter_id: string;
    solver_id: string;
    pi_binary?: string | null;
    pi_version?: string | null;
    ready: boolean;
    host_login?: {
      available: boolean;
      status: string;
      detail: string;
    };
    ollama_status?: {
      reachable: boolean;
      endpoint: string;
      model_count: number;
      detail: string;
    };
  };
  accounts?: LocalPiAccountOption[];
  providers?: LocalPiProvider[];
  models?: LocalPiDiscoveredModel[];
  latest_probe?: LocalPiProbeResult | null;
  probe_results?: Record<string, LocalPiProbeResult>;
  skills: LocalPiSkillCatalogItem[];
  tools: LocalPiToolManifestItem[];
  pi_builtin_tools?: LocalPiToolManifestItem[];
  muteki_readonly_tools?: LocalPiToolManifestItem[];
  disabled_capabilities: LocalPiDisabledCapabilityItem[];
  external_extensions?: {
    isolation_policy: string;
    managed_native_skills: Array<{
      id?: string;
      name?: string;
      mode?: string;
      enabled?: boolean;
      description?: string;
    }>;
    managed_packages: Array<{
      id?: string;
      spec?: string;
      mode?: string;
      enabled?: boolean;
      install_status?: string;
    }>;
    host_global_skills: string[];
  };
  candidate_sources?: Array<{
    repo: string;
    license: string;
    focus: string;
    status: string;
    reason: string;
  }>;
  environment?: {
    runtime: string;
    adapter_id: string;
    pi_binary?: string | null;
    cwd: string;
    os_sandbox_verified: boolean;
    network_isolation_verified: boolean;
    status: string;
    summary: string;
    checks?: Array<{
      id: string;
      label: string;
      status: "verified" | "unverified" | string;
      detail: string;
    }>;
    optional_binaries?: LocalPiSkillDependency[];
  };
  multi_run_summary?: {
    total_runs: number;
    active_runs: number;
    generating_runs: number;
    waiting_operator_runs: number;
    completed_runs: number;
    failed_runs: number;
  };
}

export async function fetchLocalPiCatalog(): Promise<LocalPiCatalogResponse> {
  try {
    const r = await apiFetch(`/api/runs/local-pi/catalog`);
    if (!r.ok) return { skills: [], tools: [], disabled_capabilities: [] };
    const j = await r.json();
    return {
      settings: j.settings,
      runtime: j.runtime,
      accounts: Array.isArray(j.accounts) ? j.accounts : [],
      providers: Array.isArray(j.providers) ? j.providers : [],
      models: Array.isArray(j.models) ? j.models : [],
      latest_probe: j.latest_probe && typeof j.latest_probe === "object" && j.latest_probe.probe_key ? j.latest_probe : null,
      probe_results: j.probe_results && typeof j.probe_results === "object" ? j.probe_results : {},
      skills: Array.isArray(j.skills) ? j.skills : [],
      tools: Array.isArray(j.tools) ? j.tools : [],
      pi_builtin_tools: Array.isArray(j.pi_builtin_tools) ? j.pi_builtin_tools : [],
      muteki_readonly_tools: Array.isArray(j.muteki_readonly_tools) ? j.muteki_readonly_tools : [],
      disabled_capabilities: Array.isArray(j.disabled_capabilities) ? j.disabled_capabilities : [],
      external_extensions: j.external_extensions,
      candidate_sources: Array.isArray(j.candidate_sources) ? j.candidate_sources : [],
      environment: j.environment,
      multi_run_summary: j.multi_run_summary,
    };
  } catch {
    return { skills: [], tools: [], disabled_capabilities: [] };
  }
}

export interface LocalPiLoadedSkill {
  id: string;
  name?: string;
  version?: string;
  summary?: string;
  requires?: string;
  completion_basis?: string;
  required_binaries?: string[];
  source?: "builtin" | "extension" | string;
  path?: string;
  sha256_prefix?: string;
  exists?: boolean;
  staged?: boolean;
}

export interface LocalPiWorkspaceFile {
  name: string;
  rel_path: string;
  size_bytes: number;
  exists: boolean;
  readable: boolean;
  kind?: string;
}

export interface LocalPiInspectorSnapshot {
  run_id: string;
  kind: string;
  operator_label?: OperatorTriageLabel | null;
  operator_label_updated_at?: string | null;
  tools_and_skills: {
    adapter_id: string;
    solver_id: string;
    chat_thread_id?: string | null;
    live_process_active: boolean;
    provider_session_id: string | null;
    execution_generation: number;
    session_state: string;
    turn_active: boolean;
    turn_count: number;
    resume_status: {
      resumable: boolean;
      provider_session_id?: string;
      session_file?: string;
      error?: string;
      continuation_row_present?: boolean;
    };
    model: {
      id?: string;
      account_id?: string;
      endpoint?: string;
      effort?: string | null;
      access_mode?: string;
      readiness?: string;
      context_window?: number | null;
      max_tokens?: number | null;
      cost?: unknown;
    };
    loaded_tools: Array<LocalPiToolManifestItem | string>;
    muteki_readonly_tools?: LocalPiToolManifestItem[];
    disabled_capabilities: Array<LocalPiDisabledCapabilityItem | string>;
    loaded_skills: LocalPiLoadedSkill[];
    environment: NonNullable<LocalPiCatalogResponse["environment"]>;
    context?: {
      used_tokens?: number | null;
      limit?: number | null;
      total_processed_tokens?: number | null;
      source?: string | null;
      updated_at?: string | null;
    } | null;
    token_usage?: {
      input_tokens?: number | null;
      output_tokens?: number | null;
      cached_input_tokens?: number | null;
      total_tokens?: number | null;
    } | null;
    last_turn_usage?: {
      input_tokens?: number | null;
      output_tokens?: number | null;
      cached_input_tokens?: number | null;
      total_tokens?: number | null;
    } | null;
    last_error?: { code?: string; message?: string; at?: string } | string | null;
  };
  materials_and_artifacts: {
    workspace_root: string;
    uploads: LocalPiWorkspaceFile[];
    workspace_files: LocalPiWorkspaceFile[];
    tool_artifacts: Array<{
      seq?: number;
      artifact_id: string;
      rel_path: string;
      tool_call_id?: string;
      tool_name?: string;
      created_at?: string;
      exists: boolean;
      readable: boolean;
      status: "ok" | "missing" | "unreadable" | string;
      preview?: string;
      referenced_files?: Array<{ path: string; exists: boolean; readable: boolean; status: string }>;
    }>;
  };
}

async function readJsonDetail(r: Response): Promise<{ body: Record<string, any>; detail?: string }> {
  const body = await r.json().catch(() => ({}));
  const detail = body?.detail ?? body?.error?.message ?? body?.message;
  return { body, detail: detail ? String(detail) : r.ok ? undefined : `HTTP ${r.status}` };
}

export async function saveLocalPiSettings(
  patch: LocalPiSettingsPatch,
): Promise<{ ok: boolean; settings?: LocalPiSettings; detail?: string }> {
  try {
    const r = await apiFetch(`/api/runs/local-pi/settings`, {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
    });
    const { body, detail } = await readJsonDetail(r);
    return { ok: r.ok && Boolean(body.ok), settings: body.settings as LocalPiSettings | undefined, detail };
  } catch (e) {
    return { ok: false, detail: String(e) };
  }
}

export async function fetchLocalPiProviderModels(body: {
  provider_id?: string;
  api?: string;
  base_url?: string;
  api_key?: string;
}): Promise<{ ok: boolean; models?: Array<{ id: string; name?: string }>; code?: string; detail?: string }> {
  try {
    const r = await apiFetch(`/api/runs/local-pi/providers/models`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });
    const j = await r.json().catch(() => ({}));
    if (j && j.ok) {
      return { ok: true, models: Array.isArray(j.models) ? j.models : [] };
    }
    return {
      ok: false,
      code: j?.code ? String(j.code) : undefined,
      detail: j?.detail ? String(j.detail) : `HTTP ${r.status}`,
    };
  } catch (e) {
    return { ok: false, detail: String(e) };
  }
}

export async function probeLocalPiCompatibility(params?: {
  account_id?: string;
  model?: string;
  endpoint?: string;
}): Promise<{ ok: boolean; probe?: LocalPiProbeResult; detail?: string }> {
  try {
    const r = await apiFetch(`/api/runs/local-pi/probe`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(params ?? {}),
    });
    const { body, detail } = await readJsonDetail(r);
    const probe = body.probe as LocalPiProbeResult | undefined;
    return { ok: r.ok && Boolean(probe?.ok), probe, detail: probe?.detail ?? detail };
  } catch (e) {
    return { ok: false, detail: String(e) };
  }
}

export async function fetchLocalPiInspector(runId: string): Promise<LocalPiInspectorSnapshot | null> {
  if (!runId || isDraftRunId(runId)) return null;
  const r = await apiFetch(`/api/runs/${runId}/local-pi/inspector`);
  const { body, detail } = await readJsonDetail(r);
  if (!r.ok) throw new Error(detail || `HTTP ${r.status}`);
  return (body.inspector ?? null) as LocalPiInspectorSnapshot | null;
}

export async function deleteRun(runId: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}`, { method: "DELETE" });
    if (r.ok) await deleteRunProjection(runId);
    return r.ok;
  } catch {
    return false;
  }
}

export async function openWorkspace(runId: string): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/runs/${runId}/open`, { method: "POST" });
    const j = await r.json().catch(() => ({}));
    return !!j.ok;
  } catch {
    return false;
  }
}


async function listFolders(): Promise<Folder[]> {
  try {
    const r = await apiFetch(`/api/folders`);
    const j = await r.json();
    return (j.folders ?? []) as Folder[];
  } catch {
    return [];
  }
}

export async function createFolder(name: string, mode: Folder["mode"], section: FolderSection): Promise<Folder | null> {
  try {
    const r = await apiFetch(`/api/folders`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ name, mode, section }),
    });
    const j = await r.json();
    return (j.folder ?? null) as Folder | null;
  } catch {
    return null;
  }
}

export async function renameFolder(id: string, name: string): Promise<boolean> {
  return updateFolder(id, { name });
}

export async function updateFolder(
  id: string,
  patch: { name?: string; section?: FolderSection; order?: number },
): Promise<boolean> {
  try {
    const r = await apiFetch(`/api/folders/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(patch),
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

export function useFolders(pollMs = 8000, bump = 0): Folder[] {
  const [folders, setFolders] = useState<Folder[]>([]);
  useEffect(() => {
    let alive = true;
    const load = async () => {
      if (!isDesktopWorkspaceActive()) return;
      const f = await listFolders();
      if (alive) setFolders((previous) => JSON.stringify(previous) === JSON.stringify(f) ? previous : f);
    };
    load();
    const id = setInterval(load, pollMs);
    window.addEventListener("muteki:workspace-activated", load);
    return () => { alive = false; clearInterval(id); window.removeEventListener("muteki:workspace-activated", load); };
  }, [pollMs, bump]);
  return folders;
}


export type LlmTemperatureMode = "default" | "custom" | "omit";

export type LlmProfile = {
  endpoint_id?: string;
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

export interface WorkerSettings {
  planner_configured?: boolean;
  engines: string[];
  max_workers: number;
  worker_backend: "local" | "container";
  worker_node_id?: string;
  worker_network: "bridge" | "host";
  worker_oci_runtime?: string;
  worker_memory?: string | null;
  worker_cpus?: number | string | null;
  worker_pids_limit?: number | string | null;
  worker_output_limit?: string | null;
  worker_disk_limit?: string | null;
  worker_resource_effective?: { worker_memory: string; worker_cpus: number; worker_pids_limit: number; worker_output_limit: string; worker_disk_limit: string };
  migration_notices?: { field: string; from: string; to: string | null; detail: string }[];
  effective_network?: string;
  network_requested?: string;
  network_reason?: string;
  network_error?: string;
  worker_container_scope: "run" | "shared";
  worker_privilege?: "root" | "default" | "elevated";
  wall_clock_budget: number;
  max_total_workers: number;
  cost_budget_usd: number;
  stage_policy: {
    prepare: Record<string, unknown>;
    coordinator: {
      wall_clock_budget: number;
      review?: {
        enabled?: boolean;
        engine?: string;
        timeout?: number;
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
    max_running: number;
    max_review_running?: number;
    priority: number;
    model?: string;
    reasoning_effort?: string;
    enabled: boolean;
  }[];
  seats?: {
    id: string;
    label: string;
    engine: string;
    credential_id: string;
    model?: string;
    reasoning_effort?: string;
    roles: string[];
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
  overrides: Record<string, { engines: string[] }>;
  dispatch_templates?: DispatchTemplate[];
}

export interface DispatchTemplate {
  id: string;
  name: string;
  seats: NonNullable<WorkerSettings["seats"]>;
}

export interface ModelCapabilityConfig {
  context_window?: number;
  max_output_tokens?: number;
  reasoning?: boolean;
  input?: string[];
  thinkingLevelMap?: Record<string, string | null>;
}

export interface DiscoveredCredentialModel {
  id: string;
  label?: string;
  provider?: string;
  context_window?: number;
  max_output_tokens?: number;
  reasoning?: boolean;
  input?: string[];
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
  test_status_preserved?: boolean;
  credential_format?: "oauth_token" | "auth_json" | "auth_home" | "api_key" | "unknown";
  mode: string;
  present: boolean;
  writable_state: boolean;
  updated_at?: number | null;
  model_capabilities?: Record<string, ModelCapabilityConfig>;
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
  runtime_instance?: string;
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
  discovered_models?: Array<{
    id?: string;
    label?: string;
    provider?: string;
    context_window?: number;
    max_output_tokens?: number;
    reasoning?: boolean | { supported?: boolean; levels?: string[]; default?: string };
    input?: string[];
  }>;
  configured_models?: string[];
  verified_models?: string[];
  default_model?: string;
}

export interface GlobalCredential {
  sharing?: {scope: "shared" | "environment"; readonly: boolean; reason: string};
  status_detail?: string;
  discovery_code?: string;
  detection_status?: "present" | "absent" | "unknown" | "disabled";
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
  present: boolean | null;
  status: string;
  models: string[];
  candidate_models?: string[];
  default_model?: string;
  model_capabilities?: Record<string, ModelCapabilityConfig>;
  model_catalog?: GlobalCredentialModelCatalog;
  model_catalogs?: Record<string, GlobalCredentialModelCatalog>;
  last_test?: GlobalCredentialLastTest | null;
  last_tests?: Record<string, GlobalCredentialLastTest>;
  usage: GlobalCredentialUsage[];
}

export interface ModelEndpoint {
  id: string;
  engine?: string;
  worker_engine?: string;
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
): Promise<{ ok: boolean; detail: string; models: string[]; discoveredModels?: DiscoveredCredentialModel[]; source?: string }> {
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
    const rawDiscovered: unknown[] = Array.isArray(payload.catalog?.discovered_models)
      ? payload.catalog.discovered_models
      : [];
    const discoveredModels: DiscoveredCredentialModel[] = [];
    for (const entry of rawDiscovered) {
      if (!entry || typeof entry !== "object") continue;
      const row = entry as Record<string, unknown>;
      const id = String(row.id ?? "").trim();
      if (!id) continue;
      const contextWindow = typeof row.context_window === "number" && Number.isInteger(row.context_window) && row.context_window > 0
        ? row.context_window
        : undefined;
      const maxOutputTokens = typeof row.max_output_tokens === "number" && Number.isInteger(row.max_output_tokens) && row.max_output_tokens > 0
        ? row.max_output_tokens
        : undefined;
      const reasoning = typeof row.reasoning === "boolean"
        ? row.reasoning
        : (row.reasoning && typeof row.reasoning === "object" && "supported" in row.reasoning
          ? Boolean((row.reasoning as { supported?: unknown }).supported)
          : undefined);
      const input = Array.isArray(row.input)
        ? row.input.map((v) => String(v || "").trim()).filter(Boolean)
        : undefined;
      discoveredModels.push({
        id,
        ...(typeof row.label === "string" && row.label ? { label: row.label } : {}),
        ...(typeof row.provider === "string" && row.provider ? { provider: row.provider } : {}),
        ...(contextWindow !== undefined ? { context_window: contextWindow } : {}),
        ...(maxOutputTokens !== undefined ? { max_output_tokens: maxOutputTokens } : {}),
        ...(reasoning !== undefined ? { reasoning } : {}),
        ...(input?.length ? { input } : {}),
      });
    }
    return {
      ok: response.ok && Boolean(payload.ok) && models.length > 0,
      detail: String(payload.detail?.message ?? payload.detail ?? (response.ok ? "" : `HTTP ${response.status}`)),
      models,
      discoveredModels,
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

export async function testCredentialDraft(body: {
  engine: string;
  backend: "local" | "container";
  connection: "official" | "custom_endpoint";
  model: string;
  base_url?: string;
  secret?: string;
  codex_auth_json?: string;
  provider?: string;
  runtime_instance?: string;
  model_capabilities?: Record<string, ModelCapabilityConfig>;
}): Promise<WorkerModelTestResult> {
  const fallback = { engine: body.engine, backend: body.backend, model: body.model };
  try {
    const response = await apiFetch(`/api/settings/credential-drafts/test`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(180_000),
    });
    const payload = await response.json().catch(() => ({}));
    return parseWorkerModelTestResult(payload, {
      ...fallback,
      ok: response.ok && Boolean(payload.ok),
      detail: String(payload.detail?.message ?? payload.detail ?? (response.ok ? "" : `HTTP ${response.status}`)),
    });
  } catch (error) {
    const detail = (error as Error)?.name === "TimeoutError" ? "测试超时（>180s），请检查端点后重试" : String(error);
    return parseWorkerModelTestResult(null, { ...fallback, ok: false, detail });
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

const workerSettingsRequests = new Map<string, Promise<WorkerSettings | null>>();

export function getWorkerSettings(): Promise<WorkerSettings | null> {
  const identity = readCacheIdentity();
  const pending = workerSettingsRequests.get(identity);
  if (pending) return pending;
  const request = (async () => {
    try {
      const r = await apiFetch(`/api/settings/workers`);
      if (!r.ok) return null;
      const j = await r.json();
      return (j.config ?? null) as WorkerSettings | null;
    } catch {
      return null;
    }
  })();
  workerSettingsRequests.set(identity, request);
  void request.finally(() => { if (workerSettingsRequests.get(identity) === request) workerSettingsRequests.delete(identity); });
  return request;
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

export type WorkerImageStatus = {
  image: string;
  daemon: { ok: boolean; detail: string };
  pulled: { ok: boolean; detail: string };
  version: { status: "match" | "mismatch" | "unknown"; expected: string | null; actual: string | null; detail: string };
  overall: "green" | "yellow" | "red";
  image_id?: string;
  architecture?: string;
  created?: string;
  size_bytes?: number;
  capabilities?: {
    declared: boolean;
    error?: string;
    architecture?: string;
    engines?: Record<string, { available: boolean; version?: string | null }> | null;
    browser?: Record<string, boolean> | null;
    tools?: unknown;
  } | null;
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
  error_kind?: string;
  proxy_mode?: string;
  proxy_display?: string;
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
    error_kind: typeof row.error_kind === "string" ? row.error_kind : undefined,
    proxy_mode: typeof row.proxy_mode === "string" ? row.proxy_mode : undefined,
    proxy_display: typeof row.proxy_display === "string" ? row.proxy_display : undefined,
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
        error_kind: typeof row.error_kind === "string" ? row.error_kind : undefined,
        proxy_mode: typeof row.proxy_mode === "string" ? row.proxy_mode : undefined,
        proxy_display: typeof row.proxy_display === "string" ? row.proxy_display : undefined,
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
  backend: "local" | "container",
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
      error_kind: typeof j.error_kind === "string" ? j.error_kind : undefined,
      proxy_mode: typeof j.proxy_mode === "string" ? j.proxy_mode : undefined,
      proxy_display: typeof j.proxy_display === "string" ? j.proxy_display : undefined,
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
  backend: "local" | "container",
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
        error_kind: typeof row.error_kind === "string" ? row.error_kind : undefined,
        proxy_mode: typeof row.proxy_mode === "string" ? row.proxy_mode : undefined,
        proxy_display: typeof row.proxy_display === "string" ? row.proxy_display : undefined,
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
  binding_kind?: "explicit" | "inherited" | "missing";
  effective_credential_id?: string;
};

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
    model_capabilities?: Record<string, ModelCapabilityConfig>;
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

export async function importHostWorkerLogin(
  accountId: string,
  engine: string,
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

const pendingWorkerSpawns = new Set<string>();
const workerSpawnListeners = new Set<() => void>();

function subscribeWorkerSpawns(listener: () => void) {
  workerSpawnListeners.add(listener);
  return () => { workerSpawnListeners.delete(listener); };
}

export function useWorkerSpawnPending(runId: string): boolean {
  return useSyncExternalStore(subscribeWorkerSpawns,
    () => pendingWorkerSpawns.has(`${currentServiceOrigin()}/api/runs/${encodeURIComponent(runId)}/workers`),
    () => false);
}

export async function spawnWorker(runId: string, engine?: string): Promise<boolean | null> {
  const path = `/api/runs/${encodeURIComponent(runId)}/workers`;
  const key = `${currentServiceOrigin()}${path}`;
  if (pendingWorkerSpawns.has(key)) return null;
  pendingWorkerSpawns.add(key);
  workerSpawnListeners.forEach(listener => listener());
  try {
    const r = await apiFetch(path, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(engine ? { engine } : {}),
    });
    const j = await r.json().catch(() => ({}));
    return r.ok && j.ok === true;
  } catch {
    return false;
  } finally {
    pendingWorkerSpawns.delete(key);
    workerSpawnListeners.forEach(listener => listener());
  }
}

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

export type ExecutionGuardrailFailPolicy = "fail_closed_ask" | "fail_closed_deny";

export interface ExecutionGuardrailProbeResult {
  ok: boolean;
  healthy: boolean;
  latency_ms: number;
  endpoint: string;
  model_name?: string | null;
  manifest_sha256?: string | null;
  auth_required?: boolean | null;
  allow_cache_reset?: boolean | null;
  python_debug?: boolean | null;
  decision_head_ok: boolean;
  scope_head_ok: boolean;
  decision_head?: unknown;
  scope_head?: unknown;
  error?: string | null;
  probed_at?: string | number | null;
}

export interface ExecutionGuardrailSettings {
  enabled: boolean;
  provider: string;
  endpoint: string;
  api_key_configured: boolean;
  api_key_masked: string | null;
  timeout_ms: number;
  fail_policy: ExecutionGuardrailFailPolicy;
  read_proceed_min: number;
  scope_escalate_min: number;
  reject_deny_min: number;
  last_probe: ExecutionGuardrailProbeResult | null;
}

export interface ExecutionGuardrailUpdatePayload {
  enabled: boolean;
  provider: string;
  endpoint: string;
  api_key?: string;
  clear_api_key?: boolean;
  timeout_ms: number;
  fail_policy: ExecutionGuardrailFailPolicy;
  read_proceed_min: number;
  scope_escalate_min: number;
  reject_deny_min: number;
}

function parseGuardrailSettings(raw: unknown): ExecutionGuardrailSettings {
  const outer = raw && typeof raw === "object" ? raw as Record<string, any> : {};
  const data = outer.settings && typeof outer.settings === "object" ? outer.settings as Record<string, any> : outer;
  return {
    enabled: Boolean(data.enabled),
    provider: typeof data.provider === "string" && data.provider ? data.provider : "cronus",
    endpoint: typeof data.endpoint === "string" ? data.endpoint : "",
    api_key_configured: Boolean(data.api_key_configured),
    api_key_masked: typeof data.api_key_masked === "string" && data.api_key_masked ? data.api_key_masked : null,
    timeout_ms: typeof data.timeout_ms === "number" && Number.isFinite(data.timeout_ms) ? data.timeout_ms : 5000,
    fail_policy: data.fail_policy === "fail_closed_deny" ? "fail_closed_deny" : "fail_closed_ask",
    read_proceed_min: typeof data.read_proceed_min === "number" && Number.isFinite(data.read_proceed_min) ? data.read_proceed_min : 0.9,
    scope_escalate_min: typeof data.scope_escalate_min === "number" && Number.isFinite(data.scope_escalate_min) ? data.scope_escalate_min : 0.2,
    reject_deny_min: typeof data.reject_deny_min === "number" && Number.isFinite(data.reject_deny_min) ? data.reject_deny_min : 0.85,
    last_probe: data.last_probe && typeof data.last_probe === "object" ? data.last_probe as ExecutionGuardrailProbeResult : null,
  };
}

export async function getExecutionGuardrailSettings(signal?: AbortSignal): Promise<ExecutionGuardrailSettings> {
  const response = await apiFetch("/api/settings/execution-guardrail", { cache: "no-store", signal });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || body?.ok === false) {
    throw new Error(String(body?.error?.message ?? body?.detail ?? `HTTP ${response.status}`));
  }
  return parseGuardrailSettings(body);
}

export async function updateExecutionGuardrailSettings(
  payload: ExecutionGuardrailUpdatePayload,
): Promise<ExecutionGuardrailSettings> {
  const response = await apiFetch("/api/settings/execution-guardrail", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(20_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || body?.ok === false) {
    throw new Error(String(body?.error?.message ?? body?.detail ?? `HTTP ${response.status}`));
  }
  return parseGuardrailSettings(body);
}

export async function probeExecutionGuardrail(
  payload?: Partial<ExecutionGuardrailUpdatePayload>,
): Promise<ExecutionGuardrailProbeResult> {
  const response = await apiFetch("/api/settings/execution-guardrail/probe", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload ?? {}),
    signal: AbortSignal.timeout(30_000),
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok && typeof body?.healthy !== "boolean") {
    throw new Error(String(body?.error?.message ?? body?.detail ?? body?.error ?? `HTTP ${response.status}`));
  }
  return {
    ok: Boolean(body?.ok),
    healthy: Boolean(body?.healthy),
    latency_ms: typeof body?.latency_ms === "number" ? body.latency_ms : 0,
    endpoint: typeof body?.endpoint === "string" ? body.endpoint : (payload?.endpoint ?? ""),
    model_name: typeof body?.model_name === "string" ? body.model_name : null,
    manifest_sha256: typeof body?.manifest_sha256 === "string" ? body.manifest_sha256 : null,
    auth_required: typeof body?.auth_required === "boolean" ? body.auth_required : null,
    allow_cache_reset: typeof body?.allow_cache_reset === "boolean" ? body.allow_cache_reset : null,
    python_debug: typeof body?.python_debug === "boolean" ? body.python_debug : null,
    decision_head_ok: Boolean(body?.decision_head_ok),
    scope_head_ok: Boolean(body?.scope_head_ok),
    decision_head: body?.decision_head,
    scope_head: body?.scope_head,
    error: typeof body?.error === "string" ? body.error : null,
    probed_at: body?.probed_at ?? null,
  };
}

export async function fetchRunGuardState(
  runId: string,
  signal?: AbortSignal,
): Promise<ProductionGuardState | null> {
  if (!runId || isDraftRunId(runId)) return null;
  try {
    const response = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/guard/state`, {
      cache: "no-store",
      signal,
    });
    if (!response.ok) return null;
    const body = await response.json().catch(() => null);
    if (!body || body.ok === false) return null;
    return normalizeGuardStateSnapshot(body);
  } catch {
    return null;
  }
}

export async function decideRunGuardApproval(
  runId: string,
  approvalId: string,
  action: GuardApprovalDecisionAction,
  note?: string,
): Promise<Record<string, unknown>> {
  const response = await apiFetch(
    `/api/runs/${encodeURIComponent(runId)}/guard/approvals/${encodeURIComponent(approvalId)}/decide`,
    {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        action,
        ...(note && note.trim() ? { note: note.trim() } : {}),
      }),
    },
  );
  const body = await response.json().catch(() => ({}));
  if (!response.ok || body?.ok === false) {
    throw new Error(String(body?.error?.message ?? body?.detail ?? `HTTP ${response.status}`));
  }
  return body as Record<string, unknown>;
}
