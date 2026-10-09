"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeUiPreferences } from "@/lib/uiPreferences";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@/components/Icon";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { ConversationModelPicker } from "@/components/conversation/ConversationModelPicker";
import {
  Badge, Button, Callout, Checkbox, Collapse, CopyButton, Dialog, EmptyState, IconButton,
  Select, StatusDot, Switch, TextArea, TextField, type ListOption,
} from "@/components/chat/ui";
import { SettingsEmpty, SettingsSection } from "@/components/settings/primitives";
import { UsageProviderSettings } from "@/components/settings/UsageProviderSettings";
import { cn } from "@/lib/cn";
import { type ChatDefaultModel, readChatDefaultModel, writeChatDefaultModel } from "@/lib/conversationDefaults";
import { credentialForRuntime } from "@/lib/modelReasoning";
import {
  adapterDescriptor, descriptorForEngine, loadProviderDescriptors, providerEngines, readyCatalog,
  runtimeScopesModelCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import { readHiddenModels, setModelHidden, subscribeHiddenModels } from "@/lib/modelVisibility";
import { recordTaskReceipt } from "@/lib/task-center";
import {
  allCredentialModels, normalizeConversationCredential,
  type ConversationCredential, type ConversationCredentialModel,
} from "@/lib/useConversation";
import {
  type GlobalCredential, type WorkerModelTestResult, type WorkerSettings, apiFetch, deleteCredentialAccount,
  getGlobalCredentials, getWorkerSettings, importHostCodexAuth, importHostWorkerLogin,
  putCredentialAccount, refreshCredentialModels, testGlobalCredential,
} from "@/lib/useRun";
import { EngineList } from "./EngineList";
import { ModelDirectory, type AgentModelRow } from "./ModelDirectory";
import { EnvRefsEditor, envRefRows, envRefsPayload, type EnvRefRow } from "./EnvRefsEditor";
import {
  CAPABILITY_LABELS, EMPTY_DRAFT, asEngine, catalogSummary, credentialLiveStatus, declaredTransportFields, engineCliAdapter,
  credentialModels, credentialName, credentialReady, formatTestTime, installedVersion, lastTestAsResult,
  readSelection, runtimeLabel, selectionHref, splitModels, statusTone, versionCheckDetail, versionStatus,
  type AgentsSelection, type AgentsSettingsNavigation, type Connection, type CredentialDraft,
  type Engine, type EngineSummary, type Feedback, type ModelRefreshFeedback,
  type RuntimeInstance,
} from "./shared";

const AUTO_REFRESH_MS = 300_000;
const ACCESS_MODE_LABELS: Record<string, string> = {
  supervised: "严格监督", "auto-accept-edits": "自动接受编辑",
  auto: "自动审查", "full-access": "完全访问",
};

const CONNECTION_OPTIONS: ListOption<Connection>[] = [
  { value: "official", label: "官方账号" },
  { value: "custom_endpoint", label: "自定义端点" },
];
const BACKEND_OPTIONS: ListOption<WorkerSettings["worker_backend"]>[] = [
  { value: "local", label: "本地环境测试" },
  { value: "container", label: "容器环境测试" },
];

function CandidateModelsField({
  value,
  refreshing,
  onRefresh,
  catalog,
  feedback,
}: {
  value: string;
  refreshing: boolean;
  onRefresh: () => void;
  catalog?: GlobalCredential["model_catalog"];
  feedback: ModelRefreshFeedback;
}) {
  const [open, setOpen] = useState(false);
  const models = splitModels(value);
  const failed = feedback?.kind === "bad" || (!feedback && catalog?.refresh_status === "failed");
  const statusText = refreshing ? "正在更新候选模型…" : feedback?.detail || catalogSummary(catalog);
  return (
    <div className="rounded-xl border border-cx-border bg-cx-bg-subtle px-4 py-3" aria-busy={refreshing}>
      <div className="flex items-center justify-between gap-3">
        <span className="text-[12.5px] font-medium text-cx-fg-2">候选模型清单</span>
        <Button size="xs" variant="outline" icon="refresh" loading={refreshing} onClick={onRefresh}>
          {refreshing ? "更新中…" : "一键更新"}
        </Button>
      </div>
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
        className="mt-2 inline-flex items-center gap-1.5 rounded-md text-[12.5px] text-cx-fg-3 outline-none transition-colors hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
      >
        <Icon name="chevronDown" size={13} className={cn("transition-transform", open && "rotate-180")} />
        {models.length ? `${models.length} 个候选模型` : "尚无候选模型"}
      </button>
      <Collapse open={open}>
        {models.length ? (
          <ul aria-label="候选模型清单" className="mt-2 flex max-h-44 flex-col gap-1 overflow-y-auto">
            {models.map((model) => <li key={model}><code className="text-[12px] text-cx-fg-2">{model}</code></li>)}
          </ul>
        ) : (
          <p className="mt-2 text-[12.5px] text-cx-fg-3">点击“一键更新”从当前凭据读取模型目录。</p>
        )}
      </Collapse>
      <p
        role={failed ? "alert" : "status"}
        className={cn("mt-2 flex items-start gap-1.5 text-[12px] leading-4", failed ? "text-cx-danger" : feedback?.kind === "ok" ? "text-cx-success" : "text-cx-fg-3")}
      >
        <Icon
          name={refreshing ? "refresh" : feedback?.kind === "ok" ? "checkCircle" : failed ? "alert" : "clock"}
          size={12}
          className={cn("mt-0.5 shrink-0", refreshing && "cx-spin")}
        />
        <span>{statusText}</span>
      </p>
    </div>
  );
}

/** One argument per line; blank lines are ignored and the service validates the rest. */
function launchArgsPayload(text: string): string[] {
  return text.split("\n").map((line) => line.trim()).filter(Boolean);
}

export function AgentsSettings({ taskSettings = false, navigation, onCredentialsChanged }: {
  taskSettings?: boolean;
  navigation?: AgentsSettingsNavigation;
  onCredentialsChanged?: (action: "save" | "import" | "create" | "delete") => void;
} = {}) {
  const [credentials, setCredentials] = useState<GlobalCredential[]>([]);
  const [instances, setInstances] = useState<RuntimeInstance[]>([]);
  const [backend, setBackend] = useState<WorkerSettings["worker_backend"]>("local");
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [lastCheckedAt, setLastCheckedAt] = useState("");
  const [chatDefault, setChatDefault] = useState<ChatDefaultModel | null>(null);
  const [chatDefaultCredentialId, setChatDefaultCredentialId] = useState("");
  const [chatDefaultModelId, setChatDefaultModelId] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [busyItem, setBusyItem] = useState("");
  const [testOpen, setTestOpen] = useState(false);
  const [testResults, setTestResults] = useState<Record<string, WorkerModelTestResult>>({});
  const descriptors = readyCatalog(useProviderDescriptors());
  // An auth-home account layout takes the whole auth.json, not a single secret.
  const authHomeSecret = (engine: Engine) => descriptorForEngine(descriptors, engine)?.credentials.secret_file === "CODEX_AUTH_HOME";
  const hostLoginImport = (engine: Engine) => descriptorForEngine(descriptors, engine)?.login.host_login_import ?? "none";
  const [listQuery, setListQuery] = useState("");
  const [capabilitiesOpen, setCapabilitiesOpen] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const [createEngine, setCreateEngine] = useState<Engine | null>(null);
  const [newDraft, setNewDraft] = useState<CredentialDraft>(EMPTY_DRAFT);
  const [createError, setCreateError] = useState("");
  const [creating, setCreating] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<GlobalCredential | null>(null);
  const [deleteError, setDeleteError] = useState("");
  const [deleting, setDeleting] = useState(false);
  const [editSecret, setEditSecret] = useState("");
  const [editBaseRevision, setEditBaseRevision] = useState<string | undefined>();
  const [showSecret, setShowSecret] = useState(false);
  const [editConnection, setEditConnection] = useState<Connection>("official");
  const [editProvider, setEditProvider] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editDefaultModel, setEditDefaultModel] = useState("");
  const [editModelsText, setEditModelsText] = useState("");
  const [modelRefreshFeedback, setModelRefreshFeedback] = useState<ModelRefreshFeedback>(null);
  const [editBinaryPath, setEditBinaryPath] = useState("");
  const [editLaunchArgs, setEditLaunchArgs] = useState("");
  const [runtimeBaseRevision, setRuntimeBaseRevision] = useState("");
  const [editEndpoint, setEditEndpoint] = useState("");
  const [editTransport, setEditTransport] = useState<Record<string, string | number | boolean>>({});
  const [editEnvRefs, setEditEnvRefs] = useState<EnvRefRow[]>([]);
  const [importAccountId, setImportAccountId] = useState("");
  const [hiddenModels, setHiddenModels] = useState<ReadonlySet<string>>(() => readHiddenModels());
  const feedbackTimer = useRef<number | null>(null);
  const loadGeneration = useRef(0);
  const backendInitialized = useRef(false);
  const refreshInFlight = useRef(false);
  const lastAutomaticRefreshAt = useRef(Date.now());

  useEffect(() => subscribeUiPreferences(() => { const saved = readChatDefaultModel(); setChatDefault(saved); setChatDefaultCredentialId(saved?.credentialId || ""); setChatDefaultModelId(saved?.modelId || ""); }), []);
  useEffect(() => subscribeHiddenModels(() => setHiddenModels(readHiddenModels())), []);

  const showFeedback = useCallback((kind: "ok" | "warn" | "bad", detail: string) => {
    if (feedbackTimer.current) window.clearTimeout(feedbackTimer.current);
    setFeedback({ kind, detail });
    feedbackTimer.current = window.setTimeout(() => setFeedback(null), 6000);
  }, []);
  useEffect(() => () => { if (feedbackTimer.current) window.clearTimeout(feedbackTimer.current); }, []);
  useEffect(() => {
    if (taskSettings) return;
    const old = document.title;
    document.title = "Agents — Project Muteki";
    return () => { document.title = old; };
  }, [taskSettings]);

  const loadData = useCallback(async (silent = false) => {
    const generation = ++loadGeneration.current;
    if (!silent) setLoading(true);
    try {
      const [rows, runtimePayload, settings, descriptorCatalog] = await Promise.all([
        getGlobalCredentials(backend),
        apiFetch(`/api/agent-runtimes?transport=${taskSettings ? "cli" : "structured"}`).then(async (response) => {
          if (!response.ok) throw new Error(`运行环境读取失败（HTTP ${response.status}）`);
          const payload = await response.json();
          if (!Array.isArray(payload.instances)) throw new Error("运行环境目录返回了无效响应");
          return payload;
        }),
        getWorkerSettings().catch(() => null),
        // Fresh: per-instance capabilities change with every probe.
        loadProviderDescriptors({ fresh: true }),
      ]);
      if (generation !== loadGeneration.current) return false;
      const nextCredentials = Array.isArray(rows) ? rows : [];
      setCredentials(nextCredentials);
      setInstances(Array.isArray(runtimePayload.instances) ? runtimePayload.instances : []);
      if (taskSettings && !backendInitialized.current && settings?.worker_backend) { setBackend(settings.worker_backend); backendInitialized.current = true; }
      if (!taskSettings) {
        const saved = readChatDefaultModel();
        const savedCredential = saved && nextCredentials.find((item) => item.id === saved.credentialId && asEngine(descriptorCatalog, item.engine) && credentialReady(item));
        const activeCredentials = nextCredentials.filter((item) => asEngine(descriptorCatalog, item.engine));
        const validSaved = saved;
        setChatDefault(validSaved);
        const ready = savedCredential || activeCredentials.find(credentialReady) || activeCredentials[0] || null;
        const models = ready ? credentialModels(ready) : [];
        setChatDefaultCredentialId(ready?.id || "");
        setChatDefaultModelId(validSaved && ready?.id === validSaved.credentialId && models.includes(validSaved.modelId) ? validSaved.modelId : ready?.default_model || models[0] || "");
      }
      const probedAt = String(runtimePayload.probed_at || "");
      const checkedAt = probedAt ? new Date(probedAt) : null;
      if (checkedAt && !Number.isNaN(checkedAt.getTime())) {
        setLastCheckedAt(checkedAt.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit" }));
      }
      return true;
    } catch (error) { showFeedback("bad", `加载 Agent 数据失败：${error instanceof Error ? error.message : String(error)}`); return false; }
    finally { if (generation === loadGeneration.current) setLoading(false); }
  }, [backend, showFeedback, taskSettings]);
  useEffect(() => { void loadData(); }, [loadData]);

  const engines = useMemo<EngineSummary[]>(() => providerEngines(descriptors).map((engine) => {
    const identity = descriptorForEngine(descriptors, engine)!.identity;
    const label = identity.display_name || engine;
    if (identity.support_status !== "supported") {
      return { engine, label, cliAdapterId: identity.cli_adapter_id, credentials: [], instances: [], primaryCredential: null, primaryInstance: null, enabled: false, version: "", readyCredentialCount: 0, modelCount: 0, environmentCount: 0, statusKind: "disabled", statusText: "暂不支持", supportStatus: "temporarily_disabled", disabledReason: "该引擎暂不支持。" };
    }
    const engineCredentials = credentials.filter((item) => asEngine(descriptors, item.engine) === engine);
    const engineInstances = instances.filter((item) => asEngine(descriptors, item.engine) === engine);
    const primaryCredential = engineCredentials.find((item) => item.source === "stored" && credentialReady(item)) || engineCredentials.find(credentialReady) || engineCredentials[0] || null;
    const primaryInstance = (!taskSettings && engineInstances.find((item) => item.adapter_id === identity.default_adapter_id)) || engineInstances.find((item) => item.configured) || engineInstances.find((item) => item.discovered) || engineInstances[0] || null;
    const enabled = primaryInstance?.enabled ?? engineCredentials.some((item) => item.usage.some((usage) => usage.enabled !== false));
    const readyCredentialCount = engineCredentials.filter(credentialReady).length;
    const modelCount = new Set(engineCredentials.flatMap((item) => {
      const normalized = normalizeConversationCredential(item);
      return normalized && primaryInstance
        ? allCredentialModels(credentialForRuntime(normalized, primaryInstance.key, runtimeScopesModelCatalog(descriptors, primaryInstance.key))).map((model) => model.id)
        : credentialModels(item);
    })).size;
    let statusKind: EngineSummary["statusKind"] = "warn";
    let statusText = "尚无可用凭据";
    if (!enabled) { statusKind = "disabled"; statusText = "已停用"; }
    else if (!primaryInstance?.health) statusText = "等待设备探测";
    else if (primaryInstance?.health && !primaryInstance.health.healthy) { statusKind = "bad"; statusText = primaryInstance.health.detail || "运行环境异常"; }
    else if (readyCredentialCount > 0) { statusKind = "ok"; statusText = "已就绪"; }
    else if (primaryInstance?.auth.status === "ok") { statusKind = "ok"; statusText = "宿主登录可用"; }
    else if (primaryInstance?.auth.status === "missing") statusText = "等待登录或添加凭据";
    return { engine, label, cliAdapterId: identity.cli_adapter_id, credentials: engineCredentials, instances: engineInstances, primaryCredential, primaryInstance, enabled, version: primaryInstance?.health?.runtime_version || "", readyCredentialCount, modelCount, environmentCount: engineInstances.length, statusKind, statusText, supportStatus: "supported", disabledReason: "" };
  }), [credentials, descriptors, instances, taskSettings]);

  /* Selection: URL query (?engine=&instance=&credential=) when the host passes
   * navigation, otherwise plain local state (task settings embed). */
  const [localSelection, setLocalSelection] = useState<AgentsSelection>({ engine: null, instanceId: "", credentialId: "" });
  const selection = useMemo<AgentsSelection>(
    () => navigation ? readSelection(navigation.searchParams, descriptors) : localSelection,
    [descriptors, navigation, localSelection],
  );
  const applySelection = useCallback((next: AgentsSelection, mode: "push" | "replace") => {
    if (navigation) {
      const href = selectionHref(navigation.pathname, navigation.searchParams, next, descriptors);
      navigation.router[mode](href, { scroll: false });
    } else {
      setLocalSelection(next);
    }
  }, [descriptors, navigation]);

  const currentEngine = useMemo(() => {
    if (!selection.engine) return null;
    const found = engines.find((item) => item.engine === selection.engine) || null;
    return found && found.supportStatus === "supported" ? found : null;
  }, [engines, selection.engine]);
  const currentInstance = useMemo(() => {
    if (!currentEngine) return null;
    if (selection.instanceId) {
      return currentEngine.instances.find((item) => item.key === selection.instanceId)
        || currentEngine.instances.find((item) => item.instance_id === selection.instanceId)
        || currentEngine.primaryInstance;
    }
    return currentEngine.primaryInstance;
  }, [currentEngine, selection.instanceId]);
  const selectedCredential = useMemo(() => {
    const item = currentEngine?.credentials.find((row) => row.id === selection.credentialId);
    if (!item) return null;
    const normalized = normalizeConversationCredential(item);
    if (!normalized || !currentInstance) return item;
    const scopedCatalog = runtimeScopesModelCatalog(descriptors, currentInstance.key);
    const scoped = credentialForRuntime(normalized, currentInstance.key, scopedCatalog);
    const catalog = item.model_catalogs?.[currentInstance.key];
    const available = allCredentialModels(scoped).map((model) => model.id);
    return {
      ...item,
      models: scoped.models.map((model) => model.id),
      candidate_models: scoped.candidate_models.map((model) => model.id),
      default_model: catalog?.default_model || (available.includes(item.default_model || "") ? item.default_model : ""),
      model_catalog: catalog || (scopedCatalog ? undefined : item.model_catalog),
    };
  }, [currentEngine, currentInstance, descriptors, selection.credentialId]);
  const currentEngineRef = useRef(currentEngine);
  const selectedCredentialRef = useRef(selectedCredential);
  const currentInstanceRef = useRef(currentInstance);
  currentEngineRef.current = currentEngine;
  selectedCredentialRef.current = selectedCredential;
  currentInstanceRef.current = currentInstance;
  const currentTransportFields = useMemo(() => currentInstance ? currentInstance.config_schema?.properties?.transport?.properties ?? declaredTransportFields(descriptors, currentInstance.adapter_id) : {}, [currentInstance, descriptors]);
  const conversationCredentials = useMemo(() => credentials
    .filter((item) => asEngine(descriptors, item.engine))
    .map(normalizeConversationCredential)
    .filter((item): item is ConversationCredential => item !== null)
    .map((item) => {
      const runtime = item.engine === currentEngine?.engine ? currentInstance
        : engines.find((engine) => engine.engine === item.engine)?.primaryInstance;
      return runtime ? credentialForRuntime(item, runtime.key, runtimeScopesModelCatalog(descriptors, runtime.key)) : item;
    }), [credentials, currentEngine, currentInstance, descriptors, engines]);

  const runtimeKey = currentEngine ? currentInstance?.key || `${engineCliAdapter(descriptors, currentEngine.engine)}:default` : "";
  const modelRows = useMemo<AgentModelRow[]>(() => {
    if (!currentEngine || !runtimeKey) return [];
    return currentEngine.credentials.flatMap((item) => {
      const normalized = normalizeConversationCredential(item);
      if (!normalized) return [];
      const scoped = credentialForRuntime(normalized, runtimeKey, runtimeScopesModelCatalog(descriptors, runtimeKey));
      return allCredentialModels(scoped).map((model: ConversationCredentialModel) => ({
        key: `${item.id}${model.id}`,
        credentialId: item.id,
        credentialName: credentialName(item),
        model,
        verified: scoped.models.some((probed) => probed.id === model.id),
      }));
    });
  }, [currentEngine, descriptors, runtimeKey]);

  useEffect(() => {
    const engine = currentEngineRef.current;
    const item = selectedCredentialRef.current;
    setEditBaseRevision(item?.revision);
    setEditConnection(item?.connection === "custom_endpoint" ? "custom_endpoint" : "official");
    setEditProvider(item?.provider || ""); setEditBaseUrl(item?.base_url || "");
    setEditDefaultModel(item?.default_model || item?.candidate_models?.[0] || item?.models[0] || "");
    setEditModelsText((item?.candidate_models || item?.models || []).join("\n"));
    setModelRefreshFeedback(null);
    setEditSecret(""); setShowSecret(false); setImportAccountId(`${engine?.engine || "agent"}-account`);
  }, [selection.engine, selectedCredential?.id]);
  useEffect(() => { const instance = currentInstanceRef.current; setRuntimeBaseRevision(instance?.updated_at || ""); setEditBinaryPath(instance?.binary_path || ""); setEditEndpoint(instance?.endpoint || ""); setEditTransport({ ...(instance?.transport || {}) }); setEditEnvRefs(envRefRows(instance?.env_refs)); setEditLaunchArgs((instance?.launch_args || []).join("\n")); }, [currentInstance?.key]);

  const editIdentity = JSON.stringify([selectedCredential?.id, editConnection, editProvider, editBaseUrl,
    editSecret, editDefaultModel, editModelsText, backend, currentInstance?.key]);
  const editIdentityRef = useRef(editIdentity);
  editIdentityRef.current = editIdentity;
  const editHasUnsavedEndpoint = Boolean(selectedCredential?.source === "stored" && (
    editSecret.trim() || editConnection !== selectedCredential.connection
    || editBaseUrl.trim().replace(/\/$/, "") !== (selectedCredential.base_url || "").replace(/\/$/, "")
    || (selectedCredential.revision && selectedCredential.revision !== editBaseRevision)
  ));
  useEffect(() => {
    const item = selectedCredentialRef.current;
    if (!item) return;
    const credentialId = item.id;
    setTestResults((current) => {
      if (!current[credentialId]) return current;
      const next = { ...current }; delete next[credentialId]; return next;
    });
  }, [selectedCredential?.id, selectedCredential?.revision, editConnection, editBaseUrl, editSecret, editDefaultModel, backend, currentInstance?.key]);
  const assertRuntimeResponse = async (response: Response, title = "Agent 运行环境操作") => {
    const payload = await response.json();
    if (payload.receipt) recordTaskReceipt(payload.receipt, title, "runtime");
    if (!response.ok || payload.ok === false || payload.receipt?.error || payload.receipt?.state === "failed") {
      throw new Error(payload.receipt?.error?.message || payload.error?.message || payload.detail?.message || payload.detail || `HTTP ${response.status}`);
    }
    if (["accepted", "running", "waiting"].includes(payload.receipt?.state)) {
      const pending = new Error("命令已受理，尚未完成；请在任务中心查看最终状态。");
      pending.name = "RuntimeOperationPending";
      throw pending;
    }
    if (payload.receipt?.state !== "completed") throw new Error("服务未返回已完成的命令回执，请重新核对状态。");
    return payload;
  };

  const selectEngineRow = (engine: Engine, instanceId: string) => {
    const item = engines.find((entry) => entry.engine === engine);
    if (!item || item.supportStatus !== "supported") return;
    setTestOpen(false);
    setCreateOpen(false);
    if (selection.engine === engine && selection.instanceId === instanceId) {
      applySelection({ engine: null, instanceId: "", credentialId: "" }, "push");
    } else {
      applySelection({ engine, instanceId, credentialId: "" }, "push");
    }
  };
  const toggleCredentialRow = (item: GlobalCredential) => {
    if (!currentEngine) return;
    setTestOpen(false);
    const expanding = selectedCredential?.id !== item.id;
    applySelection(
      { engine: currentEngine.engine, instanceId: selection.instanceId, credentialId: expanding ? item.id : "" },
      "replace",
    );
  };

  const refreshAll = useCallback(async (silent = false) => {
    if (refreshInFlight.current) return;
    refreshInFlight.current = true;
    if (!silent) setRefreshing(true);
    try {
      const response = await apiFetch("/api/agent-runtimes/refresh", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ transport_kind: taskSettings ? "cli" : "structured" }) });
      await assertRuntimeResponse(response, "全部 Agent 探测");
      const catalogResponse = await apiFetch(`/api/settings/credential-models/refresh-stale?environment=${backend}`, { method: "POST" });
      if (!catalogResponse.ok) {
        const catalogPayload = await catalogResponse.json().catch(() => ({}));
        throw new Error(catalogPayload.detail?.message || catalogPayload.detail || `模型目录刷新 HTTP ${catalogResponse.status}`);
      }
      lastAutomaticRefreshAt.current = Date.now();
      if (!await loadData(true)) return;
      if (!silent) showFeedback("ok", "全部 Agent 的凭据、运行环境和最新版本已重新探测。");
    } catch (error) {
      if (!silent) showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `探测失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      refreshInFlight.current = false;
      if (!silent) setRefreshing(false);
    }
  }, [backend, loadData, showFeedback, taskSettings]);

  useEffect(() => {
    const refreshIfStale = () => {
      if (document.visibilityState !== "visible") return;
      if (Date.now() - lastAutomaticRefreshAt.current < AUTO_REFRESH_MS) return;
      lastAutomaticRefreshAt.current = Date.now();
      void loadData(true);
    };
    const interval = window.setInterval(refreshIfStale, AUTO_REFRESH_MS);
    window.addEventListener("focus", refreshIfStale);
    document.addEventListener("visibilitychange", refreshIfStale);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshIfStale);
      document.removeEventListener("visibilitychange", refreshIfStale);
    };
  }, [loadData]);

  const probeEngine = async (item: EngineSummary) => {
    if (item.supportStatus !== "supported") return;
    const key = currentInstanceRef.current?.key || item.primaryInstance?.key || `${engineCliAdapter(descriptors, item.engine)}:default`;
    setBusyItem(`probe:${item.engine}`);
    try {
      const response = await apiFetch(`/api/agent-runtimes/${encodeURIComponent(key)}/probe`, { method: "POST" });
      await assertRuntimeResponse(response, `探测 ${item.label}`);
      const catalogs = await Promise.all(item.credentials.filter(credentialReady).map(async (credential) => ({
        credential,
        result: await refreshCredentialModels(credential.id, {
          engine: item.engine,
          backend: credential.source === "system" ? "local" : backend,
          runtime_instance: key,
        }),
      })));
      const failed = catalogs.filter(({ result }) => !result.ok);
      if (await loadData(true)) showFeedback(failed.length ? "warn" : "ok", failed.length
        ? `${item.label} 运行环境探测完成；模型目录刷新失败：${failed.map(({ credential, result }) => `${credential.label}：${result.detail}`).join("；")}`
        : `${item.label} 运行环境与模型目录已刷新。`);
    } catch (error) { showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `探测失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const toggleEngine = async (item: EngineSummary) => {
    if (item.supportStatus !== "supported") return;
    setBusyItem(`toggle:${item.engine}`);
    try {
      const inst = item.primaryInstance;
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ adapter_id: inst?.adapter_id || engineCliAdapter(descriptors, item.engine), instance_id: inst?.instance_id || "default", expected_revision: inst?.updated_at || "", enabled: !item.enabled }) });
      await assertRuntimeResponse(response, `${item.enabled ? "停用" : "启用"} ${item.label}`);
      if (await loadData(true)) showFeedback("ok", `${item.label} 已${item.enabled ? "停用" : "启用"}。`);
    } catch (error) { showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `操作失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const refreshCandidateModels = async () => {
    if (!currentEngine || !selectedCredential) return;
    const system = selectedCredential.source === "system";
    if (!system && editConnection === "custom_endpoint" && !editBaseUrl.trim()) {
      setModelRefreshFeedback({ kind: "bad", detail: "更新失败：请先填写 Base URL。" });
      showFeedback("bad", "自定义端点需要填写 Base URL，才能更新候选模型。");
      return;
    }
    const requestIdentity = editIdentity;
    setModelRefreshFeedback(null);
    setBusyItem("refresh-models");
    try {
      const result = await refreshCredentialModels(selectedCredential.id, {
        engine: currentEngine.engine,
        connection: system ? "official" : editConnection,
        base_url: !system && editConnection === "custom_endpoint" ? editBaseUrl.trim() : "",
        secret: system ? "" : editSecret.trim(),
        backend: system ? "local" : backend,
        runtime_instance: runtimeKey,
      });
      if (editIdentityRef.current !== requestIdentity) return;
      if (!result.ok) {
        const detail = `更新失败：${result.detail || "服务未返回模型"}`;
        await loadData(true);
        if (editIdentityRef.current !== requestIdentity) return;
        setModelRefreshFeedback({ kind: "bad", detail });
        showFeedback("bad", `更新候选模型失败：${result.detail || "服务未返回模型"}`);
        return;
      }
      setEditModelsText(result.models.join("\n"));
      if (editDefaultModel && !result.models.includes(editDefaultModel)) {
        setEditDefaultModel(result.models[0] || "");
      } else if (!editDefaultModel && result.models[0]) {
        setEditDefaultModel(result.models[0]);
      }
      await loadData(true);
      if (selectedCredentialRef.current?.id !== selectedCredential.id) return;
      setModelRefreshFeedback({
        kind: "ok",
        detail: `更新成功：${result.detail || `已获取 ${result.models.length} 个候选模型。`}`,
      });
      showFeedback("ok", result.detail || `已更新 ${result.models.length} 个候选模型。`);
    } catch (error) {
      if (editIdentityRef.current !== requestIdentity) return;
      const detail = `更新失败：${error instanceof Error ? error.message : String(error)}`;
      setModelRefreshFeedback({ kind: "bad", detail });
      showFeedback("bad", `更新候选模型失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusyItem("");
    }
  };
  const saveCredential = async () => {
    if (!currentEngine || !selectedCredential || selectedCredential.source !== "stored") return;
    if (editConnection === "custom_endpoint" && !editBaseUrl.trim()) { showFeedback("bad", "自定义端点需要填写 Base URL。"); return; }
    const accountId = selectedCredential.account_id || selectedCredential.id.replace(/^account:/, "");
    const requestIdentity = editIdentity;
    const savedCredentialId = selectedCredential.id;
    setBusyItem("save-credential");
    try {
      const secretPatch = editSecret.trim() ? editConnection === "official" && authHomeSecret(currentEngine.engine) ? { codex_auth_json: editSecret.trim() } : { secret: editSecret.trim() } : {};
      const savedAccount = await putCredentialAccount(accountId, { expected_revision: editBaseRevision, engine: editConnection === "custom_endpoint" ? "api" : currentEngine.engine, worker_engine: currentEngine.engine, target_engine: editConnection === "custom_endpoint" ? currentEngine.engine : undefined, connection: editConnection, provider: editConnection === "custom_endpoint" ? editProvider.trim() : undefined, base_url: editConnection === "custom_endpoint" ? editBaseUrl.trim() : "", target_model: editDefaultModel.trim(), models: splitModels(editModelsText), ...secretPatch });
      setTestResults((current) => { const next = { ...current }; delete next[savedCredentialId]; return next; });
      if (editIdentityRef.current === requestIdentity) setEditSecret("");
      await loadData(true); onCredentialsChanged?.("save");
      if (selectedCredentialRef.current?.id !== savedCredentialId) return;
      setEditBaseRevision(savedAccount?.revision);
      showFeedback("ok", `${credentialName(selectedCredential)} 已保存。`);
    } catch (error) { showFeedback("bad", `保存失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const testCredential = async () => {
    if (!currentEngine || !selectedCredential) return;
    if (editHasUnsavedEndpoint) { showFeedback("bad", "请先保存当前端点与凭据，再测试已保存配置。"); return; }
    const credentialId = selectedCredential.id;
    const requestIdentity = editIdentity;
    setBusyItem("test-credential");
    setTestOpen(true);
    setTestResults((current) => {
      const next = { ...current };
      delete next[credentialId];
      return next;
    });
    try {
      const result = await testGlobalCredential(credentialId, currentEngine.engine, backend, editDefaultModel.trim() || selectedCredential.default_model || credentialModels(selectedCredential)[0] || "", runtimeKey);
      if (editIdentityRef.current !== requestIdentity) return;
      setTestResults((current) => ({ ...current, [credentialId]: result }));
      await loadData(true);
    } catch (error) {
      const detail = `测试异常：${error instanceof Error ? error.message : String(error)}`;
      setTestResults((current) => ({
        ...current,
        [credentialId]: {
          ok: false,
          detail,
          model: editDefaultModel.trim() || selectedCredential.default_model || "",
          engine: currentEngine.engine,
          backend,
          logs: [{ stream: "error", message: detail, elapsed_ms: 0 }],
        },
      }));
    } finally { setBusyItem(""); }
  };
  // Only structured adapters receive resolved env_refs; the CLI compatibility builder ignores them.
  const currentAdapter = currentInstance ? adapterDescriptor(descriptors, currentInstance.adapter_id) : undefined;
  const currentDescriptor = currentEngine ? descriptorForEngine(descriptors, currentEngine.engine) : undefined;
  const connectionOptions: ListOption[] = !taskSettings ? (currentDescriptor?.adapters || [])
    .filter((item) => item.role === "default" || item.role === "variant")
    .map((item) => ({
      value: item.adapter_id,
      label: `${item.transport_kind.toUpperCase()}${item.role === "default" ? "（默认）" : ""}`,
      description: item.notes || item.adapter_id,
      disabled: !item.capabilities.access_modes.length,
    })) : [];
  const accessModeRows = currentAdapter ? Array.from(new Set([
    ...currentAdapter.capabilities.access_modes, ...Object.keys(currentAdapter.access_mode_notes),
  ])) : [];
  const envEditable = Boolean(currentAdapter && currentAdapter.role !== "cli");
  const saveRuntime = async () => {
    if (!currentEngine) return;
    const targetKey = currentInstance?.key || `${engineCliAdapter(descriptors, currentEngine.engine)}:default`;
    const env = envRefsPayload(editEnvRefs);
    if (envEditable && env.error) { showFeedback("bad", `保存失败：环境变量 ${env.error}`); return; }
    setBusyItem("save-runtime");
    try {
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ expected_revision: runtimeBaseRevision, adapter_id: currentInstance?.adapter_id || engineCliAdapter(descriptors, currentEngine.engine), instance_id: currentInstance?.instance_id || "default", label: currentInstance?.label || currentEngine.label, binary_path: editBinaryPath.trim(), endpoint: editEndpoint.trim(), transport: editTransport, ...(envEditable ? { env_refs: env.refs, launch_args: launchArgsPayload(editLaunchArgs) } : {}), enabled: currentEngine.enabled }) });
      const payload = await assertRuntimeResponse(response, `保存 ${currentEngine.label} 接入配置`);
      if (currentInstanceRef.current?.key === targetKey) setRuntimeBaseRevision(payload.receipt?.output?.instance?.updated_at || runtimeBaseRevision);
      if (await loadData(true)) showFeedback("ok", `${currentEngine.label} 的接入与环境配置已保存。`);
    } catch (error) { showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `保存失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const importSystemLogin = async () => {
    if (!currentEngine || !selectedCredential || selectedCredential.source !== "system") return;
    const id = importAccountId.trim(); if (!id) { showFeedback("bad", "请填写导入后的凭据 ID。"); return; }
    setBusyItem("import-system");
    try {
      const engine = currentEngine.engine;
      const hostImport = hostLoginImport(engine);
      const result = hostImport === "codex_auth_json" ? await importHostCodexAuth(id) : hostImport !== "none" ? await importHostWorkerLogin(id, engine) : { ok: false, detail: "该引擎暂不支持导入宿主登录。" };
      if (!result.ok) throw new Error(result.detail || "请检查宿主 CLI 登录状态");
      await loadData(true); onCredentialsChanged?.("import");
      applySelection({ engine, instanceId: selection.instanceId, credentialId: `account:${id}` }, "replace");
      showFeedback("ok", `宿主登录已导入为 ${id}。`);
    } catch (error) { showFeedback("bad", `导入失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const openCreateCredential = () => {
    if (!currentEngine) return;
    setNewDraft(EMPTY_DRAFT); setCreateError(""); setCreateEngine(currentEngine.engine); setCreateOpen(true);
  };
  const closeCreateCredential = () => {
    setCreateOpen(false); setCreateError(""); setNewDraft(EMPTY_DRAFT); setCreateEngine(null);
  };
  const createCredential = async () => {
    if (!currentEngine || createEngine !== currentEngine.engine) { setCreateError("新增目标已改变，请重新打开新增凭据。"); return; }
    const engine = currentEngine.engine;
    const id = newDraft.accountId.trim();
    if (!id) { setCreateError("请填写凭据 ID。"); return; }
    if (credentials.some((item) => item.account_id === id || item.id === `account:${id}`)) { setCreateError("该凭据 ID 已存在，请使用不同 ID。"); return; }
    if (newDraft.connection === "custom_endpoint" && !newDraft.baseUrl.trim()) { setCreateError("请填写 Base URL。"); return; }
    if (newDraft.connection === "custom_endpoint" && !newDraft.secret.trim()) { setCreateError("请填写 API Key。"); return; }
    setCreating(true); setCreateError("");
    try {
      const secretPatch = newDraft.secret.trim() ? newDraft.connection === "official" && authHomeSecret(engine) ? { codex_auth_json: newDraft.secret.trim() } : { secret: newDraft.secret.trim() } : {};
      await putCredentialAccount(id, { create_only: true, engine: newDraft.connection === "custom_endpoint" ? "api" : engine, worker_engine: engine, target_engine: newDraft.connection === "custom_endpoint" ? engine : undefined, connection: newDraft.connection, provider: newDraft.connection === "custom_endpoint" ? newDraft.provider.trim() : undefined, base_url: newDraft.connection === "custom_endpoint" ? newDraft.baseUrl.trim() : "", target_model: newDraft.defaultModel.trim(), models: splitModels(newDraft.modelsText), ...secretPatch });
      setCreateOpen(false); setNewDraft(EMPTY_DRAFT);
      await loadData(true); onCredentialsChanged?.("create");
      applySelection({ engine, instanceId: selection.instanceId, credentialId: `account:${id}` }, "replace");
      showFeedback("ok", `${currentEngine.label} 已新增凭据 ${id}。`);
    } catch (error) { setCreateError(error instanceof Error ? error.message : String(error)); }
    finally { setCreating(false); }
  };
  const deleteCredential = async () => {
    if (!deleteTarget) return;
    const id = deleteTarget.account_id || deleteTarget.id.replace(/^account:/, "");
    const deletedId = deleteTarget.id;
    setDeleting(true); setDeleteError("");
    try {
      const deleted = await deleteCredentialAccount(id, true);
      if (!deleted) throw new Error("服务未确认删除成功，引用保持原状，请刷新后重试。");
      setDeleteTarget(null);
      if (selectedCredentialRef.current?.id === deletedId && currentEngineRef.current) {
        applySelection({ engine: currentEngineRef.current.engine, instanceId: selection.instanceId, credentialId: "" }, "replace");
      }
      await loadData(true); onCredentialsChanged?.("delete");
      showFeedback("ok", `凭据 ${id} 及其配置引用已删除。`);
    }
    catch (error) { setDeleteError(error instanceof Error ? error.message : String(error)); }
    finally { setDeleting(false); }
  };
  const selectChatDefault = (credentialId: string, modelId: string) => {
    if (!credentialId || !modelId) return;
    const next = { credentialId, modelId };
    if (!writeChatDefaultModel(next)) { showFeedback("bad", "默认模型未保存：共享偏好未能提交，请查看同步提示。"); return; }
    setChatDefaultCredentialId(credentialId); setChatDefaultModelId(modelId); setChatDefault(next);
  };

  const testingCredential = busyItem === "test-credential";
  const liveTestResult = selectedCredential && !editHasUnsavedEndpoint ? testResults[selectedCredential.id] || null : null;
  const candidateTestResult = liveTestResult || (!editHasUnsavedEndpoint && selectedCredential && currentEngine ? lastTestAsResult(selectedCredential, currentEngine.engine) : null);
  const displayedTestResult = candidateTestResult?.backend === backend && candidateTestResult.model === editDefaultModel.trim() ? candidateTestResult : null;
  const liveStatus = selectedCredential ? credentialLiveStatus(selectedCredential, displayedTestResult, testingCredential) : null;
  const systemModels = selectedCredential ? credentialModels(selectedCredential) : [];
  const verifiedModels = selectedCredential?.models || [];
  const firstSystemModel = systemModels[0] || "";
  useEffect(() => {
    if (editDefaultModel || !firstSystemModel) return;
    setEditDefaultModel(firstSystemModel);
  }, [editDefaultModel, firstSystemModel]);

  const capabilityEntries = Object.entries(currentInstance?.health?.capabilities || {});
  const instanceOptions: ListOption<string>[] = (currentEngine?.instances || []).map((instance) => ({
    value: instance.instance_id,
    label: instance.label || instance.instance_id,
    description: instance.key,
  }));

  const renderCredentialRow = (item: GlobalCredential) => {
    if (!currentEngine) return null;
    const expanded = selectedCredential?.id === item.id;
    const rowResult = testResults[item.id] || lastTestAsResult(item, currentEngine.engine);
    const rowStatus = credentialLiveStatus(item, expanded ? displayedTestResult : rowResult, expanded && testingCredential);
    return (
      <div key={item.id} className="border-b border-cx-border-subtle last:border-b-0">
        <button
          type="button"
          aria-expanded={expanded}
          onClick={() => toggleCredentialRow(item)}
          className="flex w-full items-center gap-3 px-5 py-3 text-left outline-none transition-colors hover:bg-cx-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
        >
          {["warn", "bad"].includes(rowStatus.kind) ? <StatusDot tone={statusTone(rowStatus.kind)} label={rowStatus.text} /> : null}
          <span className="min-w-0 flex-1">
            <span className="flex items-center gap-1.5 text-[13.5px] font-medium text-cx-fg">
              <span className="truncate">{credentialName(item)}</span>
              {chatDefault?.credentialId === item.id ? <Icon name="star" size={12} filled className="shrink-0 text-cx-warning" aria-label="新对话默认凭据" /> : null}
            </span>
            <span className="mt-0.5 block truncate text-[11.5px] leading-4 text-cx-fg-3">
              {item.source === "system" ? "宿主登录" : item.connection === "custom_endpoint" ? "自定义端点" : "官方账号"} · {rowStatus.text}
            </span>
          </span>
          <Icon name="chevronDown" size={15} className={cn("shrink-0 text-cx-fg-3 transition-transform", expanded && "rotate-180")} />
        </button>
        <Collapse open={expanded}>
          {expanded && selectedCredential && liveStatus ? (
            <div className="flex flex-col gap-4 border-t border-cx-border-subtle px-5 py-4">
              <div className="flex flex-wrap items-center gap-2">
                <Badge tone={statusTone(liveStatus.kind)} dot>{liveStatus.text}</Badge>
                <span className="text-[12px] text-cx-fg-3">
                  {selectedCredential.source === "system" ? "系统检测到的宿主登录状态" : `凭据 ID：${selectedCredential.account_id || selectedCredential.id.replace(/^account:/, "")}`}
                </span>
              </div>
              {selectedCredential.sharing && <p className="text-[12.5px] text-cx-fg-3">{selectedCredential.sharing.reason}</p>}
              {selectedCredential.source === "stored" && selectedCredential.revision !== editBaseRevision ? (
                <Callout
                  tone="warning"
                  icon="alert"
                  role="alert"
                  action={<Button size="xs" variant="outline" onClick={() => {
                    setEditBaseRevision(selectedCredential.revision); setEditConnection(selectedCredential.connection === "custom_endpoint" ? "custom_endpoint" : "official");
                    setEditProvider(selectedCredential.provider || ""); setEditBaseUrl(selectedCredential.base_url || "");
                    setEditDefaultModel(selectedCredential.default_model || ""); setEditModelsText((selectedCredential.candidate_models || selectedCredential.models).join("\n")); setEditSecret("");
                  }}>载入最新配置并清除本地编辑</Button>}
                >
                  配置已在其它视图更新，本地编辑仍保留；请载入最新配置后重新核对。
                </Callout>
              ) : null}
              {selectedCredential.source === "system" ? (
                <>
                  <div className="flex items-start gap-3 rounded-xl border border-cx-border bg-cx-bg-subtle px-4 py-3">
                    <Icon name={credentialReady(selectedCredential) ? "checkCircle" : "info"} size={17} className={cn("mt-0.5 shrink-0", credentialReady(selectedCredential) ? "text-cx-success" : "text-cx-fg-3")} />
                    <div className="min-w-0">
                      <p className="text-[13px] font-medium text-cx-fg">{credentialReady(selectedCredential) ? "本机 CLI 登录可用" : "未检测到可用的本机 CLI 登录"}</p>
                      <p className="mt-0.5 text-[12.5px] leading-5 text-cx-fg-3">{credentialReady(selectedCredential) ? "本地聊天与本地 Worker 可以直接使用。容器 Worker 需要导入为独立凭据。" : "请先完成该 CLI 的登录，再刷新探测。"}</p>
                    </div>
                  </div>
                  {hostLoginImport(currentEngine.engine) !== "none" ? (
                    <div className="flex flex-wrap items-end gap-2">
                      <TextField
                        label="导入后的凭据 ID"
                        value={importAccountId}
                        onChange={(event) => setImportAccountId(event.target.value)}
                        containerClassName="min-w-[220px] flex-1"
                      />
                      <Button size="sm" variant="outline" icon="upload" loading={busyItem === "import-system"} disabled={!credentialReady(selectedCredential)} onClick={() => void importSystemLogin()}>
                        {busyItem === "import-system" ? "导入中…" : "导入为独立凭据"}
                      </Button>
                    </div>
                  ) : null}
                  <div>
                    <div className="flex items-center justify-between gap-3">
                      <p className="text-[13px] font-medium text-cx-fg">模型目录</p>
                      <Button size="xs" variant="outline" icon="refresh" loading={busyItem === "refresh-models"} onClick={() => void refreshCandidateModels()}>刷新模型</Button>
                    </div>
                    {modelRefreshFeedback ? <p role={modelRefreshFeedback.kind === "bad" ? "alert" : "status"} className={cn("mt-2 text-[12.5px]", modelRefreshFeedback.kind === "bad" ? "text-cx-danger" : "text-cx-success")}>{modelRefreshFeedback.detail}</p> : null}
                    <p className="mt-0.5 text-[12.5px] leading-5 text-cx-fg-3">
                      {credentialReady(selectedCredential) ? `来自当前运行环境的模型目录。选中的模型会用于真实连通测试。${catalogSummary(selectedCredential.model_catalog)}` : "本机登录可用后，会列出该引擎当前可用的模型。"}
                    </p>
                    {systemModels.length ? (
                      <div className="mt-2 flex flex-wrap gap-1.5" role="list">
                        {systemModels.map((model) => (
                          <Button
                            key={model}
                            size="xs"
                            variant={editDefaultModel === model ? "primary" : "outline"}
                            aria-pressed={editDefaultModel === model}
                            onClick={() => setEditDefaultModel(model)}
                          >
                            <code>{model}</code>
                            {verifiedModels.includes(model) ? <span className="text-[10.5px] opacity-80">已测通</span> : null}
                          </Button>
                        ))}
                      </div>
                    ) : (
                      <p className="mt-2 text-[12.5px] text-cx-fg-3">尚未发现可用模型。</p>
                    )}
                  </div>
                </>
              ) : (
                <div className="grid gap-4">
                  <fieldset disabled={selectedCredential.sharing?.readonly} className="grid gap-4">
                  <div>
                    <span className="mb-1.5 block text-[12.5px] font-medium text-cx-fg-2">连接方式</span>
                    <Select value={editConnection} onChange={(value) => setEditConnection(value)} options={CONNECTION_OPTIONS} ariaLabel="连接方式" className="w-full sm:max-w-[280px]" />
                  </div>
                  {editConnection === "custom_endpoint" ? (
                    <>
                      <p className="flex items-start gap-1.5 rounded-xl border border-cx-border bg-cx-bg-subtle px-3 py-2 text-[12.5px] leading-5 text-cx-fg-3">
                        <Icon name="info" size={14} className="mt-0.5 shrink-0" />
                        <span><strong className="text-cx-fg-2">两种使用方式：</strong>Worker 使用当前 {currentEngine.label} 引擎调用；Reason / Titler 复用同一端点时，由 Muteki 后端直接发送 HTTP 请求，不会启动该 Agent。</span>
                      </p>
                      <div className="grid gap-4 sm:grid-cols-2">
                        <TextField label="Base URL" placeholder="https://api.example.com/v1" value={editBaseUrl} onChange={(event) => setEditBaseUrl(event.target.value)} />
                        <TextField label="服务名称" placeholder="团队网关 / 模型服务" value={editProvider} onChange={(event) => setEditProvider(event.target.value)} />
                      </div>
                    </>
                  ) : null}
                  <TextField
                    label={authHomeSecret(currentEngine.engine) && editConnection === "official" ? "Auth JSON" : "Secret / Token / API Key"}
                    type={showSecret ? "text" : "password"}
                    placeholder="留空时保留已保存的凭据"
                    value={editSecret}
                    onChange={(event) => setEditSecret(event.target.value)}
                    trailing={
                      <button
                        type="button"
                        aria-label={showSecret ? "隐藏凭据" : "显示凭据"}
                        onClick={() => setShowSecret((value) => !value)}
                        className="grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"
                      >
                        <Icon name={showSecret ? "eyeOff" : "eye"} size={14} />
                      </button>
                    }
                  />
                  </fieldset>
                  <div>
                    <TextField
                      label="默认模型"
                      list={`agent-credential-models-${selectedCredential.id}`}
                      autoComplete="off"
                      placeholder="输入或选择模型 ID"
                      value={editDefaultModel}
                      onChange={(event) => setEditDefaultModel(event.target.value)}
                    />
                    <datalist id={`agent-credential-models-${selectedCredential.id}`}>
                      {splitModels(editModelsText).map((model) => <option key={model} value={model} />)}
                    </datalist>
                  </div>
                  <CandidateModelsField value={editModelsText} refreshing={busyItem === "refresh-models"} onRefresh={() => void refreshCandidateModels()} catalog={selectedCredential.model_catalog} feedback={modelRefreshFeedback} />
                </div>
              )}
              {displayedTestResult ? (
                <button
                  type="button"
                  onClick={() => setTestOpen(true)}
                  className={cn(
                    "flex items-center gap-2 rounded-xl border px-3 py-2 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                    displayedTestResult.ok ? "border-cx-border bg-cx-success-soft/40 hover:bg-cx-success-soft" : "border-cx-border bg-cx-danger-soft/40 hover:bg-cx-danger-soft",
                  )}
                >
                  <Icon name={displayedTestResult.ok ? "checkCircle" : "alert"} size={15} className={displayedTestResult.ok ? "text-cx-success" : "text-cx-danger"} />
                  <span className="min-w-0 flex-1">
                    <span className="block text-[12.5px] font-medium text-cx-fg">{displayedTestResult.ok ? "最近一次真实连通：可用" : "最近一次真实连通：不可用"}</span>
                    <span className="block truncate text-[11.5px] text-cx-fg-3">{[displayedTestResult.detail, displayedTestResult.backend === "container" ? "容器环境" : "本地环境", formatTestTime(displayedTestResult.tested_at)].filter(Boolean).join(" · ")}</span>
                  </span>
                  <Icon name="chevronRight" size={14} className="shrink-0 text-cx-fg-3" />
                </button>
              ) : (
                <p className="text-[12.5px] text-cx-fg-3">还没有真实连通记录。点下方按钮后会弹出测试终端，显示该凭据是否可用。</p>
              )}
              <div className="flex flex-wrap items-center gap-2">
                {taskSettings && <Select size="sm" value={backend} onChange={(value) => setBackend(value)} options={BACKEND_OPTIONS} ariaLabel="连通测试后端" />}
                <Button size="sm" variant="outline" icon="plug" loading={testingCredential} onClick={() => void testCredential()}>
                  {testingCredential ? "测试中…" : "真实连通测试"}
                </Button>
                {selectedCredential.source === "stored" ? (
                  <>
                    <span className="flex-1" />
                    <Button size="sm" variant="primary" icon="check" loading={busyItem === "save-credential"} onClick={() => void saveCredential()}>
                      {busyItem === "save-credential" ? "保存中…" : selectedCredential.sharing?.readonly ? "保存本环境模型设置" : "保存凭据"}
                    </Button>
                    <Button size="sm" variant="danger-soft" icon="trash" disabled={selectedCredential.sharing?.readonly} onClick={() => { setDeleteError(""); setDeleteTarget(selectedCredential); }}>删除</Button>
                  </>
                ) : null}
              </div>
            </div>
          ) : null}
        </Collapse>
      </div>
    );
  };

  return (
    <div className="agents-settings @container/agents flex w-full min-w-0 flex-col gap-6" aria-busy={loading}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[13px] leading-5 text-cx-fg-3">
          {taskSettings ? "在这里新增、导入、测试和管理做题 Worker 使用的凭据。带 Base URL 的自定义端点也可供 Reason / Titler 使用。" : "选择左侧引擎，在右侧管理它的凭据、接入方式与模型。"}
        </p>
        <div className="flex items-center gap-3">
          {lastCheckedAt ? <span className="inline-flex items-center gap-1.5 text-[12px] text-cx-fg-3"><Icon name="clock" size={13} />{lastCheckedAt}</span> : null}
          <Button size="sm" variant="outline" icon="refresh" loading={refreshing} disabled={loading} onClick={() => void refreshAll()}>
            {refreshing ? "探测中…" : "刷新探测"}
          </Button>
        </div>
      </div>
      {feedback ? (
        <Callout
          tone={feedback.kind === "ok" ? "success" : feedback.kind === "warn" ? "warning" : "danger"}
          icon={feedback.kind === "ok" ? "checkCircle" : feedback.kind === "warn" ? "info" : "alert"}
          role="alert"
          onDismiss={() => setFeedback(null)}
        >
          {feedback.detail}
        </Callout>
      ) : null}

      {!taskSettings ? (
        <SettingsSection anchor="agents-default-model" title="新对话默认模型" description="打开新对话时优先使用的凭据和基座模型。">
          <div className="flex flex-wrap items-center gap-3 px-5 py-4">
            <ConversationModelPicker
              credentials={conversationCredentials}
              selectedCredentialId={chatDefaultCredentialId}
              selectedModel={chatDefaultModelId}
              loading={loading}
              variant="default-model"
              className="provider-default-model-picker min-w-0 flex-1"
              onSelect={({ credentialId, model }) => selectChatDefault(credentialId, model)}
            />
            <Badge tone={chatDefault ? "success" : "neutral"} icon={chatDefault ? "check" : undefined}>{chatDefault ? "已设定" : "未固定"}</Badge>
            {chatDefault ? (
              <IconButton icon="trash" label="清除新对话默认模型" onClick={() => {
                if (!writeChatDefaultModel(null)) { showFeedback("bad", "默认模型未清除：共享偏好未能提交，请查看同步提示。"); return; }
                setChatDefault(null);
                showFeedback("ok", "已提交清除新对话默认模型的设置。");
              }} />
            ) : null}
          </div>
        </SettingsSection>
      ) : null}

      <div className="grid grid-cols-1 items-start gap-6 @3xl/agents:grid-cols-[17rem_minmax(0,1fr)]">
        <EngineList
          engines={engines}
          loading={loading}
          query={listQuery}
          onQueryChange={setListQuery}
          selectedEngine={currentEngine?.engine || null}
          selectedInstanceId={currentEngine && selection.instanceId ? (taskSettings ? currentInstance?.instance_id : currentInstance?.key) || "" : ""}
          useInstanceKeys={!taskSettings}
          busyItem={busyItem}
          onSelect={selectEngineRow}
          onToggle={(item) => void toggleEngine(item)}
          className={cn("@3xl/agents:sticky @3xl/agents:top-2", currentEngine && "hidden @3xl/agents:flex")}
        />

        {currentEngine ? (
          <div className="flex min-w-0 flex-col gap-6">
            <div className="@3xl/agents:hidden">
              <Button size="sm" variant="ghost" icon="arrowLeft" onClick={() => applySelection({ engine: null, instanceId: "", credentialId: "" }, "push")}>
                返回引擎列表
              </Button>
            </div>

            <SettingsSection>
              <div className="flex flex-wrap items-center gap-4 px-5 py-4">
                <span className="grid size-11 shrink-0 place-items-center rounded-xl border border-cx-border bg-cx-bg">
                  <EngineLogo engine={currentEngine.engine} size={24} />
                </span>
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
                    <h2 className="text-[16px] font-semibold text-cx-fg">{currentEngine.label}</h2>
                    {currentEngine.version ? (
                      <code title={versionCheckDetail(currentInstance?.health?.version_check)} className="rounded-md bg-cx-code px-1.5 py-0.5 text-[11.5px] text-cx-fg-2">
                        {installedVersion(currentEngine.version, currentInstance?.health?.version_check)}
                      </code>
                    ) : null}
                    <Badge tone={statusTone(currentEngine.statusKind)} dot>{currentEngine.statusText}</Badge>
                  </div>
                  <p className="mt-1 text-[12.5px] leading-5 text-cx-fg-3">
                    {currentEngine.readyCredentialCount} / {currentEngine.credentials.length} 凭据可用 · {currentEngine.environmentCount} 个运行环境 · 当前环境：{backend === "container" ? "容器 Worker" : taskSettings ? "本地 Worker" : "本地聊天"}
                    {currentInstance ? ` · ${versionStatus(currentInstance.health?.version_check)}` : ""}
                  </p>
                </div>
                <div className="flex items-center gap-2.5">
                  <Button size="sm" variant="outline" icon="refresh" loading={busyItem === `probe:${currentEngine.engine}`} onClick={() => void probeEngine(currentEngine)}>探测</Button>
                  <span className="text-[12.5px] text-cx-fg-3">启用</span>
                  <Switch
                    size="sm"
                    checked={currentEngine.enabled}
                    disabled={busyItem === `toggle:${currentEngine.engine}`}
                    ariaLabel={`${currentEngine.label} 启用状态`}
                    onCheckedChange={() => void toggleEngine(currentEngine)}
                  />
                </div>
              </div>
              {currentEngine.instances.length > 1 ? (
                <div className="flex flex-wrap items-center gap-3 border-t border-cx-border-subtle px-5 py-3">
                  <span className="text-[12.5px] text-cx-fg-3">运行环境实例</span>
                  <Select
                    size="sm"
                    value={currentInstance?.instance_id || ""}
                    onChange={(value) => applySelection({ engine: currentEngine.engine, instanceId: value, credentialId: selection.credentialId }, "push")}
                    options={instanceOptions}
                    ariaLabel="运行环境实例"
                    className="w-full max-w-[300px]"
                  />
                </div>
              ) : null}
              {currentInstance?.auth.login_command ? (
                <div className="flex flex-wrap items-center gap-2 border-t border-cx-border-subtle px-5 py-3">
                  <Icon name="terminal" size={14} className="shrink-0 text-cx-fg-3" />
                  <span className="text-[12.5px] text-cx-fg-3">登录命令</span>
                  <code className="min-w-0 flex-1 truncate rounded-md bg-cx-code px-2 py-1 text-[12px] text-cx-fg">{currentInstance.auth.login_command}</code>
                  <CopyButton text={currentInstance.auth.login_command} label="复制登录命令" />
                </div>
              ) : null}
            </SettingsSection>

            <SettingsSection
              anchor="agents-credentials"
              title="账号与凭据"
              description="凭据可以是官方账号，也可以是带 Base URL 的自定义端点。自定义端点也可供 Reason / Titler 使用。"
              actions={<Button size="sm" variant="outline" icon="plus" onClick={openCreateCredential}>新增凭据</Button>}
            >
              {createOpen ? (
                <div className="flex flex-col gap-4 border-b border-cx-border-subtle px-5 py-4">
                  <p className="text-[13px] font-medium text-cx-fg">为 {currentEngine.label} 新增凭据</p>
                  {createError ? <p role="alert" className="rounded-lg bg-cx-danger-soft px-3 py-2 text-[12.5px] text-cx-danger">{createError}</p> : null}
                  <div className="grid gap-4 sm:grid-cols-2">
                    <TextField label="凭据 ID" autoComplete="off" placeholder="例如 work-account" value={newDraft.accountId} onChange={(event) => setNewDraft((value) => ({ ...value, accountId: event.target.value }))} />
                    <div>
                      <span className="mb-1.5 block text-[12.5px] font-medium text-cx-fg-2">连接方式</span>
                      <Select value={newDraft.connection} onChange={(value) => setNewDraft((draft) => ({ ...draft, connection: value }))} options={CONNECTION_OPTIONS} ariaLabel="连接方式" className="w-full" />
                    </div>
                  </div>
                  {newDraft.connection === "custom_endpoint" ? (
                    <>
                      <p className="text-[12.5px] leading-5 text-cx-fg-3">Reason / Titler 只复用这里保存的 Base URL、API Key 和模型目录，不会调用当前 Agent。</p>
                      <div className="grid gap-4 sm:grid-cols-2">
                        <TextField label="Base URL" placeholder="https://api.example.com/v1" value={newDraft.baseUrl} onChange={(event) => setNewDraft((value) => ({ ...value, baseUrl: event.target.value }))} />
                        <TextField label="服务名称" value={newDraft.provider} onChange={(event) => setNewDraft((value) => ({ ...value, provider: event.target.value }))} />
                      </div>
                    </>
                  ) : null}
                  <TextArea label={createEngine && authHomeSecret(createEngine) && newDraft.connection === "official" ? "Auth JSON" : "Secret / Token / API Key"} rows={3} className="resize-none" value={newDraft.secret} onChange={(event) => setNewDraft((value) => ({ ...value, secret: event.target.value }))} />
                  <div className="grid gap-4 sm:grid-cols-2">
                    <TextField label="默认模型" placeholder="model-id" value={newDraft.defaultModel} onChange={(event) => setNewDraft((value) => ({ ...value, defaultModel: event.target.value }))} />
                    <TextArea label="候选模型" rows={3} className="resize-none" placeholder="每行一个模型 ID" value={newDraft.modelsText} onChange={(event) => setNewDraft((value) => ({ ...value, modelsText: event.target.value }))} />
                  </div>
                  <div className="flex justify-end gap-2">
                    <Button size="sm" variant="ghost" onClick={closeCreateCredential} disabled={creating}>取消</Button>
                    <Button size="sm" variant="primary" icon="plus" loading={creating} disabled={!newDraft.accountId.trim()} onClick={() => void createCredential()}>新增凭据</Button>
                  </div>
                </div>
              ) : null}
              {currentEngine.credentials.length ? currentEngine.credentials.map(renderCredentialRow) : !createOpen ? (
                <SettingsEmpty>尚未添加凭据，可新增官方账号或自定义端点。</SettingsEmpty>
              ) : null}
            </SettingsSection>

            <SettingsSection anchor="agents-runtime" title="运行配置" description="设置本机二进制、服务端点、环境变量和该引擎的传输参数。">
              <div className="flex flex-col gap-4 px-5 py-4">
                {connectionOptions.length > 1 ? (
                  <Select
                    ariaLabel="接入方式"
                    value={currentInstance?.adapter_id || currentDescriptor?.identity.default_adapter_id || ""}
                    options={connectionOptions}
                    onChange={(adapterId) => {
                      const instance = currentEngine.instances.find((row) => row.adapter_id === adapterId);
                      if (instance) applySelection({ ...selection, instanceId: instance.key }, "push");
                    }}
                  />
                ) : null}
                <div className="flex min-w-0 items-center gap-2 text-[12px] text-cx-fg-3">
                  <Icon name="terminal" size={13} className="shrink-0" />
                  <code className="truncate">{currentInstance?.key || `${engineCliAdapter(descriptors, currentEngine.engine)}:default`}</code>
                  {currentInstance ? <span className="shrink-0">· {runtimeLabel(currentInstance)}</span> : null}
                </div>
                {!taskSettings && accessModeRows.length ? (
                  <div aria-label="权限模式说明" className="flex flex-col gap-2 text-[12.5px]">
                    {accessModeRows.map((mode) => (
                      <div key={mode}>
                        <p className="font-medium text-cx-fg-2">{ACCESS_MODE_LABELS[mode] || mode}{currentAdapter?.capabilities.access_modes.includes(mode) ? "" : "（不可用）"}</p>
                        {currentAdapter?.access_mode_notes[mode] ? <p className="mt-0.5 whitespace-pre-wrap text-cx-fg-3">{currentAdapter.access_mode_notes[mode]}</p> : null}
                      </div>
                    ))}
                  </div>
                ) : null}
                {(currentInstance?.updated_at || "") !== runtimeBaseRevision ? (
                  <Callout
                    tone="warning"
                    icon="alert"
                    role="alert"
                    action={<Button size="xs" variant="outline" onClick={() => { setRuntimeBaseRevision(currentInstance?.updated_at || ""); setEditBinaryPath(currentInstance?.binary_path || ""); setEditEndpoint(currentInstance?.endpoint || ""); setEditTransport({ ...(currentInstance?.transport || {}) }); setEditEnvRefs(envRefRows(currentInstance?.env_refs)); setEditLaunchArgs((currentInstance?.launch_args || []).join("\n")); }}>载入最新配置并清除本地编辑</Button>}
                  >
                    运行配置已在其它视图更新，请重新核对。
                  </Callout>
                ) : null}
                <div className="grid gap-4 sm:grid-cols-2">
                  <TextField label="二进制路径" placeholder="留空时自动探测" value={editBinaryPath} onChange={(event) => setEditBinaryPath(event.target.value)} />
                  <TextField label="服务端点" placeholder="留空时使用本地进程" value={editEndpoint} onChange={(event) => setEditEndpoint(event.target.value)} />
                  {Object.entries(currentTransportFields).map(([key, field]) => field.type === "boolean" ? (
                    <Checkbox
                      key={key}
                      checked={Boolean(editTransport[key] ?? field.default)}
                      onCheckedChange={(selected) => setEditTransport((value) => ({ ...value, [key]: selected }))}
                      label={field.title || key}
                    />
                  ) : (
                    <TextField
                      key={key}
                      label={field.title || key}
                      type={field.type === "integer" || field.type === "number" ? "number" : "text"}
                      value={String(editTransport[key] ?? field.default ?? "")}
                      onChange={(event) => setEditTransport((value) => ({ ...value, [key]: field.type === "integer" || field.type === "number" ? Number(event.target.value) : event.target.value }))}
                    />
                  ))}
                </div>
                {envEditable ? <TextArea
                  label="启动参数"
                  description="每行一个参数，原样插在可执行文件之后（全局选项位置），不经过 shell 解析。Claude 只支持 --flag [值] 形式。"
                  placeholder={"例如：\n--config\nmodel_reasoning_summary=detailed"}
                  rows={3}
                  autoResize
                  spellCheck={false}
                  className="font-mono text-[12px]"
                  value={editLaunchArgs}
                  onChange={(event) => setEditLaunchArgs(event.target.value)}
                /> : null}
                {envEditable ? <EnvRefsEditor rows={editEnvRefs} onChange={setEditEnvRefs} /> : null}
                <div className={cn("flex items-start gap-2.5 rounded-xl border px-3 py-2.5", currentInstance?.health?.healthy ? "border-cx-border bg-cx-success-soft/40" : "border-cx-border bg-cx-bg-subtle")}>
                  <Icon name={currentInstance?.health?.healthy ? "checkCircle" : "info"} size={15} className={cn("mt-0.5 shrink-0", currentInstance?.health?.healthy ? "text-cx-success" : "text-cx-fg-3")} />
                  <div className="min-w-0">
                    <p className="text-[12.5px] font-medium text-cx-fg">{currentInstance?.health?.healthy ? "运行环境健康" : "尚未得到健康结果"}</p>
                    <p className="mt-0.5 text-[12px] leading-5 text-cx-fg-3">
                      {currentInstance?.health?.detail || currentInstance?.auth.detail || "保存后执行探测，以读取版本、登录状态和运行能力。"} {versionCheckDetail(currentInstance?.health?.version_check)}
                    </p>
                  </div>
                </div>
                <div className="flex justify-end gap-2">
                  <Button size="sm" variant="outline" icon="refresh" loading={busyItem === `probe:${currentEngine.engine}`} onClick={() => void probeEngine(currentEngine)}>重新探测</Button>
                  <Button size="sm" variant="primary" icon="check" loading={busyItem === "save-runtime"} onClick={() => void saveRuntime()}>
                    {busyItem === "save-runtime" ? "保存中…" : "保存接入配置"}
                  </Button>
                </div>
              </div>
            </SettingsSection>

            <SettingsSection anchor="agents-models" title="模型" description="按当前运行环境汇总各凭据的模型。★ 设为新对话默认模型；眼睛控制模型是否出现在对话的模型选择器中。">
              <ModelDirectory
                rows={modelRows}
                chatDefault={chatDefault}
                hidden={hiddenModels}
                onSetDefault={selectChatDefault}
                onToggleHidden={(credentialId, modelId, hide) => {
                  if (!setModelHidden(credentialId, modelId, hide)) showFeedback("bad", "模型显隐未保存：共享偏好未能提交，请查看同步提示。");
                }}
              />
            </SettingsSection>

            <SettingsSection>
              <button
                type="button"
                aria-expanded={capabilitiesOpen}
                onClick={() => setCapabilitiesOpen((value) => !value)}
                className="flex w-full items-center gap-3 px-5 py-3.5 text-left outline-none transition-colors hover:bg-cx-hover focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
              >
                <Icon name="network" size={15} className="shrink-0 text-cx-fg-3" />
                <span className="min-w-0 flex-1 text-[13.5px] font-medium text-cx-fg">探测到的能力</span>
                {capabilityEntries.length ? <span className="text-[12px] text-cx-fg-3">{capabilityEntries.length} 项</span> : null}
                <Icon name="chevronDown" size={15} className={cn("shrink-0 text-cx-fg-3 transition-transform", capabilitiesOpen && "rotate-180")} />
              </button>
              <Collapse open={capabilitiesOpen}>
                <div className="grid gap-x-6 gap-y-1 border-t border-cx-border-subtle px-5 py-3 sm:grid-cols-2">
                  {capabilityEntries.length ? capabilityEntries.map(([key, value]) => (
                    <div key={key} className="flex items-center gap-2 py-1">
                      <Icon name={value ? "checkCircle" : "minus"} size={14} className={value ? "shrink-0 text-cx-success" : "shrink-0 text-cx-fg-4"} />
                      <span className="min-w-0 flex-1 text-[12.5px] text-cx-fg-2">{CAPABILITY_LABELS[key] || key}</span>
                      <span className="shrink-0 text-[11.5px] text-cx-fg-4">{typeof value === "boolean" ? value ? "支持" : "未启用" : String(value)}</span>
                    </div>
                  )) : <p className="py-2 text-[12.5px] text-cx-fg-3">探测运行环境后显示能力。</p>}
                </div>
              </Collapse>
            </SettingsSection>
          </div>
        ) : (
          <div className="cx-settings-card hidden @3xl/agents:block">
            <EmptyState icon="bot" title="选择一个 Agent 引擎" description="在左侧列表选择引擎后，这里会显示它的凭据、运行配置与模型。" />
          </div>
        )}
      </div>

      {!taskSettings ? <UsageProviderSettings /> : null}

      <Dialog
        open={Boolean(deleteTarget)}
        onOpenChange={(open) => { if (!open) { setDeleteTarget(null); setDeleteError(""); } }}
        tone="danger"
        title="删除凭据"
        description={`将删除 ${deleteTarget ? credentialName(deleteTarget) : "该凭据"}。该操作不会删除对应的 Agent 引擎。`}
        dismissable={!deleting}
        footer={
          <>
            <Button variant="secondary" size="sm" onClick={() => { setDeleteTarget(null); setDeleteError(""); }} disabled={deleting}>取消</Button>
            <Button variant="danger" size="sm" loading={deleting} onClick={() => void deleteCredential()}>
              {deleteTarget?.usage?.length ? `解除 ${deleteTarget.usage.length} 处引用并删除` : "确认删除"}
            </Button>
          </>
        }
      >
        {deleteError ? (
          <p role="alert" className="rounded-lg bg-cx-danger-soft px-3 py-2 text-[12.5px] text-cx-danger">{deleteError}</p>
        ) : deleteTarget?.usage?.length ? (
          <p className="text-[12.5px] leading-5 text-cx-fg-3">将同步解除以下配置引用：{deleteTarget.usage.map((item) => item.label || item.id).join("、")}。关联对话和产物会保留，相关 Worker 会停用，系统角色会恢复默认接入。</p>
        ) : (
          <p className="text-[12.5px] leading-5 text-cx-fg-3">该凭据当前没有配置引用，可以直接删除。</p>
        )}
      </Dialog>

      <Dialog
        open={testOpen && (testingCredential || Boolean(testingCredential ? liveTestResult : displayedTestResult))}
        onOpenChange={(open) => { if (!open) setTestOpen(false); }}
        title={`${currentEngine?.label || "Agent"} · ${selectedCredential ? credentialName(selectedCredential) : "凭据"}`}
        description={testingCredential ? "正在向模型发起真实请求" : (testingCredential ? liveTestResult : displayedTestResult)?.ok ? "该凭据当前可用" : "该凭据当前不可用"}
        size="lg"
        footer={<Button variant="secondary" size="sm" onClick={() => setTestOpen(false)}>关闭</Button>}
      >
        <ModelTestTerminal testing={testingCredential} result={testingCredential ? liveTestResult : displayedTestResult} title="连通测试终端" />
      </Dialog>
    </div>
  );
}
