"use client";

import { useSyncExternalStore } from "react";
import { fetchWorkbenchSnapshot, saveWorkbenchSlice, SrcApiError } from "@/lib/srcApi";
import { readStudioState, restoreStudioState, studio, subscribeStudio, type PersistedState, type StudioState } from "./store";
import { isBackendExcludedField, sliceValueForBackend } from "./real/owned";


const DB_NAME = "muteki-studio-prototype";
const DB_VERSION = 1;
const STORE = "slices";
const META_KEY = "meta";
const SCHEMA = 1;
const CHANNEL = "muteki-studio-prototype";
const SAVE_DELAY_MS = 500;
const READ_TIMEOUT_MS = 10_000;

export type SliceKey =
  | "projects"
  | "sources"
  | "clues"
  | "tasks"
  | "dossier"
  | "delivery"
  | "governance"
  | "threads"
  | "chat"
  | "imports"
  | "records"
  | "ui";

export const SLICE_LABELS: Record<Exclude<SliceKey, "ui">, string> = {
  projects: "项目与目标",
  sources: "资产仓库",
  clues: "发现档案",
  tasks: "跟进任务",
  dossier: "长期计划、范围材料与工作记录",
  delivery: "报告交付、证据、反馈与复核",
  governance: "持续关注、变化、权限与连接器",
  threads: "对话消息流",
  chat: "对话引用与 AI 建议",
  imports: "导入批次与来源",
  records: "归并、筛选与处理记录",
};

const SLICES: Record<SliceKey, Array<keyof PersistedState>> = {
  projects: ["projects"],
  sources: ["sources"],
  clues: ["clues"],
  tasks: ["tasks"],
  dossier: ["planVersions", "planQuestions", "scopeMaterials", "workRuns"],
  delivery: ["evidences", "reports", "feedbacks", "retests"],
  governance: ["changes", "monitors", "scopeSnapshots", "approvals", "grants", "environmentStatus", "connectors", "runtimeMode", "serverSync"],
  threads: ["threads", "threadOrder"],
  chat: ["context", "findings", "turnFindings", "turnRefs", "suggestions", "suggestionScopes", "suggestionEmpty", "turnTasks", "handoff", "taskRuns"],
  imports: ["imports", "importOrigins"],
  records: ["merges", "history", "savedFilters", "dismissedDuplicates", "undeterminedDuplicates"],
  ui: ["workspace", "activeThreadId"],
};
const SLICE_KEYS = Object.keys(SLICES) as SliceKey[];
export const CONTENT_SLICE_KEYS = SLICE_KEYS.filter((key): key is Exclude<SliceKey, "ui"> => key !== "ui");

interface Meta {
  schema: number;
  rev: number;
  savedAt: string;
  tab: string;
}

export type SavePhase =
  | "loading"
  | "saved"
  | "pending"
  | "saving"
  | "error"
  | "conflict"
  | "readError"
  | "unavailable";

export interface SaveStatus {
  phase: SavePhase;
  savedAt: string;
  message: string;
  restored: boolean;
  unsaved: boolean;
}

const INITIAL_STATUS: SaveStatus = { phase: "loading", savedAt: "", message: "", restored: false, unsaved: false };

let status: SaveStatus = INITIAL_STATUS;
const statusListeners = new Set<() => void>();

function setStatus(patch: Partial<SaveStatus>) {
  status = { ...status, ...patch };
  statusListeners.forEach((listener) => listener());
}

export function useSaveStatus(): SaveStatus {
  return useSyncExternalStore(
    (listener) => { statusListeners.add(listener); return () => { statusListeners.delete(listener); }; },
    () => status,
    () => INITIAL_STATUS,
  );
}


const TAB_ID = typeof window === "undefined" ? "server" : Math.random().toString(36).slice(2, 10);

function request<T>(req: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

let opening: Promise<IDBDatabase> | null = null;

function db(): Promise<IDBDatabase> {
  if (!opening) {
    opening = new Promise<IDBDatabase>((resolve, reject) => {
      const req = window.indexedDB.open(DB_NAME, DB_VERSION);
      req.onupgradeneeded = () => {
        if (!req.result.objectStoreNames.contains(STORE)) req.result.createObjectStore(STORE);
      };
      req.onsuccess = () => {
        const connection = req.result;
        connection.onversionchange = () => { connection.close(); opening = null; };
        resolve(connection);
      };
      req.onerror = () => reject(req.error);
      req.onblocked = () => reject(new DOMException("浏览器存储被其他页面占用。", "InvalidStateError"));
    });
    opening.catch(() => { opening = null; });
  }
  return opening;
}

async function readAll(): Promise<{ meta: Meta | undefined; data: Partial<PersistedState> }> {
  const tx = (await db()).transaction(STORE, "readonly");
  const store = tx.objectStore(STORE);
  const [meta, ...slices] = await Promise.all([
    request(store.get(META_KEY)) as Promise<Meta | undefined>,
    ...SLICE_KEYS.map((key) => request(store.get(key)) as Promise<Partial<PersistedState> | undefined>),
  ]);
  const data: Partial<PersistedState> = {};
  slices.forEach((slice) => { if (slice && typeof slice === "object") Object.assign(data, slice); });
  return { meta, data };
}

type WriteResult = { ok: true; meta: Meta | null } | { ok: false };

async function write(slices: Partial<Record<SliceKey, object>>, expectedRev: number, force: boolean): Promise<WriteResult> {
  const keys = Object.keys(slices) as SliceKey[];
  const content = keys.some((key) => key !== "ui");
  const tx = (await db()).transaction(STORE, "readwrite");
  const store = tx.objectStore(STORE);
  return new Promise<WriteResult>((resolve, reject) => {
    let result: WriteResult | null = null;
    let failure: unknown = null;
    const putAll = (meta: Meta | null) => {
      try {
        keys.forEach((key) => store.put(slices[key], key));
        if (meta) store.put(meta, META_KEY);
        result = { ok: true, meta };
      } catch (error) {
        failure = error;
        tx.abort();
      }
    };
    if (content) {
      const metaReq = store.get(META_KEY);
      metaReq.onsuccess = () => {
        const stored = metaReq.result as Meta | undefined;
        if (!force && (stored?.rev ?? 0) !== expectedRev) {
          result = { ok: false };
          tx.abort();
          return;
        }
        putAll({ schema: SCHEMA, rev: Math.max(stored?.rev ?? 0, expectedRev) + 1, savedAt: new Date().toISOString(), tab: TAB_ID });
      };
    } else {
      putAll(null);
    }
    tx.oncomplete = () => resolve(result ?? { ok: false });
    tx.onabort = () => {
      if (result && !result.ok) resolve(result);
      else reject(failure ?? tx.error ?? new DOMException("写入被浏览器中止。", "AbortError"));
    };
  });
}


interface PersistenceDriver {
  id: string;
  kind: "idb" | "backend";
  readAll(): Promise<{ meta: Meta | undefined; data: Partial<PersistedState> }>;
  write(slices: Partial<Record<SliceKey, object>>, expectedRev: number, force: boolean): Promise<WriteResult>;
  inspectRemote(): Promise<{ savedAt: string; remoteData: Partial<PersistedState> }>;
}

const idbDriver: PersistenceDriver = {
  id: "idb",
  kind: "idb",
  readAll,
  write,
  inspectRemote: async () => {
    const saved = await readAll();
    return { savedAt: saved.meta?.savedAt ?? "", remoteData: saved.data };
  },
};

function backendDriver(taskId: string): PersistenceDriver {
  const sliceRevisions = new Map<SliceKey, number>();
  const savedSliceJson = new Map<SliceKey, string>();
  const uiKey = `ui@${taskId}`;
  const hasIdb = () => typeof window !== "undefined" && "indexedDB" in window && Boolean(window.indexedDB);

  const readUi = async (): Promise<Partial<PersistedState>> => {
    if (!hasIdb()) return {};
    try {
      const tx = (await db()).transaction(STORE, "readonly");
      const row = await request(tx.objectStore(STORE).get(uiKey)) as Partial<PersistedState> | undefined;
      return row && typeof row === "object" ? row : {};
    } catch {
      return {};
    }
  };
  const writeUi = async (data: object) => {
    if (!hasIdb()) return;
    try {
      const tx = (await db()).transaction(STORE, "readwrite");
      tx.objectStore(STORE).put(data, uiKey);
    } catch {
      // UI preferences cache in IndexedDB is optional in backend mode
    }
  };
  const applySnapshot = async (withUi: boolean, syncRevisions: boolean) => {
    const snapshot = await fetchWorkbenchSnapshot(taskId);
    const data: Partial<PersistedState> = {};
    let rev = 0;
    let savedAt = "";
    Object.entries(snapshot.slices ?? {}).forEach(([key, meta]) => {
      if (syncRevisions) {
        sliceRevisions.set(key as SliceKey, meta.revision);
      }
      rev += meta.revision;
      if (meta.updated_at > savedAt) savedAt = meta.updated_at;
      if (meta.data && typeof meta.data === "object") {
        const sliceKey = key as SliceKey;
        const filtered = sliceValueForBackend(SLICES[sliceKey] ?? [], meta.data as Record<keyof StudioState, unknown>);
        if (syncRevisions && sliceKey !== "ui" && Object.keys(filtered).length > 0) {
          try {
            savedSliceJson.set(sliceKey, JSON.stringify(filtered));
          } catch {
          }
        }
        Object.assign(data, filtered);
      }
    });
    if (withUi) Object.assign(data, await readUi());
    return { data, rev, savedAt };
  };

  return {
    id: `backend:${taskId}`,
    kind: "backend",
    async readAll() {
      const { data, rev, savedAt } = await applySnapshot(true, true);
      if (!rev) return { meta: undefined, data };
      return { meta: { schema: SCHEMA, rev, savedAt, tab: "server" }, data };
    },
    async write(slices, _expectedRev, force) {
      const keys = Object.keys(slices) as SliceKey[];
      const content = keys.filter((key) => key !== "ui");
      if (slices.ui) await writeUi(slices.ui);
      if (force && content.length > 0) {
        await applySnapshot(false, true);
      }
      let wroteAnyContent = false;
      for (const key of content) {
        const value = sliceValueForBackend(SLICES[key], slices[key] as Record<keyof StudioState, unknown>);
        if (!Object.keys(value).length) continue;
        let serialized = "";
        try {
          serialized = JSON.stringify(value);
        } catch {
          serialized = "";
        }
        if (!force && serialized && savedSliceJson.get(key) === serialized) {
          continue;
        }
        const base = sliceRevisions.get(key) ?? 0;
        try {
          const receipt = await saveWorkbenchSlice(taskId, key, value, base, TAB_ID);
          sliceRevisions.set(key, receipt.output.result.revision);
          if (serialized) savedSliceJson.set(key, serialized);
          wroteAnyContent = true;
        } catch (error) {
          if (error instanceof SrcApiError && error.code === "version_conflict") {
            return { ok: false };
          }
          throw error;
        }
      }
      const savedAt = new Date().toISOString();
      return { ok: true, meta: wroteAnyContent ? { schema: SCHEMA, rev: [...sliceRevisions.values()].reduce((a, b) => a + b, 0), savedAt, tab: "server" } : null };
    },
    async inspectRemote() {
      const { data, savedAt } = await applySnapshot(false, false);
      return { savedAt, remoteData: data };
    },
  };
}

let driver: PersistenceDriver = idbDriver;

export function isBackendPersistence(): boolean {
  return driver.kind === "backend";
}

function describeError(error: unknown, action: "读取" | "保存" | "加载演示数据"): string {
  if (error instanceof SrcApiError && error.code === "src.task_paused") {
    return "任务已暂停，请先恢复后再执行该操作。";
  }
  const name = error instanceof DOMException || error instanceof Error ? error.name : "";
  const message = error instanceof Error ? error.message : "";
  if (driver.kind !== "backend") {
    if (name === "QuotaExceededError") return "浏览器给这个页面的存储空间已经用完。请在浏览器设置里清理网站数据，腾出空间后重试。";
    if (name === "DataCloneError") return "有内容无法写入浏览器存储。";
    if (name === "SecurityError") return "浏览器不允许这个页面使用本地存储（例如无痕模式，或禁用了网站数据）。";
    if (name === "VersionError") return "保存的数据来自更新版本的工作台。";
    if (name === "InvalidStateError") return `浏览器存储暂时不可用，${action}没有完成。刷新页面后重试。`;
  }
  return message ? `${action}时出错：${message}` : `${action}时出现未知错误。`;
}


let started = false;
let listenersAttached = false;
let persistenceEpoch = 0;
let loadedOnce = false;
let applying = false;
let baseline: Partial<Record<keyof PersistedState, unknown>> = {};
let knownRev = 0;
let timer: number | undefined;
let writing = false;
let writeAgain = false;
let activeWrite: Promise<{ ok: boolean; error?: string }> | null = null;
let channel: BroadcastChannel | null = null;

function fieldsOf(current: StudioState): Partial<Record<keyof PersistedState, unknown>> {
  const out: Partial<Record<keyof PersistedState, unknown>> = {};
  SLICE_KEYS.forEach((key) => SLICES[key].forEach((field) => { out[field] = current[field]; }));
  return out;
}

function sliceValue(current: StudioState, key: SliceKey): object {
  const out: Record<string, unknown> = {};
  SLICES[key].forEach((field) => { out[field] = current[field]; });
  return out;
}

function serializedComparableSlice(source: Record<string, unknown>, key: SliceKey, backend: boolean): string {
  const fields = SLICES[key];
  const filtered: Record<string, unknown> = {};
  for (const field of fields) {
    if (backend && key !== "ui" && isBackendExcludedField(field)) continue;
    filtered[field] = source[field];
  }
  if (!Object.keys(filtered).length) return "";
  try {
    return JSON.stringify(filtered);
  } catch {
    return String(Math.random());
  }
}

function dirtySlicesFor(
  current: StudioState,
  base: Partial<Record<keyof PersistedState, unknown>>,
  backend: boolean,
): SliceKey[] {
  return SLICE_KEYS.filter((key) => {
    const activeFields = SLICES[key].filter((field) => !(backend && key !== "ui" && isBackendExcludedField(field)));
    if (!activeFields.length) return false;
    if (activeFields.every((field) => current[field] === base[field])) return false;
    const curJson = serializedComparableSlice(current as unknown as Record<string, unknown>, key, backend);
    const baseJson = serializedComparableSlice(base as unknown as Record<string, unknown>, key, backend);
    return curJson !== baseJson;
  });
}

function dirtySlices(current: StudioState): SliceKey[] {
  return dirtySlicesFor(current, baseline, driver.kind === "backend");
}

function hasContentChanges(current: StudioState): boolean {
  return dirtySlices(current).some((key) => key !== "ui");
}

function autosavePaused(): boolean {
  return status.phase === "loading" || status.phase === "readError" || status.phase === "unavailable" || status.phase === "conflict" || status.phase === "error";
}

function onStateChange() {
  if (applying) return;
  const current = readStudioState();
  if (autosavePaused()) {
    if (status.phase !== "loading" && !status.unsaved && hasContentChanges(current)) setStatus({ unsaved: true });
    return;
  }
  const dirty = dirtySlices(current);
  if (!dirty.length) return;
  if (dirty.some((key) => key !== "ui") && status.phase === "saved") setStatus({ phase: "pending" });
  schedule();
}

function schedule() {
  window.clearTimeout(timer);
  timer = window.setTimeout(() => { timer = undefined; void save(false, false); }, SAVE_DELAY_MS);
}

async function save(force: boolean, all: boolean): Promise<{ ok: boolean; error?: string }> {
  if (writing) {
    if (force && activeWrite) {
      await activeWrite;
    } else {
      writeAgain = true;
      return activeWrite ?? { ok: true };
    }
  }
  const epoch = persistenceEpoch;
  const targetDriver = driver;
  const current = readStudioState();
  const keys = all ? SLICE_KEYS : dirtySlices(current);
  if (!keys.length) {
    if (epoch === persistenceEpoch && status.phase === "pending") setStatus({ phase: "saved" });
    return { ok: true };
  }
  const content = keys.some((key) => key !== "ui");
  writing = true;
  if (content && epoch === persistenceEpoch) setStatus({ phase: "saving", message: "" });
  const snapshot = fieldsOf(current);
  const slices: Partial<Record<SliceKey, object>> = {};
  keys.forEach((key) => { slices[key] = sliceValue(current, key); });
  const run = (async (): Promise<{ ok: boolean; error?: string }> => {
    try {
      const result = await targetDriver.write(slices, knownRev, force);
      if (epoch !== persistenceEpoch) {
        return { ok: result.ok };
      }
      if (!result.ok) {
        const msg = "另一个窗口或标签页刚保存了新的改动，这个页面还有没保存的改动。可逐模块选择合并或整份保留。";
        setStatus({ phase: "conflict", message: msg, unsaved: true });
        return { ok: false, error: msg };
      }
      keys.forEach((key) => SLICES[key].forEach((field) => { baseline[field] = snapshot[field]; }));
      if (result.meta) {
        knownRev = result.meta.rev;
        channel?.postMessage({ type: "saved", rev: knownRev, tab: TAB_ID });
      }
      const pendingContent = hasContentChanges(readStudioState());
      setStatus({
        phase: pendingContent ? "pending" : status.phase === "saving" || status.phase === "pending" || force ? "saved" : status.phase,
        savedAt: result.meta?.savedAt ?? status.savedAt,
        message: "",
        unsaved: false,
      });
      return { ok: true };
    } catch (error) {
      const msg = describeError(error, "保存");
      if (epoch === persistenceEpoch) {
        setStatus({ phase: "error", message: msg, unsaved: true });
      }
      return { ok: false, error: msg };
    } finally {
      if (epoch === persistenceEpoch) {
        writing = false;
        activeWrite = null;
        const more = writeAgain || dirtySlices(readStudioState()).length > 0;
        writeAgain = false;
        if (more && !autosavePaused()) schedule();
      } else {
        writing = false;
        activeWrite = null;
        writeAgain = false;
      }
    }
  })();
  activeWrite = run;
  return run;
}

async function load(initial: boolean): Promise<void> {
  const epoch = persistenceEpoch;
  if (driver.kind !== "backend" && (typeof window === "undefined" || !("indexedDB" in window) || !window.indexedDB)) {
    setStatus({ phase: "unavailable", message: "这个浏览器不支持本地存储，刷新或关闭页面后改动会丢失。" });
    return;
  }
  let saved: { meta: Meta | undefined; data: Partial<PersistedState> };
  try {
    saved = await Promise.race([
      driver.readAll(),
      new Promise<never>((_, reject) => {
        window.setTimeout(() => reject(new DOMException("读取超时：存储长时间没有响应。", "TimeoutError")), READ_TIMEOUT_MS);
      }),
    ]);
  } catch (error) {
    if (epoch !== persistenceEpoch) return;
    const name = error instanceof DOMException ? error.name : "";
    if (!loadedOnce && (name === "SecurityError" || name === "InvalidAccessError")) {
      setStatus({ phase: "unavailable", message: `${describeError(error, "读取")}刷新或关闭页面后改动会丢失。` });
    } else {
      setStatus({ phase: "readError", message: `${describeError(error, "读取")}为了不覆盖已保存的内容，自动保存已暂停。` });
    }
    return;
  }
  if (epoch !== persistenceEpoch) return;
  if (saved.meta && saved.meta.schema > SCHEMA) {
    setStatus({ phase: "readError", message: "保存的数据来自更新版本的工作台，这个版本无法读取。为了不覆盖它，自动保存已暂停。" });
    return;
  }
  if (saved.meta) {
    const data = initial ? saved.data : { ...saved.data, workspace: undefined, activeThreadId: undefined };
    applying = true;
    try {
      restoreStudioState(data, { initial });
    } catch (error) {
      if (epoch === persistenceEpoch) {
        setStatus({ phase: "readError", message: `${error instanceof Error ? error.message : "保存的数据无法识别。"}为了不覆盖它，自动保存已暂停。` });
      }
      return;
    } finally {
      applying = false;
    }
  }
  if (epoch !== persistenceEpoch) return;
  loadedOnce = true;
  knownRev = saved.meta?.rev ?? 0;
  baseline = fieldsOf(readStudioState());
  setStatus({
    phase: "saved",
    savedAt: saved.meta?.savedAt ?? "",
    message: "",
    unsaved: false,
    restored: initial ? Boolean(saved.meta) : status.restored,
  });
  if (dirtySlices(readStudioState()).length) schedule();
}

function onMessage(event: MessageEvent) {
  const data = event.data as { type?: string; rev?: number; tab?: string } | null;
  if (!data || data.type !== "saved" || data.tab === TAB_ID || (data.rev ?? 0) <= knownRev) return;
  if (status.phase === "loading" || status.phase === "readError" || status.phase === "unavailable" || status.phase === "conflict") return;
  if (writing || hasContentChanges(readStudioState())) {
    setStatus({ phase: "conflict", message: "另一个窗口或标签页保存了新的改动，这个页面也有还没保存的改动。可逐字段/分片合并或二选一。", unsaved: true });
    return;
  }
  void load(false);
}

function flushCaptured(prevDriver: PersistenceDriver, prevState: StudioState, prevBase: Partial<Record<keyof PersistedState, unknown>>, prevRev: number) {
  window.clearTimeout(timer);
  timer = undefined;
  const dirty = dirtySlicesFor(prevState, prevBase, prevDriver.kind === "backend");
  if (!dirty.length) return;
  const slices: Partial<Record<SliceKey, object>> = {};
  dirty.forEach((key) => { slices[key] = sliceValue(prevState, key); });
  void prevDriver.write(slices, prevRev, false).catch(() => {});
}

function flush() {
  if (timer === undefined) return;
  window.clearTimeout(timer);
  timer = undefined;
  void save(false, false);
}

function restart(next: PersistenceDriver, initNextState?: () => void) {
  if (started && driver.id === next.id) {
    initNextState?.();
    return;
  }
  persistenceEpoch += 1;
  const prevDriver = driver;
  const prevState = readStudioState();
  const prevBase = baseline;
  const prevRev = knownRev;
  const switching = started && driver.id !== next.id;
  if (started) {
    if (initNextState) flushCaptured(prevDriver, prevState, prevBase, prevRev);
    else flush();
  }
  initNextState?.();
  driver = next;
  if (!switching) return;
  channel?.close();
  channel = null;
  loadedOnce = false;
  baseline = {};
  knownRev = 0;
  writing = false;
  writeAgain = false;
  activeWrite = null;
  setStatus({ ...INITIAL_STATUS });
  void load(true);
}

function attachWindowListeners() {
  if (listenersAttached || typeof window === "undefined") return;
  listenersAttached = true;
  subscribeStudio(onStateChange);
  document.addEventListener("visibilitychange", () => { if (document.visibilityState === "hidden") flush(); });
  window.addEventListener("pagehide", flush);
  window.addEventListener("beforeunload", (event) => {
    flush();
    const phase = status.phase;
    if (phase !== "pending" && phase !== "saving" && !status.unsaved) return;
    event.preventDefault();
    event.returnValue = "";
  });
}

export function startStudioPersistence() {
  if (typeof window === "undefined") return;
  if (started && driver.id === idbDriver.id && readStudioState().hostTag === "demo") return;
  const epoch = ++persistenceEpoch;
  studio.beginDemoInit();
  if (started && driver.id !== idbDriver.id) {
    flush();
    driver = idbDriver;
    channel?.close();
    channel = null;
    loadedOnce = false;
    baseline = {};
    knownRev = 0;
    setStatus({ ...INITIAL_STATUS });
  } else if (!started) {
    driver = idbDriver;
    started = true;
    attachWindowListeners();
  }
  void studio.ensureDemo().then(() => {
    if (persistenceEpoch !== epoch) return;
    if (!channel && typeof BroadcastChannel !== "undefined") {
      channel = new BroadcastChannel(CHANNEL);
      channel.onmessage = onMessage;
    }
    void load(true);
  }).catch((error: unknown) => {
    if (persistenceEpoch !== epoch) return;
    setStatus({ phase: "readError", message: `${describeError(error, "加载演示数据")}为了不覆盖已保存的内容，自动保存已暂停。` });
  });
}

export function saveNow() {
  if (status.phase === "readError" || status.phase === "unavailable" || status.phase === "loading" || status.phase === "conflict") return;
  window.clearTimeout(timer);
  timer = undefined;
  if (status.phase === "error") setStatus({ phase: "pending" });
  void save(false, false);
}

export function retryLoad() {
  void load(!loadedOnce);
}

export function loadLatest() {
  window.clearTimeout(timer);
  timer = undefined;
  void load(false);
}

export function overwriteSaved(): Promise<{ ok: boolean; error?: string }> {
  window.clearTimeout(timer);
  timer = undefined;
  setStatus({ phase: "pending", message: "" });
  return save(true, true);
}


export type ConflictChoice = "local" | "remote" | "union";

export interface ConflictSliceDiff {
  key: Exclude<SliceKey, "ui">;
  label: string;
  localDirty: boolean;
  localSummary: string;
  remoteSummary: string;
}

function summarizeSlice(data: Partial<PersistedState>, key: Exclude<SliceKey, "ui">): string {
  if (key === "projects") return `${data.projects?.length ?? 0} 个项目`;
  if (key === "sources") return `${data.sources?.length ?? 0} 份资产`;
  if (key === "clues") return `${data.clues?.length ?? 0} 条发现`;
  if (key === "tasks") return `${data.tasks?.length ?? 0} 个跟进任务`;
  if (key === "dossier") return `${data.planVersions?.length ?? 0} 版计划 · ${data.workRuns?.length ?? 0} 条工作记录`;
  if (key === "delivery") return `${data.reports?.length ?? 0} 份报告 · ${data.evidences?.length ?? 0} 份证据 · ${data.feedbacks?.length ?? 0} 条反馈`;
  if (key === "governance") return `${data.monitors?.length ?? 0} 个关注 · ${data.changes?.length ?? 0} 条变化 · ${data.approvals?.length ?? 0} 项审批`;
  if (key === "threads") return `${Object.keys(data.threads ?? {}).length} 条对话线程`;
  if (key === "chat") return `${Object.keys(data.suggestions ?? {}).length} 组建议`;
  if (key === "imports") return `${data.imports?.length ?? 0} 个导入批次`;
  if (key === "records") return `${data.merges?.length ?? 0} 次归并 · ${data.history?.length ?? 0} 条记录`;
  return "";
}

function mergeArrayById<T extends { id: string }>(local: T[] = [], remote: T[] = []): T[] {
  const map = new Map<string, T>();
  remote.forEach((item) => map.set(item.id, item));
  local.forEach((item) => map.set(item.id, item));
  return Array.from(map.values());
}

export async function inspectConflictSlices(): Promise<{
  savedAt: string;
  remoteData: Partial<PersistedState>;
  diffs: ConflictSliceDiff[];
}> {
  const local = readStudioState();
  const dirty = new Set(dirtySlices(local));
  const { remoteData: remote, savedAt: remoteSavedAt } = await driver.inspectRemote();
  const diffs: ConflictSliceDiff[] = CONTENT_SLICE_KEYS.map((key) => ({
    key,
    label: SLICE_LABELS[key],
    localDirty: dirty.has(key),
    localSummary: summarizeSlice(local, key),
    remoteSummary: summarizeSlice(remote, key),
  }));
  return {
    savedAt: remoteSavedAt,
    remoteData: remote,
    diffs,
  };
}

export async function resolveConflictBySlices(
  choices: Record<Exclude<SliceKey, "ui">, ConflictChoice>,
  remoteData: Partial<PersistedState>,
): Promise<{ ok: boolean; error?: string }> {
  const local = readStudioState();
  const merged: Partial<PersistedState> = {};

  for (const sliceKey of CONTENT_SLICE_KEYS) {
    const choice = choices[sliceKey] ?? "union";
    for (const field of SLICES[sliceKey]) {
      const lVal = local[field];
      const rVal = remoteData[field];
      if (choice === "local" || rVal === undefined) {
        (merged as Record<string, unknown>)[field] = lVal;
      } else if (choice === "remote") {
        (merged as Record<string, unknown>)[field] = rVal;
      } else {
        const hasId = (arr: unknown[]) => arr.length === 0 || (typeof arr[0] === "object" && arr[0] !== null && "id" in arr[0]);
        if (Array.isArray(lVal) && Array.isArray(rVal) && hasId(lVal) && hasId(rVal) && typeof lVal[0] !== "string" && typeof rVal[0] !== "string") {
          (merged as Record<string, unknown>)[field] = mergeArrayById(lVal as unknown as Array<{ id: string }>, rVal as unknown as Array<{ id: string }>);
        } else if (typeof lVal === "object" && lVal !== null && !Array.isArray(lVal) && typeof rVal === "object" && rVal !== null && !Array.isArray(rVal)) {
          (merged as Record<string, unknown>)[field] = { ...(rVal as object), ...(lVal as object) };
        } else {
          (merged as Record<string, unknown>)[field] = lVal;
        }
      }
    }
  }

  applying = true;
  try {
    restoreStudioState(merged, { initial: false });
  } finally {
    applying = false;
  }
  return overwriteSaved();
}


const SERVER_STORAGE_KEY = "muteki-studio-simulated-cloud-v1";

export function switchStudioPersistenceTask(taskId: string, initNextState: () => void) {
  restart(backendDriver(taskId), initNextState);
  if (started || typeof window === "undefined") return;
  started = true;
  attachWindowListeners();
  void load(true);
}

export function startStudioPersistenceForTask(taskId: string) {
  switchStudioPersistenceTask(taskId, () => {});
}

export async function syncWithServer(): Promise<{ savedAt: string; conflict: boolean }> {
  if (driver.kind !== "backend") {
    const res = syncWithSimulatedServer();
    return { savedAt: res.savedAt, conflict: false };
  }
  window.clearTimeout(timer);
  timer = undefined;
  if (hasContentChanges(readStudioState())) {
    setStatus({ phase: "conflict", message: "服务器上有新的改动，这个页面也有还没保存的改动。可逐模块合并或整份保留。", unsaved: true });
    return { savedAt: "", conflict: true };
  }
  try {
    await load(false);
    if (status.phase === "readError" || status.phase === "error") {
      throw new Error(status.message || "从服务器同步失败");
    }
    return { savedAt: status.savedAt, conflict: false };
  } catch (error) {
    setStatus({ phase: "error", message: describeError(error, "读取"), unsaved: true });
    throw error;
  }
}

export function syncWithSimulatedServer(): { syncRev: number; savedAt: string } {
  const current = readStudioState();
  const serverSync = {
    ...current.serverSync,
    lastSyncedAt: new Date().toISOString(),
    syncRev: current.serverSync.syncRev + 1,
  };
  const payload = exportWorkspaceArchive({ ...current, serverSync });
  window.localStorage.setItem(SERVER_STORAGE_KEY, payload);
  studio.recordServerSync(serverSync);
  saveNow();
  return { syncRev: serverSync.syncRev, savedAt: serverSync.lastSyncedAt };
}

export function exportWorkspaceArchive(current = readStudioState()): string {
  const data: Partial<Record<keyof PersistedState, unknown>> = {};
  CONTENT_SLICE_KEYS.forEach((key) => {
    SLICES[key].forEach((field) => {
      data[field] = current[field];
    });
  });
  return JSON.stringify(
    {
       archiveFormat: "muteki-src-studio-archive.v1",
      exportedAt: new Date().toISOString(),
      syncRev: current.serverSync.syncRev,
      deviceName: current.serverSync.deviceName,
      data,
    },
    null,
    2,
  );
}

export function importWorkspaceArchive(jsonText: string): { ok: boolean; error?: string; summary?: string } {
  try {
    const parsed = JSON.parse(jsonText) as { data?: Partial<PersistedState>; exportedAt?: string; deviceName?: string };
    const data = parsed.data ?? (parsed as unknown as Partial<PersistedState>);
    if (!data || typeof data !== "object" || (!Array.isArray(data.sources) && !Array.isArray(data.clues))) {
      return { ok: false, error: "文件内容不是有效的工作台跨设备存档 JSON。" };
    }
    applying = true;
    try {
      restoreStudioState(data, { initial: false });
    } finally {
      applying = false;
    }
    overwriteSaved();
    return {
      ok: true,
      summary: `已导入 ${data.sources?.length ?? 0} 份资产、${data.clues?.length ?? 0} 条发现、${data.reports?.length ?? 0} 份报告${parsed.deviceName ? `（来自 ${parsed.deviceName}）` : ""}。`,
    };
  } catch (error) {
    return { ok: false, error: error instanceof Error ? `JSON 解析失败：${error.message}` : "无法解析存档文件。" };
  }
}
