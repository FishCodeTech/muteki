"use client";

import { useEffect, useSyncExternalStore } from "react";
import { apiFetch } from "./serviceAuth";
import { readCacheIdentity } from "./readCache";

export type PolicyModeId = "ctf" | "pentest" | "src";

export interface AgentVersionPolicyConfig {
  enforce: boolean;
  modes: {
    ctf: boolean;
    pentest: boolean;
    src: boolean;
  };
  updated_at: string;
}

export type BackendVersionStatusKind =
  | "supported"
  | "below_minimum"
  | "missing"
  | "unknown"
  | "uncomparable"
  | "not_worker"
  | "image_unavailable"
  | "undeclared"
  | "not_in_image"
  | "error";

export interface EngineBackendVersionStatus {
  available: boolean;
  status: BackendVersionStatusKind;
  binary_path?: string;
  raw_version?: string;
  installed_version: string;
  meets_minimum: boolean;
  below_minimum: boolean;
  below_minimum_raw: boolean;
  error_code?: string;
  detail: string;
}

export interface AgentEngineVersionRow {
  engine: string;
  cli_engine: string;
  display_name: string;
  worker_capable: boolean;
  supported_modes: PolicyModeId[];
  baseline_version: string;
  min_version: string;
  upgrade_command: string;
  alt_upgrade_command: string;
  upgrade_note: string;
  container_upgrade_note: string;
  local: EngineBackendVersionStatus;
  container: EngineBackendVersionStatus;
}

export interface AgentVersionPolicySnapshot {
  policy: AgentVersionPolicyConfig;
  baseline_versions: Record<string, string>;
  worker_image: {
    image: string;
    pulled: boolean;
    declared: boolean;
  };
  engines: AgentEngineVersionRow[];
  evaluated_at: string;
}

export interface WorkerVersionIssue {
  engine: string;
  displayName: string;
  backend: "local" | "container";
  installedVersion: string;
  minVersion: string;
  baselineVersion: string;
  upgradeCommand: string;
  altUpgradeCommand: string;
  upgradeNote: string;
  containerUpgradeNote: string;
  summary: string;
  detail: string;
}

export type AgentVersionPolicyState = {
  status: "idle" | "loading" | "ready" | "error";
  data: AgentVersionPolicySnapshot | null;
  error: string;
};

type StoreRecord = {
  identity: string;
  state: AgentVersionPolicyState;
  inFlight: Promise<AgentVersionPolicySnapshot> | null;
};

const INITIAL_STATE: AgentVersionPolicyState = {
  status: "idle",
  data: null,
  error: "",
};

let store: StoreRecord = {
  identity: "",
  state: INITIAL_STATE,
  inFlight: null,
};

const listeners = new Set<() => void>();

function notify() {
  for (const listener of listeners) listener();
}

function ensureStoreIdentity(): StoreRecord {
  const identity = readCacheIdentity();
  if (store.identity !== identity) {
    store = {
      identity,
      state: INITIAL_STATE,
      inFlight: null,
    };
  }
  return store;
}

function getSnapshot(): AgentVersionPolicyState {
  return ensureStoreIdentity().state;
}

function getServerSnapshot(): AgentVersionPolicyState {
  return INITIAL_STATE;
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export async function loadAgentVersionPolicy(options?: { fresh?: boolean }): Promise<AgentVersionPolicySnapshot> {
  const current = ensureStoreIdentity();
  const fresh = Boolean(options?.fresh);
  if (!fresh && current.inFlight) return current.inFlight;
  if (!fresh && current.state.status === "ready" && current.state.data) {
    return current.state.data;
  }

  const identity = current.identity;
  store = {
    ...current,
    state: {
      status: current.state.data ? "ready" : "loading",
      data: current.state.data,
      error: "",
    },
  };
  notify();

  const promise = (async () => {
    const query = fresh ? "?fresh=1" : "";
    const response = await apiFetch(`/api/settings/agent-version-policy${query}`);
    const payload = await response.json().catch(() => null);
    if (!response.ok || !payload || typeof payload !== "object" || !Array.isArray(payload.engines)) {
      const detail = payload?.detail || payload?.error?.message || `读取 Agent 最低版本设置失败（HTTP ${response.status}）`;
      throw new Error(detail);
    }
    const snapshot = payload as AgentVersionPolicySnapshot;
    if (ensureStoreIdentity().identity === identity) {
      store = {
        identity,
        state: { status: "ready", data: snapshot, error: "" },
        inFlight: null,
      };
      notify();
    }
    return snapshot;
  })().catch((err) => {
    const message = err instanceof Error ? err.message : String(err);
    if (ensureStoreIdentity().identity === identity) {
      store = {
        identity,
        state: {
          status: store.state.data ? "ready" : "error",
          data: store.state.data,
          error: message,
        },
        inFlight: null,
      };
      notify();
    }
    throw err;
  });

  if (ensureStoreIdentity().identity === identity) {
    store.inFlight = promise;
  }
  return promise;
}

export interface AgentVersionPolicyPatch {
  enforce?: boolean;
  modes?: Partial<Record<PolicyModeId, boolean>>;
  reset_to_baseline?: boolean;
}

export async function saveAgentVersionPolicy(patch: AgentVersionPolicyPatch): Promise<AgentVersionPolicySnapshot> {
  const identity = ensureStoreIdentity().identity;
  const response = await apiFetch("/api/settings/agent-version-policy", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
  const payload = await response.json().catch(() => null);
  if (!response.ok || !payload || typeof payload !== "object" || !Array.isArray(payload.engines)) {
    const detail = payload?.detail || payload?.error?.message || `保存 Agent 最低版本设置失败（HTTP ${response.status}）`;
    throw new Error(detail);
  }
  const snapshot = payload as AgentVersionPolicySnapshot;
  if (ensureStoreIdentity().identity === identity) {
    store = {
      identity,
      state: { status: "ready", data: snapshot, error: "" },
      inFlight: null,
    };
    notify();
  }
  return snapshot;
}

export function useAgentVersionPolicy(): AgentVersionPolicyState {
  const state = useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);
  useEffect(() => {
    if (state.status === "idle") {
      void loadAgentVersionPolicy().catch(() => undefined);
    }
  }, [state.status]);
  return state;
}

export function normalizeEngineKey(raw: string): string {
  const trimmed = String(raw || "").trim().toLowerCase();
  if (!trimmed) return "";
  if (trimmed.startsWith("cli-")) return trimmed.slice(4);
  if (trimmed.startsWith("cli.")) return trimmed.slice(4);
  if (trimmed.includes(".")) return trimmed.split(".")[0] || trimmed;
  return trimmed;
}

export function engineVersionRow(
  snapshot: AgentVersionPolicySnapshot | null | undefined,
  engine: string,
): AgentEngineVersionRow | null {
  if (!snapshot) return null;
  const norm = normalizeEngineKey(engine);
  return snapshot.engines.find((row) => row.engine === norm) ?? null;
}

export function isPolicyEnforcedForMode(
  snapshot: AgentVersionPolicySnapshot | null | undefined,
  mode?: PolicyModeId,
): boolean {
  const policy = snapshot?.policy;
  if (!policy || !policy.enforce) return false;
  if (!mode) return policy.modes.ctf || policy.modes.pentest || policy.modes.src;
  return Boolean(policy.modes[mode]);
}

function parseSemver(raw: string): { core: number[]; pre: [number, number] | null } | null {
  const v = raw.trim().toLowerCase().replace(/^v/, "");
  if (!v) return null;
  let splitAt = v.length;
  for (let i = 0; i < v.length; i += 1) {
    const ch = v[i]!;
    if (!((ch >= "0" && ch <= "9") || ch === ".")) {
      splitAt = i;
      break;
    }
  }
  const coreStr = v.slice(0, splitAt).replace(/\.+$/, "");
  const suffix = v.slice(splitAt).replace(/^[-+._]+/, "");
  const parts = coreStr.split(".");
  if (parts.length < 2 || parts.length > 4) return null;
  const nums: number[] = [];
  for (const p of parts) {
    if (!/^\d+$/.test(p)) return null;
    nums.push(Number(p));
  }
  while (nums.length < 4) nums.push(0);
  if (!suffix) return { core: nums, pre: [4, 0] };
  const tagMatch = suffix.match(/^([a-z]+)[.-]?(\d*)$/);
  if (!tagMatch) return { core: nums, pre: null };
  const rankMap: Record<string, number> = { dev: 0, a: 1, alpha: 1, b: 2, beta: 2, rc: 3 };
  const rank = rankMap[tagMatch[1] || ""];
  if (rank === undefined) return { core: nums, pre: null };
  const n = tagMatch[2] ? Number(tagMatch[2]) : 0;
  return { core: nums, pre: [rank, n] };
}

function parseCursorDate(raw: string): [number, number, number] | null {
  const m = raw.trim().match(/^v?(\d{4})\.(\d{2})\.(\d{2})(?:-[0-9a-z]+)?$/i);
  if (!m) return null;
  const year = Number(m[1]);
  const month = Number(m[2]);
  const day = Number(m[3]);
  if (month < 1 || month > 12 || day < 1 || day > 31) return null;
  return [year, month, day];
}

export function compareEngineVersion(engine: string, installed: string, target: string): -1 | 0 | 1 | null {
  const leftRaw = installed.trim().toLowerCase().replace(/^v/, "");
  const rightRaw = target.trim().toLowerCase().replace(/^v/, "");
  if (!leftRaw || !rightRaw) return null;
  if (leftRaw === rightRaw) return 0;

  const leftSem = parseSemver(leftRaw);
  const rightSem = parseSemver(rightRaw);
  if (leftSem && rightSem) {
    for (let i = 0; i < 4; i += 1) {
      if (leftSem.core[i]! < rightSem.core[i]!) return -1;
      if (leftSem.core[i]! > rightSem.core[i]!) return 1;
    }
    if (leftSem.pre && rightSem.pre) {
      if (leftSem.pre[0] !== rightSem.pre[0]) return leftSem.pre[0] < rightSem.pre[0] ? -1 : 1;
      if (leftSem.pre[1] !== rightSem.pre[1]) return leftSem.pre[1] < rightSem.pre[1] ? -1 : 1;
      return 0;
    }
  }

  if (normalizeEngineKey(engine) === "cursor") {
    const ld = parseCursorDate(leftRaw);
    const rd = parseCursorDate(rightRaw);
    if (ld && rd) {
      for (let i = 0; i < 3; i += 1) {
        if (ld[i]! < rd[i]!) return -1;
        if (ld[i]! > rd[i]!) return 1;
      }
      return 0;
    }
  }
  return null;
}

export function workerVersionIssue(
  snapshot: AgentVersionPolicySnapshot | null | undefined,
  engine: string,
  backend: "local" | "container",
  mode?: PolicyModeId,
  nodeId = "local",
): WorkerVersionIssue | null {
  if (!snapshot || !isPolicyEnforcedForMode(snapshot, mode)) return null;
  if (backend === "container" && nodeId && nodeId !== "local") return null;
  const row = engineVersionRow(snapshot, engine);
  if (!row) return null;
  const target = backend === "container" ? row.container : row.local;
  if (!target.below_minimum_raw) return null;
  const installed = target.installed_version || "未知";
  const envLabel = backend === "container" ? "容器镜像内" : "本机";
  const summary = `${row.display_name} 当前${envLabel}版本 ${installed} 低于最低支持版本 ${row.min_version}，禁止设置为出战 Worker`;
  const detail = backend === "container"
    ? `${summary}。${row.container_upgrade_note || "请更新或重建 muteki-worker 镜像，或在运行环境中切换为本地运行。"}`
    : `${summary}。建议升级：在终端执行 \`${row.upgrade_command}\` 后重新检测。`;
  return {
    engine: row.engine,
    displayName: row.display_name,
    backend,
    installedVersion: installed,
    minVersion: row.min_version,
    baselineVersion: row.baseline_version,
    upgradeCommand: row.upgrade_command,
    altUpgradeCommand: row.alt_upgrade_command,
    upgradeNote: row.upgrade_note,
    containerUpgradeNote: row.container_upgrade_note,
    summary,
    detail,
  };
}
