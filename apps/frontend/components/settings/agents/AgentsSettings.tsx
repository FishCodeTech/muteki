"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { subscribeUiPreferences } from "@/lib/uiPreferences";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import {
  Badge, Button, Callout, Dialog, EmptyState, IconButton, Menu, MenuItem, MenuSeparator, Select, Switch, type ListOption,
} from "@muteki/ui";
import { AgentVersionSupportSection } from "@/components/worker-settings/AgentVersionSupportSection";
import { UsageProviderSettings } from "@/components/settings/UsageProviderSettings";
import { settingsPageFromPath } from "../catalog";
import { copyToClipboard } from "@/lib/clipboard";
import { cn } from "@/lib/cn";
import { type ChatDefaultModel, readChatDefaultModel, writeChatDefaultModel } from "@/lib/conversationDefaults";
import { credentialForRuntime } from "@/lib/modelReasoning";
import {
  adapterDescriptor, descriptorForEngine, loadProviderDescriptors, providerEngines, readyCatalog,
  runtimeScopesModelCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import { readHiddenModels, setModelHidden, subscribeHiddenModels } from "@/lib/modelVisibility";
import { priceKey, readPricingPrefill, usePricingCatalog, type PriceIdentity } from "@/lib/modelPricing";
import { recordTaskReceipt } from "@/lib/task-center";
import {
  allCredentialModels, normalizeConversationCredential, type ConversationCredentialModel,
} from "@/lib/useConversation";
import {
  type GlobalCredential, type WorkerModelTestResult, type WorkerSettings, apiFetch, deleteCredentialAccount,
  getGlobalCredentials, getWorkerSettings, importHostCodexAuth, importHostWorkerLogin,
  putCredentialAccount, refreshCredentialModels, testCredentialDraft, testGlobalCredential,
} from "@/lib/useRun";
import { AddAgentDialog, type AddAgentInput } from "./AddAgentDialog";
import { CredentialEditor, type CredentialValues, type DraftTestValues } from "./CredentialEditor";
import { EngineList } from "./EngineList";
import { DeleteConfirm, ImportRow, TestSummary } from "./InlinePanels";
import { ModelTable, rowIdentity, type AgentModelRow, type ModelGroup } from "./ModelTable";
import { PricingFooter } from "./PricingFooter";
import { RuntimeConfigPanel, type RuntimeValues } from "./RuntimeConfigPanel";
import {
  asEngine, catalogSummary, credentialLiveStatus, declaredTransportFields, engineCliAdapter, credentialModels, credentialName,
  credentialReady, installedVersion, lastTestAsResult, readSelection, relativeTime, selectionHref, statusTone, versionCheckDetail,
  type AgentsSelection, type AgentsSettingsNavigation, type Engine, type EngineSummary, type Feedback, type RuntimeInstance,
} from "./shared";

const AUTO_REFRESH_MS = 300_000;
const MODEL_REFRESH_CONCURRENCY = 3;
const SPLIT_LAYOUT_WIDTH = 768;

const BACKEND_OPTIONS: ListOption<WorkerSettings["worker_backend"]>[] = [
  { value: "local", label: "本地环境测试" },
  { value: "container", label: "容器环境测试" },
];

type Panel = { kind: "create"; engine: Engine; nonce: number } | { kind: "edit" | "delete" | "import"; id: string; engine: Engine; nonce: number };

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
  const [refreshProgress, setRefreshProgress] = useState("");
  const [refreshReport, setRefreshReport] = useState<Feedback>(null);
  const [lastCheckedAt, setLastCheckedAt] = useState("");
  const [chatDefault, setChatDefault] = useState<ChatDefaultModel | null>(null);
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [busyItem, setBusyItem] = useState("");
  const [testShown, setTestShown] = useState<ReadonlySet<string>>(new Set());
  const [testResults, setTestResults] = useState<Record<string, WorkerModelTestResult>>({});
  const [listQuery, setListQuery] = useState("");
  const [panel, setPanel] = useState<Panel | null>(null);
  const [runtimeOpen, setRuntimeOpen] = useState({ open: false, nonce: 0 });
  const [deleteError, setDeleteError] = useState("");
  const [deleting, setDeleting] = useState(false);
  const [importError, setImportError] = useState("");
  const [focusGroup, setFocusGroup] = useState("");
  const [addDialog, setAddDialog] = useState<{ open: boolean; engine: Engine | null; nonce: number }>({ open: false, engine: null, nonce: 0 });
  const [removeTarget, setRemoveTarget] = useState<RuntimeInstance | null>(null);
  const [removing, setRemoving] = useState(false);
  const panelDirty = useRef(false);
  const [discardGuard, setDiscardGuard] = useState<{ next: Panel | null; after?: () => void } | null>(null);
  const [expandedPrice, setExpandedPrice] = useState("");
  const [extraPrice, setExtraPrice] = useState<PriceIdentity | null>(null);
  const [hiddenModels, setHiddenModels] = useState<ReadonlySet<string>>(() => readHiddenModels());
  const descriptors = readyCatalog(useProviderDescriptors());
  const pricing = usePricingCatalog();
  const authHomeSecret = (engine: Engine) => descriptorForEngine(descriptors, engine)?.credentials.secret_file === "CODEX_AUTH_HOME";
  const hostLoginImport = (engine: Engine) => descriptorForEngine(descriptors, engine)?.login.host_login_import ?? "none";
  const feedbackTimer = useRef<number | null>(null);
  const loadGeneration = useRef(0);
  const backendInitialized = useRef(false);
  const refreshInFlight = useRef(false);
  const lastAutomaticRefreshAt = useRef(Date.now());

  useEffect(() => subscribeUiPreferences(() => setChatDefault(readChatDefaultModel())), []);
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
    document.title = "Agents — Muteki";
    return () => { document.title = old; };
  }, [taskSettings]);

  const loadData = useCallback(async (silent = false) => {
    const generation = ++loadGeneration.current;
    if (!silent) setLoading(true);
    try {
      const [rows, runtimePayload, settings] = await Promise.all([
        getGlobalCredentials(backend),
        apiFetch(`/api/agent-runtimes?transport=${taskSettings ? "cli" : "structured"}`).then(async (response) => {
          if (!response.ok) throw new Error(`运行环境读取失败（HTTP ${response.status}）`);
          const payload = await response.json();
          if (!Array.isArray(payload.instances)) throw new Error("运行环境目录返回了无效响应");
          return payload;
        }),
        getWorkerSettings().catch(() => null),
        loadProviderDescriptors({ fresh: true }),
      ]);
      if (generation !== loadGeneration.current) return false;
      setCredentials(Array.isArray(rows) ? rows : []);
      setInstances(Array.isArray(runtimePayload.instances) ? runtimePayload.instances : []);
      if (taskSettings && !backendInitialized.current && settings?.worker_backend) { setBackend(settings.worker_backend); backendInitialized.current = true; }
      if (!taskSettings) setChatDefault(readChatDefaultModel());
      const probedAt = String(runtimePayload.probed_at || "");
      const checkedAt = probedAt ? new Date(probedAt) : null;
      if (checkedAt && !Number.isNaN(checkedAt.getTime())) {
        setLastCheckedAt(checkedAt.toLocaleTimeString("zh-CN", { hour: "2-digit", minute: "2-digit", second: "2-digit" }));
      }
      return true;
    } catch (error) { showFeedback("bad", `加载 Agent 数据失败：${error instanceof Error ? error.message : String(error)}`); return false; }
    finally { if (generation === loadGeneration.current) setLoading(false); }
  }, [backend, showFeedback, taskSettings]);
  const loadedWith = useRef<typeof loadData | null>(null);
  useEffect(() => { const silent = loadedWith.current === loadData; loadedWith.current = loadData; void loadData(silent); }, [loadData]);

  const engines = useMemo<EngineSummary[]>(() => providerEngines(descriptors).map((engine) => {
    const identity = descriptorForEngine(descriptors, engine)!.identity;
    const label = identity.display_name || engine;
    if (identity.support_status !== "supported") {
      return { engine, label, cliAdapterId: identity.cli_adapter_id, credentials: [], instances: [], primaryCredential: null, primaryInstance: null, enabled: false, version: "", readyCredentialCount: 0, modelCount: 0, environmentCount: 0, statusKind: "disabled", statusText: "暂不支持", supportStatus: "temporarily_disabled", disabledReason: "该引擎暂不支持。", added: taskSettings, defaultListed: false };
    }
    const defaultListed = taskSettings || Boolean(identity.default_enabled);
    const engineCredentials = credentials.filter((item) => asEngine(descriptors, item.engine) === engine);
    const engineInstances = instances.filter((item) => asEngine(descriptors, item.engine) === engine
      && (defaultListed || item.configured || item.adapter_id !== identity.default_adapter_id));
    const added = defaultListed || engineInstances.some((item) => item.configured);
    const primaryCredential = engineCredentials.find((item) => item.source === "stored" && credentialReady(item)) || engineCredentials.find(credentialReady) || engineCredentials[0] || null;
    const primaryInstance = (!taskSettings && (engineInstances.find((item) => item.adapter_id === identity.default_adapter_id && item.instance_id === "default")
      || engineInstances.find((item) => item.adapter_id === identity.default_adapter_id)))
      || engineInstances.find((item) => item.configured) || engineInstances.find((item) => item.discovered) || engineInstances[0] || null;
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
    return { engine, label, cliAdapterId: identity.cli_adapter_id, credentials: engineCredentials, instances: engineInstances, primaryCredential, primaryInstance, enabled, version: primaryInstance?.health?.runtime_version || "", readyCredentialCount, modelCount, environmentCount: engineInstances.length, statusKind, statusText, supportStatus: "supported", disabledReason: "", added, defaultListed };
  }), [credentials, descriptors, instances, taskSettings]);
  const listedEngines = useMemo(() => engines.filter((item) => item.added), [engines]);

  const [localSelection, setLocalSelection] = useState<AgentsSelection>({ engine: null, instanceId: "", credentialId: "" });
  const taskSelectionInitialized = useRef(false);
  useEffect(() => {
    if (!taskSettings || navigation || taskSelectionInitialized.current) return;
    const first = engines.find((item) => item.supportStatus === "supported");
    if (!first) return;
    taskSelectionInitialized.current = true;
    setLocalSelection({ engine: first.engine, instanceId: "", credentialId: "" });
  }, [engines, navigation, taskSettings]);
  const selection = useMemo<AgentsSelection>(
    () => navigation ? readSelection(navigation.searchParams, descriptors) : localSelection,
    [descriptors, navigation, localSelection],
  );
  const applySelection = useCallback((next: AgentsSelection, mode: "push" | "replace") => {
    if (navigation) {
      if (settingsPageFromPath(navigation.pathname) !== "agents") return;
      const href = selectionHref(navigation.pathname, navigation.searchParams, next, descriptors);
      navigation.router[mode](href + (navigation.hash || ""), { scroll: false });
    } else {
      setLocalSelection(next);
    }
  }, [descriptors, navigation]);

  const prefill = taskSettings ? null : readPricingPrefill(navigation?.searchParams);
  const prefillKey = prefill ? priceKey(prefill) : "";
  const prefillStage = useRef("");
  const rootRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (taskSettings || !navigation || loading || selection.engine || prefillKey) return;
    if ((rootRef.current?.offsetWidth ?? 0) < SPLIT_LAYOUT_WIDTH) return;
    const first = listedEngines.find((item) => item.supportStatus === "supported" && item.enabled) || listedEngines.find((item) => item.supportStatus === "supported");
    if (first) applySelection({ engine: first.engine, instanceId: "", credentialId: "" }, "replace");
  }, [applySelection, listedEngines, loading, navigation, prefillKey, selection.engine, taskSettings]);

  const currentEngine = useMemo(() => {
    if (!selection.engine) return null;
    const found = listedEngines.find((item) => item.engine === selection.engine) || null;
    return found && found.supportStatus === "supported" ? found : null;
  }, [listedEngines, selection.engine]);
  const currentInstance = useMemo(() => {
    if (!currentEngine) return null;
    if (selection.instanceId) {
      return currentEngine.instances.find((item) => item.key === selection.instanceId)
        || currentEngine.instances.find((item) => item.instance_id === selection.instanceId)
        || currentEngine.primaryInstance;
    }
    return currentEngine.primaryInstance;
  }, [currentEngine, selection.instanceId]);
  const currentEngineRef = useRef(currentEngine);
  const currentInstanceRef = useRef(currentInstance);
  currentEngineRef.current = currentEngine;
  currentInstanceRef.current = currentInstance;
  const runtimeKey = currentEngine ? currentInstance?.key || `${engineCliAdapter(descriptors, currentEngine.engine)}:default` : "";

  const scopeCredential = useCallback((item: GlobalCredential): GlobalCredential => {
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
      default_model: [catalog?.default_model, scoped.default_model].find(model => model && available.includes(model)) || "",
      model_catalog: catalog || (scopedCatalog ? undefined : item.model_catalog),
      last_test: item.last_tests ? item.last_tests[currentInstance.key] || null : item.last_test,
    };
  }, [currentInstance, descriptors]);

  const groups = useMemo<ModelGroup[]>(() => {
    if (!currentEngine || !runtimeKey) return [];
    return currentEngine.credentials.map((item) => {
      const scoped = scopeCredential(item);
      const normalized = normalizeConversationCredential(item);
      const runtimeCredential = normalized ? credentialForRuntime(normalized, runtimeKey, runtimeScopesModelCatalog(descriptors, runtimeKey)) : null;
      const discoveredCaps = new Map(
        (scoped.model_catalog?.discovered_models || [])
          .filter((entry) => entry?.id)
          .map((entry) => [String(entry.id), entry]),
      );
      const rows: AgentModelRow[] = runtimeCredential
        ? allCredentialModels(runtimeCredential).map((model: ConversationCredentialModel) => {
          const explicitCap = item.model_capabilities?.[model.id];
          const discoveredCap = discoveredCaps.get(model.id);
          const isPiCustom = runtimeCredential.engine === "pi" && item.connection === "custom_endpoint";
          const contextWindow = isPiCustom
            ? (explicitCap?.context_window ?? discoveredCap?.context_window)
            : undefined;
          const maxOutputTokens = isPiCustom
            ? (explicitCap?.max_output_tokens ?? discoveredCap?.max_output_tokens)
            : undefined;
          return {
            key: `${item.id}${model.id}`,
            credentialId: item.id,
            engine: runtimeCredential.engine,
            providerId: runtimeCredential.provider,
            credentialName: credentialName(item),
            model,
            verified: runtimeCredential.models.some((probed) => probed.id === model.id),
            ...(typeof contextWindow === "number" && contextWindow > 0 ? { contextWindow } : {}),
            ...(typeof maxOutputTokens === "number" && maxOutputTokens > 0 ? { maxOutputTokens } : {}),
          };
        })
        : [];
      const testing = busyItem === `test:${item.id}`;
      const result = testResults[item.id] || lastTestAsResult(scoped, currentEngine.engine);
      const status = credentialLiveStatus(scoped, result, testing);
      const refreshedAgo = relativeTime(scoped.model_catalog?.refreshed_at);
      const system = item.source === "system";
      return {
        id: item.id,
        name: credentialName(item),
        typeLabel: system ? "宿主登录" : item.connection === "custom_endpoint" ? "自定义端点" : "官方账号",
        statusKind: status.kind,
        statusText: status.text,
        summary: `${rows.length} 个模型${refreshedAgo ? ` · ${refreshedAgo}更新` : ""}`,
        detail: [
          system ? "系统检测到的宿主登录状态；容器 Worker 需导入为独立凭据。" : `凭据 ID：${item.account_id || item.id.replace(/^account:/, "")}`,
          catalogSummary(scoped.model_catalog),
          result ? `最近连通测试：${result.ok ? "可用" : "不可用"}${result.detail ? `（${result.detail}）` : ""}` : "",
        ].filter(Boolean).join("\n"),
        rows,
        ready: Boolean(credentialReady(item)),
        isDefaultSource: chatDefault?.credentialId === item.id,
        testing,
        refreshing: busyItem === `refresh:${item.id}`,
        canEdit: !system,
        canImport: system && hostLoginImport(currentEngine.engine) !== "none" && Boolean(credentialReady(item)),
        canDelete: !system && !item.sharing?.readonly,
        isSystem: system,
        hasTestResult: Boolean(result),
      };
    });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [busyItem, chatDefault, currentEngine, descriptors, runtimeKey, scopeCredential, testResults]);

  const knownModels = useMemo(() => new Set(credentials.flatMap((item) => {
    const normalized = normalizeConversationCredential(item);
    return normalized ? allCredentialModels(normalized).map((model) => `${normalized.engine}\u0000${model.id}`) : [];
  })), [credentials]);
  const pricingEngines = useMemo(() => engines.filter((item) => item.supportStatus === "supported").map((item) => ({ value: item.engine, label: item.label })), [engines]);

  useEffect(() => {
    if (!prefill || loading) return;
    const engine = asEngine(descriptors, prefill.engine);
    if (engine && selection.engine !== engine) {
      if (prefillStage.current === `${prefillKey}:select`) return;
      prefillStage.current = `${prefillKey}:select`;
      applySelection({ engine, instanceId: "", credentialId: "" }, "replace");
      return;
    }
    if (prefillStage.current === prefillKey) return;
    prefillStage.current = prefillKey;
    const row = groups.flatMap((group) => group.rows).find((item) => item.engine === prefill.engine && item.model.id === prefill.model && (!prefill.credential_id || item.credentialId === prefill.credential_id));
    if (row) setExpandedPrice(priceKey(rowIdentity(row))); else setExtraPrice(prefill);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [applySelection, descriptors, groups, loading, prefillKey, selection.engine]);

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
    const item = listedEngines.find((entry) => entry.engine === engine);
    if (!item || item.supportStatus !== "supported") return;
    if (selection.engine === engine && selection.instanceId === instanceId) return;
    requestPanel(null, () => {
      setRuntimeOpen((current) => ({ ...current, open: false }));
      setExpandedPrice("");
      applySelection({ engine, instanceId, credentialId: "" }, "push");
    });
  };

  const refreshAll = async () => {
    if (refreshInFlight.current || busyItem) return;
    refreshInFlight.current = true;
    setRefreshing(true);
    setRefreshReport(null);
    setRefreshProgress("正在探测全部运行环境…");
    const failures: string[] = [];
    let completed = 0;
    let succeeded = 0;
    let skipped = 0;
    try {
      try {
        const response = await apiFetch("/api/agent-runtimes/refresh", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ transport_kind: taskSettings ? "cli" : "structured" }) });
        await assertRuntimeResponse(response, "全部 Agent 探测");
      } catch (error) {
        if (error instanceof Error && error.name === "RuntimeOperationPending") throw error;
        failures.push(`运行环境：${error instanceof Error ? error.message : String(error)}`);
      }
      setRefreshProgress("正在读取最新凭据…");
      const [credentialResponse, runtimeResponse, catalog] = await Promise.all([
        apiFetch(`/api/settings/credentials?environment=${backend}&fresh=true`),
        apiFetch(`/api/agent-runtimes?transport=${taskSettings ? "cli" : "structured"}`),
        loadProviderDescriptors({ fresh: true }),
      ]);
      const [credentialPayload, runtimePayload] = await Promise.all([credentialResponse.json(), runtimeResponse.json()]);
      if (!credentialResponse.ok) throw new Error(credentialPayload.detail?.message || credentialPayload.detail || `最新凭据目录读取失败（HTTP ${credentialResponse.status}）`);
      if (!Array.isArray(credentialPayload.credentials)) throw new Error("最新凭据目录响应无效");
      if (!runtimeResponse.ok) throw new Error(runtimePayload.detail?.message || runtimePayload.detail || `最新运行环境读取失败（HTTP ${runtimeResponse.status}）`);
      if (!Array.isArray(runtimePayload.instances)) throw new Error("最新运行环境目录响应无效");
      const freshCredentials = credentialPayload.credentials as GlobalCredential[];
      const freshInstances = runtimePayload.instances as RuntimeInstance[];
      const jobs: { credential: GlobalCredential; instance: RuntimeInstance; engine: Engine }[] = [];
      for (const instance of freshInstances) {
        const engine = asEngine(catalog, instance.engine);
        if (!engine || descriptorForEngine(catalog, engine)?.identity.support_status !== "supported") continue;
        if (!taskSettings && instance.discovered && !instance.enabled) continue;
        for (const credential of freshCredentials) {
          if (asEngine(catalog, credential.engine) !== engine || !credentialReady(credential)) continue;
          if (!instance.health?.healthy && credential.connection !== "custom_endpoint") {
            skipped += 1;
            continue;
          }
          jobs.push({ credential, instance, engine });
        }
      }
      setRefreshProgress(`正在更新模型 0 / ${jobs.length}`);
      let nextJob = 0;
      const worker = async () => {
        while (nextJob < jobs.length) {
          const { credential, instance, engine } = jobs[nextJob++];
          try {
            const result = await refreshCredentialModels(credential.id, {
              engine,
              backend: credential.source === "system" ? "local" : backend,
              runtime_instance: instance.key,
            });
            if (result.ok) succeeded += 1;
            else failures.push(`${descriptorForEngine(catalog, engine)?.identity.display_name || engine} · ${credentialName(credential)} · ${instance.key}：${result.detail || "未返回模型"}`);
          } catch (error) {
            failures.push(`${credentialName(credential)} · ${instance.key}：${error instanceof Error ? error.message : String(error)}`);
          } finally {
            completed += 1;
            setRefreshProgress(`正在更新模型 ${completed} / ${jobs.length}`);
          }
        }
      };
      await Promise.all(Array.from({ length: Math.min(MODEL_REFRESH_CONCURRENCY, jobs.length) }, worker));
      const detail = `全部 Agent 探测完成，已更新 ${succeeded} / ${jobs.length} 项模型目录。${skipped ? `另有 ${skipped} 项因运行环境不可用而跳过。` : ""}${failures.length ? `刷新失败：${failures.join("；")}` : jobs.length ? "" : "请先登录或添加可用凭据。"}`;
      setRefreshReport({ kind: failures.length || skipped || !jobs.length ? "warn" : "ok", detail });
    } catch (error) {
      setRefreshReport({ kind: error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", detail: `探测未完成：${error instanceof Error ? error.message : String(error)}` });
    } finally {
      setRefreshProgress("正在载入探测结果…");
      await loadData(true);
      lastAutomaticRefreshAt.current = Date.now();
      refreshInFlight.current = false;
      setRefreshing(false);
      setRefreshProgress("");
    }
  };

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

  const pendingAware = (error: unknown, prefix: string) => {
    const pending = error instanceof Error && error.name === "RuntimeOperationPending";
    showFeedback(pending ? "warn" : "bad", pending ? (error as Error).message : `${prefix}：${error instanceof Error ? error.message : String(error)}`);
  };
  const probeEngine = async (item: EngineSummary) => {
    if (item.supportStatus !== "supported" || refreshInFlight.current) return;
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
    } catch (error) { pendingAware(error, "探测失败"); }
    finally { setBusyItem(""); }
  };
  const instanceName = (item: EngineSummary, inst: RuntimeInstance | null) => {
    const own = inst?.label || inst?.instance_id || "";
    return inst && item.instances.length > 1 && own !== item.label ? `${item.label} · ${own}` : item.label;
  };
  const toggleInstance = async (item: EngineSummary, inst: RuntimeInstance | null) => {
    if (item.supportStatus !== "supported") return;
    const enabled = inst ? inst.enabled : item.enabled;
    const name = instanceName(item, inst);
    setBusyItem(`toggle:${inst?.key || item.engine}`);
    try {
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ adapter_id: inst?.adapter_id || engineCliAdapter(descriptors, item.engine), instance_id: inst?.instance_id || "default", expected_revision: inst?.updated_at || "", enabled: !enabled }) });
      await assertRuntimeResponse(response, `${enabled ? "停用" : "启用"} ${name}`);
      if (await loadData(true)) showFeedback("ok", `${name} 已${enabled ? "停用" : "启用"}。`);
    } catch (error) { pendingAware(error, "操作失败"); }
    finally { setBusyItem(""); }
  };
  const toggleEngine = (item: EngineSummary) => toggleInstance(item, item.primaryInstance);

  const openAddDialog = (engine: Engine | null = null) => setAddDialog({ open: true, engine, nonce: Date.now() });
  const addAgent = async ({ engine, adapterId, instanceId, label }: AddAgentInput) => {
    const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ adapter_id: adapterId, instance_id: instanceId, label, enabled: true, expected_revision: "" }) });
    try {
      await assertRuntimeResponse(response, `添加 ${label}`);
    } catch (error) {
      if (!(error instanceof Error && error.name === "RuntimeOperationPending")) throw error;
    }
    const key = `${adapterId}:${instanceId}`;
    setAddDialog((current) => ({ ...current, open: false }));
    requestPanel(null);
    setRuntimeOpen((current) => ({ ...current, open: false }));
    applySelection({ engine, instanceId: key, credentialId: "" }, "push");
    await loadData(true);
    showFeedback("ok", `已添加 ${label}，正在探测本机安装…`);
    setBusyItem(`probe:${engine}`);
    try {
      const probe = await apiFetch(`/api/agent-runtimes/${encodeURIComponent(key)}/probe`, { method: "POST" });
      await assertRuntimeResponse(probe, `探测 ${label}`);
      if (await loadData(true)) showFeedback("ok", `${label} 已添加并完成探测。`);
    } catch (error) { pendingAware(error, `${label} 已添加，但探测失败`); void loadData(true); }
    finally { setBusyItem(""); }
  };

  const removalOf = (item: EngineSummary, inst: RuntimeInstance | null) => {
    if (taskSettings || !inst?.configured) return null;
    const defaultAdapter = descriptorForEngine(descriptors, item.engine)?.identity.default_adapter_id;
    const restores = inst.instance_id === "default" && (item.defaultListed || inst.adapter_id !== defaultAdapter);
    const last = !restores && !item.defaultListed && !item.instances.some((row) => row.configured && row.key !== inst.key);
    return { restores, last };
  };
  const removeInstance = async (inst: RuntimeInstance) => {
    const item = engines.find((entry) => entry.engine === asEngine(descriptors, inst.engine));
    const removal = item ? removalOf(item, inst) : null;
    if (!item || !removal) return;
    const name = instanceName(item, inst);
    setRemoving(true);
    try {
      const response = await apiFetch(`/api/agent-runtimes/${encodeURIComponent(inst.key)}`, { method: "DELETE" });
      await assertRuntimeResponse(response, `${removal.restores ? "恢复" : "移除"} ${name}`);
      setRemoveTarget(null);
      setRuntimeOpen((current) => ({ ...current, open: false }));
      if (removal.last) applySelection({ engine: null, instanceId: "", credentialId: "" }, "replace");
      else if (selection.instanceId) applySelection({ engine: item.engine, instanceId: "", credentialId: "" }, "replace");
      if (await loadData(true)) {
        showFeedback("ok", removal.restores ? `${name} 已恢复默认设置。`
          : removal.last ? `${item.label} 已移除，可随时通过“添加 Agent”重新添加。` : `${name} 已移除。`);
      }
    } catch (error) { setRemoveTarget(null); pendingAware(error, "移除失败"); }
    finally { setRemoving(false); }
  };

  const findCredential = (id: string) => currentEngine?.credentials.find((item) => item.id === id) || null;
  const refreshGroupModels = async (id: string) => {
    const item = findCredential(id);
    if (!currentEngine || !item || refreshInFlight.current) return;
    const system = item.source === "system";
    setBusyItem(`refresh:${id}`);
    try {
      const result = await refreshCredentialModels(id, {
        engine: currentEngine.engine,
        connection: system || item.connection !== "custom_endpoint" ? "official" : "custom_endpoint",
        base_url: !system && item.connection === "custom_endpoint" ? item.base_url || "" : "",
        backend: system ? "local" : backend,
        runtime_instance: runtimeKey,
      });
      await loadData(true);
      showFeedback(result.ok ? "ok" : "bad", result.ok ? result.detail || `${credentialName(item)}：已更新 ${result.models.length} 个模型。` : `更新模型失败：${result.detail || "服务未返回模型"}`);
    } catch (error) { showFeedback("bad", `更新模型失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const testGroup = async (id: string) => {
    const item = findCredential(id);
    if (!currentEngine || !item) return;
    const model = scopeCredential(item).default_model || credentialModels(scopeCredential(item))[0] || "";
    viewTest(id);
    setBusyItem(`test:${id}`);
    setTestResults((current) => { const next = { ...current }; delete next[id]; return next; });
    try {
      const result = await testGlobalCredential(id, currentEngine.engine, backend, model, runtimeKey);
      setTestResults((current) => ({ ...current, [id]: result }));
      await loadData(true);
    } catch (error) {
      const detail = `测试异常：${error instanceof Error ? error.message : String(error)}`;
      setTestResults((current) => ({ ...current, [id]: { ok: false, detail, model, engine: currentEngine.engine, backend, logs: [{ stream: "error", message: detail, elapsed_ms: 0 }] } }));
    } finally { setBusyItem(""); }
  };
  const viewTest = (id: string) => setTestShown((current) => new Set(current).add(id));
  const hideTest = (id: string) => setTestShown((current) => { const next = new Set(current); next.delete(id); return next; });

  const commitPanel = (next: Panel | null, after?: () => void) => {
    panelDirty.current = false;
    setDeleteError(""); setImportError("");
    setPanel(next);
    after?.();
  };
  const requestPanel = (next: Panel | null, after?: () => void) => {
    if (panelDirty.current) { setDiscardGuard({ next, after }); return false; }
    commitPanel(next, after);
    return true;
  };
  const closePanel = () => {
    panelDirty.current = false;
    setPanel(null);
    if (selection.credentialId && currentEngine) applySelection({ engine: currentEngine.engine, instanceId: selection.instanceId, credentialId: "" }, "replace");
  };
  useEffect(() => {
    if (panel && panel.engine !== currentEngine?.engine) { panelDirty.current = false; setPanel(null); }
  }, [currentEngine?.engine, panel]);
  const openCreate = () => { if (currentEngine) requestPanel({ kind: "create", engine: currentEngine.engine, nonce: Date.now() }); };
  const openFor = (kind: "edit" | "delete" | "import") => (id: string) => { if (currentEngine) requestPanel({ kind, id, engine: currentEngine.engine, nonce: Date.now() }); };
  const urlCredentialHandled = useRef("");
  useEffect(() => {
    if (!selection.credentialId || urlCredentialHandled.current === selection.credentialId || !currentEngine) return;
    const item = currentEngine.credentials.find((row) => row.id === selection.credentialId && row.source === "stored");
    if (!item) return;
    urlCredentialHandled.current = selection.credentialId;
    setPanel({ kind: "edit", id: item.id, engine: currentEngine.engine, nonce: Date.now() });
  }, [currentEngine, selection.credentialId]);

  const submitCredential = async (values: CredentialValues, baseRevision?: string) => {
    if (!panel || (panel.kind !== "create" && panel.kind !== "edit")) return;
    const engine = panel.engine;
    const label = engines.find((item) => item.engine === engine)?.label || engine;
    const custom = values.connection === "custom_endpoint";
    const secretPatch = values.secret ? (!custom && authHomeSecret(engine) ? { codex_auth_json: values.secret } : { secret: values.secret }) : {};
    const common = {
      engine: custom ? "api" : engine, worker_engine: engine, target_engine: custom ? engine : undefined, connection: values.connection,
      provider: custom ? values.provider : undefined, base_url: custom ? values.baseUrl : "", target_model: values.defaultModel, models: values.models,
      ...(engine === "pi" && custom && values.modelCapabilities !== undefined ? { model_capabilities: values.modelCapabilities } : {}),
      ...secretPatch,
    };
    if (panel.kind === "edit") {
      const item = findCredential(panel.id);
      if (!item) throw new Error("该凭据已不存在。");
      const saved = await putCredentialAccount(item.account_id || item.id.replace(/^account:/, ""), { expected_revision: baseRevision, ...common });
      if (saved?.revision !== baseRevision && !saved?.test_status_preserved) {
        setTestResults((current) => { const next = { ...current }; delete next[item.id]; return next; });
        hideTest(item.id);
      }
      await loadData(true); onCredentialsChanged?.("save");
      showFeedback("ok", `${credentialName(item)} 已保存。`);
      return;
    }
    const id = values.accountId;
    if (!id) throw new Error("请填写凭据 ID。");
    if (credentials.some((item) => item.account_id === id || item.id === `account:${id}`)) throw new Error("该凭据 ID 已存在，请使用不同 ID。");
    await putCredentialAccount(id, { create_only: true, ...common });
    await loadData(true); onCredentialsChanged?.("create");
    setFocusGroup(`account:${id}`);
    if (values.models.length) { showFeedback("ok", `${label} 已新增接入方式 ${id}。`); return; }
    setBusyItem(`refresh:account:${id}`);
    void refreshCredentialModels(`account:${id}`, { engine, backend, runtime_instance: runtimeKey }).then(async (result) => {
      await loadData(true);
      showFeedback(result.ok ? "ok" : "warn", result.ok ? `${label} 已添加 ${id}，读取到 ${result.models.length} 个模型。` : `${label} 已添加 ${id}，但读取模型失败：${result.detail || "服务未返回模型"}`);
    }).finally(() => setBusyItem(""));
  };
  const refreshEditorModels = async (values: { connection: "official" | "custom_endpoint"; baseUrl: string; secret: string }) => {
    const item = panel?.kind === "edit" ? findCredential(panel.id) : null;
    if (!currentEngine || (panel?.kind === "edit" && !item)) return { ok: false, models: [], detail: "该凭据已不存在。" };
    const result = await refreshCredentialModels(item?.id ?? "account:draft-check", {
      engine: currentEngine.engine, connection: values.connection, base_url: values.connection === "custom_endpoint" ? values.baseUrl : "",
      secret: values.secret, backend, runtime_instance: runtimeKey,
    });
    if (item) void loadData(true);
    return {
      ok: result.ok,
      models: result.models,
      discoveredModels: result.discoveredModels,
      detail: result.ok ? result.detail || `已获取 ${result.models.length} 个模型。` : result.detail,
    };
  };
  const testEditorDraft = (values: DraftTestValues) => {
    const engine = currentEngine?.engine || "";
    const official = values.connection === "official";
    return testCredentialDraft({
      engine, backend, connection: values.connection, model: values.model, provider: values.provider,
      runtime_instance: runtimeKey,
      base_url: official ? "" : values.baseUrl,
      ...(engine === "pi" && !official && values.modelCapabilities ? { model_capabilities: values.modelCapabilities } : {}),
      ...(official && authHomeSecret(engine) ? { codex_auth_json: values.secret } : { secret: values.secret }),
    });
  };
  const deleteCredential = async (item: GlobalCredential) => {
    const id = item.account_id || item.id.replace(/^account:/, "");
    setDeleting(true); setDeleteError("");
    try {
      const deleted = await deleteCredentialAccount(id, true);
      if (!deleted) throw new Error("服务未确认删除成功，引用保持原状，请刷新后重试。");
      setPanel(null); hideTest(item.id);
      await loadData(true); onCredentialsChanged?.("delete");
      showFeedback("ok", `凭据 ${id} 及其配置引用已删除。`);
    }
    catch (error) { setDeleteError(error instanceof Error ? error.message : String(error)); }
    finally { setDeleting(false); }
  };
  const importSystemLogin = async (id: string) => {
    if (!currentEngine) return;
    setBusyItem("import-system"); setImportError("");
    try {
      const engine = currentEngine.engine;
      const hostImport = hostLoginImport(engine);
      const result = hostImport === "codex_auth_json" ? await importHostCodexAuth(id) : hostImport !== "none" ? await importHostWorkerLogin(id, engine) : { ok: false, detail: "该引擎暂不支持导入宿主登录。" };
      if (!result.ok) throw new Error(result.detail || "请检查宿主 CLI 登录状态");
      setPanel(null); setFocusGroup(`account:${id}`);
      await loadData(true); onCredentialsChanged?.("import");
      showFeedback("ok", `宿主登录已导入为 ${id}。`);
    } catch (error) { setImportError(error instanceof Error ? error.message : String(error)); }
    finally { setBusyItem(""); }
  };

  const currentAdapter = currentInstance ? adapterDescriptor(descriptors, currentInstance.adapter_id) : undefined;
  const currentDescriptor = currentEngine ? descriptorForEngine(descriptors, currentEngine.engine) : undefined;
  const currentTransportFields = currentInstance ? currentInstance.config_schema?.properties?.transport?.properties ?? declaredTransportFields(descriptors, currentInstance.adapter_id) : {};
  const runtimeAccessModes = (currentInstance as (RuntimeInstance & { access_modes?: string[] }) | null)?.access_modes ?? [];
  const effectiveAdapter = currentAdapter ? {
    ...currentAdapter,
    capabilities: { ...currentAdapter.capabilities, access_modes: runtimeAccessModes },
  } : undefined;
  const connectionOptions: ListOption[] = !taskSettings ? (currentDescriptor?.adapters || [])
    .filter((item) => item.role === "default" || item.role === "variant")
    .map((item) => ({
      value: item.adapter_id,
      label: `${item.transport_kind.toUpperCase()}${item.role === "default" ? "（默认）" : ""}`,
      description: item.notes || item.adapter_id,
      disabled: !currentEngine?.instances.some((instance) => instance.adapter_id === item.adapter_id
        && Boolean((instance as RuntimeInstance & { access_modes?: string[] }).access_modes?.length)),
    })) : [];
  const accessModeRows = runtimeAccessModes;
  const envEditable = Boolean(currentAdapter && currentAdapter.role !== "cli");
  const saveRuntime = async (values: RuntimeValues, baseRevision: string) => {
    if (!currentEngine) return;
    try {
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ expected_revision: baseRevision, adapter_id: currentInstance?.adapter_id || engineCliAdapter(descriptors, currentEngine.engine), instance_id: currentInstance?.instance_id || "default", label: currentInstance?.label || currentEngine.label, ...values, enabled: currentInstance?.enabled ?? currentEngine.enabled }) });
      const payload = await assertRuntimeResponse(response, `保存 ${currentEngine.label} 接入配置`);
      if (await loadData(true)) showFeedback("ok", `${currentEngine.label} 的接入与环境配置已保存。`);
      return String(payload.receipt?.output?.instance?.updated_at || baseRevision);
    } catch (error) {
      if (error instanceof Error && error.name === "RuntimeOperationPending") { showFeedback("warn", error.message); return; }
      throw error;
    }
  };

  const selectChatDefault = (credentialId: string, modelId: string) => {
    if (!credentialId || !modelId) return;
    const next = { credentialId, modelId };
    if (!writeChatDefaultModel(next)) { showFeedback("bad", "默认模型未保存：共享偏好未能提交，请查看同步提示。"); return; }
    setChatDefault(next);
  };
  const defaultLabel = useMemo(() => {
    if (!chatDefault) return "";
    const credential = credentials.find((item) => item.id === chatDefault.credentialId);
    const engine = credential ? engines.find((item) => item.engine === asEngine(descriptors, credential.engine)) : null;
    return `${engine?.label || credential?.engine || ""} · ${credential ? credentialName(credential) : chatDefault.credentialId} / ${chatDefault.modelId}`;
  }, [chatDefault, credentials, descriptors, engines]);

  const probing = Boolean(currentEngine && busyItem === `probe:${currentEngine.engine}`);
  const currentRemoval = currentEngine ? removalOf(currentEngine, currentInstance) : null;
  const pendingRemoval = removeTarget ? (() => {
    const item = engines.find((entry) => entry.engine === asEngine(descriptors, removeTarget.engine));
    return item ? { item, name: instanceName(item, removeTarget), removal: removalOf(item, removeTarget) } : null;
  })() : null;
  const authJson = currentEngine ? authHomeSecret(currentEngine.engine) : false;
  const createPanel = currentEngine && panel?.kind === "create" ? (
    <CredentialEditor
      key={panel.nonce} mode="create" engine={currentEngine.engine} engineLabel={currentEngine.label} credential={null} authJson={authJson}
      onClose={closePanel} onSubmit={submitCredential} onRefreshModels={refreshEditorModels} onTestDraft={testEditorDraft} onDirtyChange={(dirty) => { panelDirty.current = dirty; }}
    />
  ) : null;
  const renderExtra = (group: ModelGroup) => {
    const item = findCredential(group.id);
    if (!currentEngine || !item) return null;
    const active = panel && panel.kind !== "create" && panel.id === group.id ? panel : null;
    const result = testResults[item.id] || lastTestAsResult(scopeCredential(item), currentEngine.engine);
    return (
      <>
        {active?.kind === "edit" ? (
          <CredentialEditor
            key={active.nonce} mode="edit" engine={currentEngine.engine} engineLabel={currentEngine.label} credential={scopeCredential(item)} authJson={authJson}
            onClose={closePanel} onSubmit={submitCredential} onRefreshModels={refreshEditorModels} onTestDraft={testEditorDraft} onDirtyChange={(dirty) => { panelDirty.current = dirty; }}
            onDelete={() => openFor("delete")(item.id)}
          />
        ) : null}
        {active?.kind === "delete" ? <DeleteConfirm key={active.nonce} credential={item} busy={deleting} error={deleteError} onCancel={closePanel} onConfirm={() => void deleteCredential(item)} /> : null}
        {active?.kind === "import" ? (
          <ImportRow key={active.nonce} initialId={`${currentEngine.engine}-account`} busy={busyItem === "import-system"} error={importError} onCancel={closePanel} onImport={(id) => void importSystemLogin(id)} />
        ) : null}
        {testShown.has(item.id) && (result || group.testing) ? <TestSummary result={group.testing ? null : result} testing={group.testing} onClose={() => hideTest(item.id)} /> : null}
      </>
    );
  };
  const emptyGroups = currentEngine && !groups.length ? (
    <div className="cx-settings-card"><EmptyState icon="plug" title="尚未添加接入方式" description="添加官方账号或自定义端点后，这里会列出它的模型和价格。" action={<Button size="sm" variant="primary" icon="plus" onClick={openCreate}>新增接入方式</Button>} /></div>
  ) : null;

  return (
    <div ref={rootRef} className="agents-settings @container/agents flex w-full min-w-0 flex-col gap-4" aria-busy={loading || refreshing}>
      <div className={cn("flex flex-wrap items-center gap-3", taskSettings ? "justify-end" : "justify-between")}>
        {!taskSettings ? (
          <div data-settings-anchor="agents-default-model" id="setting-agents-default-model" className="flex min-w-0 items-center gap-2 text-cx-sm">
            <Icon name="star" size={14} filled={Boolean(chatDefault)} className={chatDefault ? "shrink-0 text-cx-warning" : "shrink-0 text-cx-fg-4"} />
            <span className="shrink-0 text-cx-fg-3">新对话默认</span>
            {chatDefault ? (
              <>
                <code className="min-w-0 truncate rounded-md bg-cx-code px-1.5 py-0.5 text-cx-xs text-cx-fg" title={defaultLabel}>{defaultLabel}</code>
                <IconButton icon="x" label="清除新对话默认模型" onClick={() => {
                  if (!writeChatDefaultModel(null)) { showFeedback("bad", "默认模型未清除：共享偏好未能提交，请查看同步提示。"); return; }
                  setChatDefault(null);
                  showFeedback("ok", "已提交清除新对话默认模型的设置。");
                }} />
              </>
            ) : <span className="text-cx-fg-3">未设定，点模型前的 ★ 设置</span>}
          </div>
        ) : null}
        <div className="flex items-center gap-3">
          {lastCheckedAt ? <span className="inline-flex items-center gap-1.5 text-cx-xs text-cx-fg-3"><Icon name="clock" size={13} />{lastCheckedAt}</span> : null}
          <Button size="sm" variant="outline" icon="refresh" loading={refreshing} disabled={loading || Boolean(busyItem)} onClick={() => void refreshAll()}>
            {refreshing ? "探测中…" : "探测全部"}
          </Button>
        </div>
      </div>
      {refreshing ? <p role="status" aria-live="polite" className="text-cx-xs text-cx-fg-3">{refreshProgress}</p> : null}
      {refreshReport ? (
        <Callout tone={refreshReport.kind === "ok" ? "success" : refreshReport.kind === "warn" ? "warning" : "danger"} icon={refreshReport.kind === "ok" ? "checkCircle" : "alert"} role="status" onDismiss={() => setRefreshReport(null)}>
          {refreshReport.detail}
        </Callout>
      ) : null}
      {feedback ? (
        <Callout tone={feedback.kind === "ok" ? "success" : feedback.kind === "warn" ? "warning" : "danger"} icon={feedback.kind === "ok" ? "checkCircle" : feedback.kind === "warn" ? "info" : "alert"} role="alert" onDismiss={() => setFeedback(null)}>
          {feedback.detail}
        </Callout>
      ) : null}

      <div className="grid grid-cols-1 items-start gap-4 @3xl/agents:grid-cols-[17rem_minmax(0,1fr)]">
        <EngineList
          engines={listedEngines}
          loading={loading}
          query={listQuery}
          onQueryChange={setListQuery}
          selectedEngine={currentEngine?.engine || null}
          selectedInstanceId={currentEngine && selection.instanceId ? (taskSettings ? currentInstance?.instance_id : currentInstance?.key) || "" : ""}
          useInstanceKeys={!taskSettings}
          busyItem={busyItem}
          onSelect={selectEngineRow}
          onToggle={(item) => void toggleEngine(item)}
          onToggleInstance={!taskSettings ? (item, instance) => void toggleInstance(item, instance) : undefined}
          onAdd={!taskSettings ? () => openAddDialog() : undefined}
          className={cn("@3xl/agents:sticky @3xl/agents:top-2", currentEngine && "hidden @3xl/agents:flex")}
        />

        {currentEngine ? (
          <div className="flex min-w-0 flex-col gap-4">
            <div className="@3xl/agents:hidden">
              <Button size="sm" variant="ghost" icon="arrowLeft" onClick={() => applySelection({ engine: null, instanceId: "", credentialId: "" }, "push")}>返回引擎列表</Button>
            </div>

            <div className="cx-settings-card" data-settings-anchor="agents-runtime" id="setting-agents-runtime">
              <div className="grid grid-cols-[44px_minmax(0,1fr)] items-center gap-3 px-4 py-3.5 @xl/agents:grid-cols-[44px_minmax(0,1fr)_auto] @xl/agents:gap-4">
                <span className="grid size-11 shrink-0 place-items-center rounded-xl border border-cx-border bg-cx-bg"><EngineLogo engine={currentEngine.engine} size={24} /></span>
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-x-2.5 gap-y-1">
                    <h2 className="text-cx-md font-semibold text-cx-fg">{instanceName(currentEngine, currentInstance)}</h2>
                    {currentEngine.version ? <code title={versionCheckDetail(currentInstance?.health?.version_check)} className="rounded-md bg-cx-code px-1.5 py-0.5 text-cx-xs text-cx-fg-2">{installedVersion(currentEngine.version, currentInstance?.health?.version_check)}</code> : null}
                    <Badge tone={statusTone(currentEngine.statusKind)} dot>{currentEngine.statusText}</Badge>
                    {currentInstance?.health?.version_check?.below_minimum ? (
                      <Badge tone="danger">
                        低于最低版本（需 &ge; {currentInstance.health.version_check.min_supported_version}）
                      </Badge>
                    ) : null}
                  </div>
                  <p className="mt-1 text-cx-xs leading-5 text-cx-fg-3">{currentEngine.readyCredentialCount} / {currentEngine.credentials.length} 个接入方式可用 · {currentEngine.modelCount} 个模型</p>
                  {currentInstance?.health?.version_check?.below_minimum ? (
                    <div className="mt-2 flex flex-wrap items-center gap-2 rounded-lg border border-cx-border bg-cx-bg-subtle px-2.5 py-1.5 text-cx-xs text-cx-danger">
                      <span>
                        禁止设置为 CTF / Pentest / SRC 出战 Worker。{currentInstance.health.version_check.upgrade_note || "请先升级 CLI 版本。"}
                      </span>
                      {currentInstance.health.version_check.upgrade_command ? (
                        <>
                          <code className="rounded bg-cx-code px-1.5 py-0.5 font-cx-mono text-cx-2xs text-cx-fg">
                            {currentInstance.health.version_check.upgrade_command}
                          </code>
                          <Button
                            size="xs"
                            variant="outline"
                            icon="copy"
                            onClick={() => {
                              const cmd = currentInstance.health?.version_check?.upgrade_command;
                              if (cmd) {
                                void copyToClipboard(cmd).then((ok) => {
                                  if (ok) showFeedback("ok", `已复制升级命令：${cmd}`);
                                });
                              }
                            }}
                          >
                            复制升级命令
                          </Button>
                        </>
                      ) : null}
                    </div>
                  ) : null}
                </div>
                <div className="col-span-2 flex flex-wrap items-center justify-end gap-2 @xl/agents:col-span-1">
                  <Button size="sm" variant="outline" icon="refresh" loading={probing} disabled={refreshing} onClick={() => void probeEngine(currentEngine)}>探测</Button>
                  <Button size="sm" variant="outline" icon="settings2" aria-expanded={runtimeOpen.open} onClick={() => setRuntimeOpen((current) => current.open ? { ...current, open: false } : { open: true, nonce: Date.now() })}>配置</Button>
                  <Switch size="sm" checked={currentInstance?.enabled ?? currentEngine.enabled} disabled={busyItem === `toggle:${currentInstance?.key || currentEngine.engine}`} ariaLabel={`${instanceName(currentEngine, currentInstance)} 启用状态`} onCheckedChange={() => void toggleInstance(currentEngine, currentInstance)} />
                  {!taskSettings ? (
                    <Menu trigger={<IconButton icon="more" label="更多操作" />} placement="bottom-end" ariaLabel={`${currentEngine.label} 操作`}>
                      <MenuItem icon="plus" onSelect={() => openAddDialog(currentEngine.engine)} description="同一引擎的另一套运行配置">添加实例</MenuItem>
                      {currentRemoval ? <>
                        <MenuSeparator />
                        <MenuItem icon={currentRemoval.restores ? "undo" : "trash"} danger={!currentRemoval.restores} onSelect={() => setRemoveTarget(currentInstance)}>
                          {currentRemoval.restores ? "恢复默认设置" : currentRemoval.last ? `移除 ${currentEngine.label}` : "移除此实例"}
                        </MenuItem>
                      </> : null}
                    </Menu>
                  ) : null}
                </div>
              </div>
            </div>

            {runtimeOpen.open ? (
              <RuntimeConfigPanel
                key={`${currentInstance?.key || currentEngine.engine}:${runtimeOpen.nonce}`}
                onClose={() => setRuntimeOpen((current) => ({ ...current, open: false }))}
                engine={currentEngine}
                instance={currentInstance}
                adapter={effectiveAdapter}
                fallbackKey={`${engineCliAdapter(descriptors, currentEngine.engine)}:default`}
                transportFields={currentTransportFields}
                connectionOptions={connectionOptions}
                accessModes={accessModeRows}
                envEditable={envEditable}
                showAccessModes={!taskSettings}
                probing={probing}
                refreshing={refreshing}
                onSwitchAdapter={(adapterId) => {
                  const instance = currentEngine.instances.find((row) => row.adapter_id === adapterId);
                  if (instance) applySelection({ ...selection, instanceId: instance.key }, "push");
                }}
                onProbe={() => void probeEngine(currentEngine)}
                onSave={saveRuntime}
              />
            ) : null}

            <div data-settings-anchor="agents-models" id="setting-agents-models">
              <ModelTable
                groups={groups}
                chatDefault={chatDefault}
                hidden={hiddenModels}
                catalog={pricing.catalog}
                expandedPrice={expandedPrice}
                onExpandPrice={setExpandedPrice}
                onSetDefault={selectChatDefault}
                onToggleHidden={(credentialId, modelId, hide) => {
                  if (!setModelHidden(credentialId, modelId, hide)) showFeedback("bad", "模型显隐未保存：共享偏好未能提交，请查看同步提示。");
                }}
                onTest={(id) => void testGroup(id)}
                onRefresh={(id) => void refreshGroupModels(id)}
                onViewTest={viewTest}
                onEdit={openFor("edit")}
                onImport={openFor("import")}
                onDelete={openFor("delete")}
                renderExtra={renderExtra}
                createPanel={createPanel}
                focusId={focusGroup}
                refreshDisabled={refreshing}
                loginCommand={currentInstance?.auth.login_command}
                showDefault={!taskSettings}
                showPrice={!taskSettings}
                empty={emptyGroups}
                toolbarActions={<>
                  {taskSettings ? <Select size="sm" value={backend} onChange={(value) => setBackend(value)} options={BACKEND_OPTIONS} ariaLabel="连通测试后端" /> : null}
                  <Button size="sm" variant="outline" icon="plus" disabled={panel?.kind === "create"} onClick={openCreate}>新增接入方式</Button>
                </>}
              />
            </div>
          </div>
        ) : (
          <div className="cx-settings-card hidden @3xl/agents:block">
            {!taskSettings && !loading && !listedEngines.length ? (
              <EmptyState icon="bot" title="还没有添加 Agent" description="添加要在对话中使用的 Agent，之后可在这里配置模型、价格和接入方式。" action={<Button size="sm" variant="primary" icon="plus" onClick={() => openAddDialog()}>添加 Agent</Button>} />
            ) : (
              <EmptyState icon="bot" title="选择一个 Agent 引擎" description="在左侧选择引擎，查看它的模型、价格和接入方式。" />
            )}
          </div>
        )}
      </div>

      {!taskSettings ? (
        <AddAgentDialog
          key={addDialog.nonce}
          open={addDialog.open}
          onOpenChange={(open) => setAddDialog((current) => ({ ...current, open }))}
          engines={engines}
          instances={instances}
          descriptors={descriptors}
          initialEngine={addDialog.engine}
          onAdd={addAgent}
        />
      ) : null}
      <Dialog
        open={Boolean(pendingRemoval?.removal)}
        onOpenChange={(open) => { if (!open && !removing) setRemoveTarget(null); }}
        size="sm"
        icon={pendingRemoval?.removal?.restores ? "undo" : "trash"}
        tone={pendingRemoval?.removal?.restores ? "warning" : "danger"}
        title={pendingRemoval ? (pendingRemoval.removal?.restores ? `恢复 ${pendingRemoval.name} 的默认设置？` : `移除 ${pendingRemoval.removal?.last ? pendingRemoval.item.label : pendingRemoval.name}？`) : ""}
        footer={<>
          <Button variant="ghost" disabled={removing} onClick={() => setRemoveTarget(null)}>取消</Button>
          <Button variant={pendingRemoval?.removal?.restores ? "primary" : "danger"} loading={removing} onClick={() => { if (removeTarget) void removeInstance(removeTarget); }}>
            {pendingRemoval?.removal?.restores ? "恢复默认" : "移除"}
          </Button>
        </>}
      >
        {pendingRemoval?.removal ? (
          <p>
            {pendingRemoval.removal.restores
              ? "已保存的可执行文件、启动参数和环境变量会被清除，改回自动探测的默认配置。"
              : "这个实例保存的运行配置会被删除。"}
            {pendingRemoval.removal.last ? `移除后 ${pendingRemoval.item.label} 不再出现在列表和模型选择器中，可随时重新添加。` : ""}
            已保存的接入方式（凭据）和对话记录不受影响。
          </p>
        ) : null}
      </Dialog>

      <Dialog
        open={discardGuard !== null}
        onOpenChange={(open) => { if (!open) setDiscardGuard(null); }}
        size="sm"
        tone="warning"
        title="放弃未保存的修改？"
        description="当前编辑尚未保存，继续将丢弃这些修改。"
        footer={<>
          <Button variant="ghost" onClick={() => setDiscardGuard(null)}>取消</Button>
          <Button variant="danger" onClick={() => { const guard = discardGuard; setDiscardGuard(null); if (guard) commitPanel(guard.next, guard.after); }}>放弃修改</Button>
        </>}
      />

      {!taskSettings ? (
        <div data-settings-anchor="agents-version-policy" id="setting-agents-version-policy">
          <AgentVersionSupportSection workerSettingsHref="/ctf/workers" />
        </div>
      ) : null}
      {!taskSettings ? (
        <PricingFooter catalog={pricing.catalog} error={pricing.error} onCatalog={pricing.setCatalog} knownModels={knownModels} engines={pricingEngines} defaultEngine={currentEngine?.engine} extra={extraPrice} />
      ) : null}
      {!taskSettings ? <UsageProviderSettings /> : null}
    </div>
  );
}
