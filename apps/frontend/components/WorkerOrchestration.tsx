"use client";
import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ComponentProps } from "react";
import { Alert, Button, Skeleton } from "@muteki/ui";
import { RetentionSettings } from "@/components/RetentionSettings";
import { SettingsNav, SettingsPage, type SettingsNavGroup, type SettingsNavLinkProps } from "@/components/settings/primitives";
import {
  type DispatchTemplate,
  type GlobalCredential,
  type ModelEndpoint,
  type ProfileHealth,
  type WorkerModelTestResult,
  type WorkerModelOptions,
  type WorkerImageStatus,
  type WorkerSettings,
  checkAuth,
  fetchProfilesHealth,
  getGlobalCredentials,
  getModelEndpoints,
  getWorkerModelOptions,
  getWorkerModelTestResults,
  getWorkerImageStatus,
  getWorkerSettings,
  putWorkerSettings,
  pullWorkerImage,
  refreshCredentialModels,
  testWorkerProfileModel,
  testWorkerProfileModelsBatch,
} from "@/lib/useRun";
import {
  engineDisplayName, loadProviderDescriptors, readyCatalog, useProviderDescriptors,
  type ProviderWorkerProfileSpec,
} from "@/lib/providerDescriptors";
import { WorkerRoster } from "@/components/worker-roster/WorkerRoster";
import { DispatchTemplates } from "@/components/worker-roster/DispatchTemplates";
import { AddWorkerDialog } from "@/components/worker-roster/AddWorkerDialog";
import { AgentVersionSupportSection } from "@/components/worker-settings/AgentVersionSupportSection";
import { RuntimeSection } from "@/components/worker-settings/RuntimeSection";
import { LocalPiSection } from "@/components/worker-settings/LocalPiSection";
import { ExecutionGuardrailSection } from "@/components/worker-settings/ExecutionGuardrailSection";
import { SchedulingSection } from "@/components/worker-settings/SchedulingSection";
import { NodesSection } from "@/components/worker-settings/NodesSection";
import { QUOTA_FIELDS, type WorkerQuotas } from "@/components/worker-settings/RuntimeSection";
import { useAgentVersionPolicy, workerVersionIssue, type PolicyModeId } from "@/lib/agentVersionPolicy";
import { listRunnerNodes, type RunnerNode } from "@/lib/runnerNodes";
import { ModelsSection, llmTemperatureMode } from "@/components/worker-settings/ModelsSection";
import { SaveBar } from "@/components/worker-roster/SaveBar";
import {
  ORDINARY_ROLES, canServeChannel, credentialKey, engineLocalOnly, engineOf, globalCredentialForLegacy, globalCredentialKey, globalCredentialUsable,
  isOrdinarySeat, workerProfileSpec,
  type BatchCheckState, type Catalog, type Credential, type Engine, type ModelDiscoveryOutcome, type ReviewPolicy,
  type NewWorkerOptions, type Seat, type VerifierPolicy,
} from "@/components/worker-roster/model";

const LazyTaskCredentialManager = lazy(
  () => import("@/components/ProviderManager").then((module) => ({ default: module.ProviderManager })),
);
function TaskCredentialManager(props: ComponentProps<typeof LazyTaskCredentialManager>) {
  const [client, setClient] = useState(false);
  useEffect(() => { setClient(true); }, []);
  return client ? <Suspense fallback={<div role="status">正在加载凭据设置…</div>}><LazyTaskCredentialManager {...props} /></Suspense> : null;
}

type SettingsSection = "roster" | "local_pi" | "guardrail" | "versions" | "credentials" | "runtime" | "nodes" | "scheduling" | "models" | "retention";
const WORKER_SECTIONS: SettingsSection[] = ["roster", "local_pi", "guardrail", "versions", "credentials", "runtime", "nodes", "scheduling", "models", "retention"];
function isWorkerSection(value: string): value is SettingsSection {
  return WORKER_SECTIONS.includes(value as SettingsSection);
}
type SaveState = "idle" | "saving" | "saved" | "error";
type AccountConnection = "official" | "custom_endpoint";
type WorkerDraftSnapshot = {
  seats: Seat[];
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  backend: WorkerSettings["worker_backend"];
  nodeId: string;
  network: WorkerSettings["worker_network"] | "none";
  containerScope: WorkerSettings["worker_container_scope"];
  ociRuntime: string;
  quotas: WorkerQuotas;
  maxTotal: number;
  wallClock: number;
  costBudget: number;
  llmProfiles: WorkerSettings["llm_profiles"];
};

const DEFAULT_LLM_PROFILES: WorkerSettings["llm_profiles"] = {
  planner: { provider: "deepseek", model: "deepseek-v4-pro", base_url: "", connection: "default", temperature_mode: "omit" },
  titler: { provider: "deepseek", model: "deepseek-v4-flash", base_url: "", connection: "default", temperature_mode: "omit" },
};
const AUTO_CHECK_KEY = "muteki.workers.autoCheck";

function workerDraftSignature(snapshot: WorkerDraftSnapshot): string {
  return JSON.stringify(snapshot);
}

const quotaText = (value: unknown) => value === null || value === undefined ? "" : String(value);
function quotasOf(config: WorkerSettings): WorkerQuotas {
  return {
    memory: quotaText(config.worker_memory),
    cpus: quotaText(config.worker_cpus),
    pids: quotaText(config.worker_pids_limit),
    output: quotaText(config.worker_output_limit),
    disk: quotaText(config.worker_disk_limit),
  };
}
function quotaPayload(quotas: WorkerQuotas) {
  const text = (value: string) => value.trim() || null;
  const number = (value: string) => value.trim() ? Number(value.trim()) : null;
  return {
    worker_memory: text(quotas.memory),
    worker_cpus: number(quotas.cpus),
    worker_pids_limit: number(quotas.pids),
    worker_output_limit: text(quotas.output),
    worker_disk_limit: text(quotas.disk),
  };
}
function invalidQuota(quotas: WorkerQuotas): string {
  const size = /^\d+(\.\d+)?\s*([kmgtpe]i?b?|b)?$/i;
  for (const field of QUOTA_FIELDS) {
    const value = quotas[field.key].trim();
    if (!value) continue;
    if (field.key === "cpus" ? !(Number(value) > 0 && Number.isFinite(Number(value)))
      : field.key === "pids" ? !(Number.isInteger(Number(value)) && Number(value) > 0)
        : !size.test(value)) return field.title;
  }
  return "";
}


function endpointWireApi(catalog: Catalog, engine: Engine): string {
  return workerProfileSpec(catalog, engine)?.endpoint_wire_api ?? "";
}

const DEFAULT_REVIEW: ReviewPolicy = {
  enabled: false,
  engine: "",
  timeout: 90,
  after_fruitless_workers: 0,
  after_duplicate_intents: 0,
  on_course_correct: false,
  on_candidate_spike: false,
  on_operator_hint: false,
  allow_review_fallback: false,
  every_completed_workers: 0,
  candidate_spike_threshold: 5,
  max_concurrent: 1,
  cooldown_events: 8,
  max_review_workers: 12,
  reasoning_effort: "inherit",
};

const DEFAULT_VERIFIER: VerifierPolicy = {
  enabled: false,
  engine: "",
  timeout: 240,
  max_concurrent: 0,
  allow_verifier_fallback: false,
  max_verifier_workers: 24,
  reasoning_effort: "inherit",
};

function randomId(prefix: string, engine: Engine): string {
  const suffix = typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID().replaceAll("-", "").slice(0, 8)
    : `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  return `${prefix}_${engine}_${suffix}`;
}

function syncCredentialFromGlobal(credential: Credential, availableCredentials: GlobalCredential[], catalog: Catalog): Credential {
  const selected = globalCredentialForLegacy(credential, availableCredentials);
  if (!selected) return credential;
  const engine = engineOf(selected.engine);
  const connection = selected.connection === "custom_endpoint" ? "custom_endpoint" : "official";
  return {
    ...credential,
    label: selected.label || globalCredentialKey(selected),
    engine,
    kind: selected.source === "system" ? "system_inherit" : connection === "custom_endpoint" ? "custom_endpoint" : "engine_key",
    secret_ref: selected.source === "system" ? "" : globalCredentialKey(selected),
    target_engine: connection === "custom_endpoint" ? engine : undefined,
    endpoint: connection === "custom_endpoint"
      ? { base_url: selected.base_url || "", wire_api: endpointWireApi(catalog, engine) }
      : undefined,
  };
}

function syncCredentialsFromGlobal(credentials: Credential[], availableCredentials: GlobalCredential[], catalog: Catalog): Credential[] {
  return credentials.map((credential) => syncCredentialFromGlobal(credential, availableCredentials, catalog));
}

function normalizeSeatRoles(rawRoles: string[]): string[] {
  const hadRace = rawRoles.includes("race");
  const roles = rawRoles.filter((role) => role !== "race");
  if (hadRace && !roles.some((role) => ORDINARY_ROLES.includes(role) || role === "solve" || role === "scout")) {
    return ["explore", ...roles];
  }
  return roles;
}

function legacyIdentity(config: WorkerSettings, catalog: Catalog): {
  seats: Seat[];
  credentials: Credential[];
} {
  if (Array.isArray(config.seats)) {
    return {
      seats: config.seats.map((item) => ({
        ...item,
        reasoning_effort: item.reasoning_effort || "default",
        capacity: { ...item.capacity },
        roles: normalizeSeatRoles(item.roles),
      })),
      credentials: (config.credentials || []).map((item) => ({ ...item, endpoint: item.endpoint ? { ...item.endpoint } : undefined })),
    };
  }

  const credentials: Credential[] = [];
  const seats = config.worker_profiles.map((profile, index): Seat => {
    const engine = engineOf(profile.engine);
    const account = profile.credential_account || "";
    const id = `cred_legacy_${engine}_${index}`;
    credentials.push({
      id,
      label: account || `${engineDisplayName(catalog, engine)} 系统登录`,
      engine,
      kind: profile.base_url ? "custom_endpoint" : account ? "engine_key" : "system_inherit",
      secret_ref: account,
      target_engine: profile.base_url ? engine : undefined,
      endpoint: profile.base_url ? { base_url: profile.base_url, wire_api: profile.wire_api } : undefined,
    });
    return {
      id: profile.id,
      label: (profile as typeof profile & { label?: string }).label || profile.name || profile.id,
      engine,
      credential_id: id,
      model: profile.model || "",
      reasoning_effort: profile.reasoning_effort || "default",
      roles: normalizeSeatRoles(profile.roles),
      capacity: {
        max_running: Math.max(1, Number(profile.max_running) || 1),
        max_review_running: Math.max(0, Number(profile.max_review_running) || 0),
      },
      priority: profile.priority,
      enabled: profile.enabled,
    };
  });
  return { seats, credentials };
}

function modelTestHealth(result: WorkerModelTestResult): ProfileHealth["status"] {
  if (result.ok) return "ok";
  return ["auth", "credential", "credential.unavailable", "credential.api_key_required"].includes(result.layer || "") ? "auth_failed" : "blocked";
}

function buildModelTestProfile({
  worker,
  id,
  label,
  engine,
  accountId,
  connection,
  baseUrl,
  model,
  reasoningEffort,
}: {
  worker: ProviderWorkerProfileSpec;
  id: string;
  label: string;
  engine: Engine;
  accountId: string;
  connection: AccountConnection | "system";
  baseUrl?: string;
  model?: string;
  reasoningEffort?: string;
}): WorkerSettings["worker_profiles"][number] {
  const custom = connection === "custom_endpoint";
  const credentialMode = custom ? "api_key" : worker.official_credential_mode;
  return {
    id,
    name: label,
    engine,
    transport: worker.transport,
    auth: credentialMode,
    credential_mode: credentialMode,
    credential_account: accountId === "__system__" ? "" : accountId,
    credential_id: accountId === "__system__" ? `system:${engine}` : `account:${accountId}`,
    api_key_ref: "",
    base_url: custom ? String(baseUrl || "") : "",
    wire_api: worker.endpoint_wire_api,
    roles: [...ORDINARY_ROLES, "review"],
    max_running: 1,
    max_review_running: 0,
    priority: 10,
    model: model || "",
    reasoning_effort: reasoningEffort || "default",
    enabled: true,
  };
}

function SectionFrame({ children }: { children: React.ReactNode }) {
  return <div className="wsettings-orchestration wsettings-narrow">{children}</div>;
}

function WorkspaceSettingsLink({ href, children, ...props }: SettingsNavLinkProps) {
  return <a href={href} {...props}>{children}</a>;
}

export function WorkerOrchestration({ defaultReturnTo = "/", navigation }: { defaultReturnTo?: string; navigation: { pathname: string; searchParams: URLSearchParams } }) {
  const descriptors = readyCatalog(useProviderDescriptors());
  const versionPolicy = useAgentVersionPolicy();
  const [config, setConfig] = useState<WorkerSettings | null>(null);
  const [seats, setSeats] = useState<Seat[]>([]);
  const [templates, setTemplates] = useState<DispatchTemplate[]>([]);
  const [credentials, setCredentials] = useState<Credential[]>([]);
  const [availableCredentials, setAvailableCredentials] = useState<GlobalCredential[]>([]);
  const [addingWorker, setAddingWorker] = useState(false);
  const [modelEndpoints, setModelEndpoints] = useState<ModelEndpoint[]>([]);
  const [models, setModels] = useState<WorkerModelOptions>({
    allow_custom: false,
    manual_models: {},
    discovered_models: {},
    models_by_profile: {},
    discovery: {},
    models: {},
  });
  const [, setHealth] = useState<Record<string, ProfileHealth>>({});
  const [section, setSection] = useState<SettingsSection>("roster");
  const [highlightId, setHighlightId] = useState<string | null>(null);
  const [autoCheck, setAutoCheck] = useState(true);
  const [review, setReview] = useState<ReviewPolicy>(DEFAULT_REVIEW);
  const [verifier, setVerifier] = useState<VerifierPolicy>(DEFAULT_VERIFIER);
  const [backend, setBackend] = useState<WorkerSettings["worker_backend"]>("local");
  const [nodeId, setNodeId] = useState("local");
  const [runnerNodes, setRunnerNodes] = useState<RunnerNode[] | null>(null);
  const [ociRuntime, setOciRuntime] = useState("");
  const [quotas, setQuotas] = useState<WorkerQuotas>({ memory: "", cpus: "", pids: "", output: "", disk: "" });
  const [network, setNetwork] = useState<WorkerSettings["worker_network"]>("bridge");
  const [effectiveNetwork, setEffectiveNetwork] = useState<string>("");
  const [networkError, setNetworkError] = useState<string>("");
  const [containerScope, setContainerScope] = useState<WorkerSettings["worker_container_scope"]>("run");
  const [imageStatus, setImageStatus] = useState<WorkerImageStatus | null>(null);
  const [imageLoading, setImageLoading] = useState(false);
  const [pullingImage, setPullingImage] = useState(false);
  const [maxTotal, setMaxTotal] = useState(0);
  const [wallClock, setWallClock] = useState(0);
  const [costBudget, setCostBudget] = useState(0);
  const [llmProfiles, setLlmProfiles] = useState<WorkerSettings["llm_profiles"]>(DEFAULT_LLM_PROFILES);
  const [dirty, setDirty] = useState(false);
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [testingIds, setTestingIds] = useState<Set<string>>(() => new Set());
  const [batchCheck, setBatchCheck] = useState<BatchCheckState>({ running: false, completed: 0, total: 0 });
  const [discoveringModels, setDiscoveringModels] = useState(false);
  const [testResults, setTestResults] = useState<Record<string, WorkerModelTestResult>>({});
  const [feedback, setFeedback] = useState("");
  const [inContainer, setInContainer] = useState(false);
  const [reloadRevision, setReloadRevision] = useState(0);
  const [initialLoadError, setInitialLoadError] = useState("");
  const baselineDraftRef = useRef("");
  const autoCheckSeenRef = useRef<Map<string, string> | null>(null);
  const autoCheckTimersRef = useRef(new Map<string, number>());

  const currentDraftSignature = useMemo(() => workerDraftSignature({
    seats,
    review,
    verifier,
    backend,
    nodeId,
    network,
    containerScope,
    ociRuntime,
    quotas,
    maxTotal,
    wallClock,
    costBudget,
    llmProfiles,
  }), [backend, containerScope, costBudget, llmProfiles, maxTotal, network, nodeId, ociRuntime, quotas,
    review, seats, verifier, wallClock]);
  const currentDraftSignatureRef = useRef(currentDraftSignature);
  currentDraftSignatureRef.current = currentDraftSignature;

  useEffect(() => {
    if (!config || !baselineDraftRef.current) return;
    setDirty(currentDraftSignature !== baselineDraftRef.current);
  }, [config, currentDraftSignature]);

  const { searchParams, pathname } = navigation;
  const returnParam = searchParams.get("return");
  const requestedSection = searchParams.get("section") || searchParams.get("tab");
  const returnTo = useMemo(() => {
    const value = returnParam || defaultReturnTo;
    return value.startsWith("/") ? value : defaultReturnTo;
  }, [defaultReturnTo, returnParam]);
  const activeMode: PolicyModeId = useMemo(
    () => (pathname.startsWith("/pentest") || defaultReturnTo.startsWith("/pentest") ? "pentest" : "ctf"),
    [defaultReturnTo, pathname],
  );
  const effectiveNodeId = backend === "container" ? nodeId : "local";
  const workspaceLabels = useMemo(() => {
    return activeMode === "pentest"
      ? { title: "渗透设置", back: "返回渗透" }
      : { title: "单题设置", back: "返回单题" };
  }, [activeMode]);

  useEffect(() => {
    const requested = (requestedSection || window.location.hash.replace(/^#/, "")).trim();
    if (requested === "system") {
      window.location.replace("/settings/update");
      return;
    }
    if (requested === "appearance") {
      window.location.replace("/settings/appearance");
      return;
    }
    const next = requested === "execution-guardrail" ? "guardrail" : requested;
    const allowed = isWorkerSection(next)
      && (next !== "local_pi" || activeMode === "ctf")
      && (next !== "guardrail" || activeMode === "pentest");
    setSection(allowed ? next : "roster");
  }, [activeMode, requestedSection]);

  const markDirty = useCallback(() => { setDirty(true); setSaveState("idle"); }, []);
  const showFeedback = useCallback((message: string) => { setFeedback(message); window.setTimeout(() => setFeedback(""), 3200); }, []);
  const refreshHealth = useCallback(async () => { const rows = await fetchProfilesHealth(); setHealth(Object.fromEntries(rows.map((item) => [item.profile_id, item]))); }, []);
  const refreshImage = useCallback(async () => {
    setImageLoading(true);
    setImageStatus(await getWorkerImageStatus());
    setImageLoading(false);
  }, []);
  const pullImage = useCallback(async () => {
    setPullingImage(true);
    const result = await pullWorkerImage();
    setPullingImage(false);
    showFeedback(result.ok ? "Worker 镜像拉取完成" : `镜像拉取失败：${result.detail}`);
    await refreshImage();
  }, [refreshImage, showFeedback]);
  const refreshWorkerModels = useCallback(async (credentialId: string, engine: Engine): Promise<ModelDiscoveryOutcome> => {
    setDiscoveringModels(true);
    try {
      const credential = availableCredentials.find((item) => item.id === credentialId);
      if (!credential) {
        return { ok: false, detail: "当前 Worker 尚未绑定可用凭据。" };
      }
      const result = await refreshCredentialModels(credential.id, {
        engine,
        connection: credential.connection === "custom_endpoint" ? "custom_endpoint" : "official",
        base_url: credential.base_url || "",
        backend,
        runtime_instance: credential.model_catalog?.runtime_instance || "default",
      });
      setAvailableCredentials(await getGlobalCredentials());
      if (result.ok) {
        const detail = result.detail || `已为当前凭据更新 ${result.models.length} 个模型。`;
        showFeedback(detail);
        return { ok: true, detail };
      }
      return {
        ok: false,
        detail: result.detail || `${engineDisplayName(descriptors, engine)} 未返回当前凭据可用的模型。`,
      };
    } finally {
      setDiscoveringModels(false);
    }
  }, [availableCredentials, backend, descriptors, showFeedback]);

  useEffect(() => {
    let alive = true;
    Promise.all([getWorkerSettings(), getGlobalCredentials(), getModelEndpoints(), getWorkerModelOptions(), fetchProfilesHealth(), getWorkerModelTestResults(), getWorkerImageStatus(), checkAuth(), loadProviderDescriptors().catch(() => null)]).then(([cfg, globalRows, endpointRows, modelRows, healthRows, modelTestRows, workerImage, authState, catalog]) => {
      if (!alive) return;
      if (!cfg) {
        setInitialLoadError("无法读取 Worker 配置，请确认服务连接正常。");
        return;
      }
      setInitialLoadError("");
      const identity = legacyIdentity(cfg, catalog);
      const loadedReview = { ...DEFAULT_REVIEW, ...(cfg.stage_policy.coordinator.review || {}) };
      const loadedVerifier = { ...DEFAULT_VERIFIER, ...(cfg.stage_policy.coordinator.verifier || {}) };
      baselineDraftRef.current = workerDraftSignature({
        seats: identity.seats,
        review: loadedReview,
        verifier: loadedVerifier,
        backend: cfg.worker_backend || "local",
        nodeId: cfg.worker_node_id || "local",
        network: cfg.worker_network || "bridge",
        containerScope: cfg.worker_container_scope || "run",
        ociRuntime: cfg.worker_oci_runtime || "",
        quotas: quotasOf(cfg),
        maxTotal: cfg.max_total_workers,
        wallClock: cfg.wall_clock_budget,
        costBudget: cfg.cost_budget_usd,
        llmProfiles: cfg.llm_profiles,
      });
      setConfig(cfg);
      setSeats(identity.seats);
      setTemplates(cfg.dispatch_templates ?? []);
      setCredentials(syncCredentialsFromGlobal(identity.credentials, globalRows, catalog));
      setAvailableCredentials(globalRows);
      setModelEndpoints(endpointRows);
      setModels(modelRows);
      setHealth(Object.fromEntries(healthRows.map((item) => [item.profile_id, item])));
      setTestResults(modelTestRows);
      autoCheckSeenRef.current = null;
      setReview(loadedReview);
      setVerifier(loadedVerifier);
      setBackend(cfg.worker_backend || "local");
      setNodeId(cfg.worker_node_id || "local");
      setOciRuntime(cfg.worker_oci_runtime || "");
      setQuotas(quotasOf(cfg));
      setNetwork(cfg.worker_network === "host" ? "host" : "bridge");
      setContainerScope(cfg.worker_container_scope || "run");
      setEffectiveNetwork(cfg.effective_network || cfg.worker_network || "bridge");
      setNetworkError(cfg.network_error || "");
      setImageStatus(workerImage);
      setInContainer(authState.inContainer);
      setMaxTotal(cfg.max_total_workers);
      setWallClock(cfg.wall_clock_budget);
      setCostBudget(cfg.cost_budget_usd);
      setLlmProfiles(cfg.llm_profiles);
    }).catch((cause) => {
      if (alive) setInitialLoadError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => { alive = false; };
  }, [reloadRevision]);

  const ordinarySeats = seats.filter(isOrdinarySeat);
  const maxWorkers = ordinarySeats.filter((seat) => seat.enabled).reduce((sum, seat) => sum + Math.max(1, seat.capacity.max_running || 1), 0);

  const ensureCredential = useCallback((engine: Engine, key: string, resolvedCredential?: GlobalCredential): string => {
    const selected = resolvedCredential || availableCredentials.find((item) => item.engine === engine && globalCredentialKey(item) === key);
    if (!globalCredentialUsable(selected) || selected.engine !== engine) return "";
    const existing = credentials.find((credential) => (
      credential.id === selected.id
      || (credential.engine === engine && credentialKey(credential) === key)
    ));
    const connection = selected?.connection === "custom_endpoint" ? "custom_endpoint" : "official";
    if (existing) {
      if (selected) {
        setCredentials((current) => current.map((credential) => credential.id === existing.id ? syncCredentialFromGlobal(credential, [selected], descriptors) : credential));
      }
      return existing.id;
    }
    const next: Credential = {
      id: selected.id,
      label: selected?.label || (key === "__system__" ? `${engineDisplayName(descriptors, engine)} 系统登录` : key || "未配置模型服务"),
      engine,
      kind: key === "__system__" ? "system_inherit" : connection === "custom_endpoint" ? "custom_endpoint" : "engine_key",
      secret_ref: key === "__system__" ? "" : key,
      target_engine: connection === "custom_endpoint" ? engine : undefined,
      endpoint: connection === "custom_endpoint" ? { base_url: selected?.base_url || "", wire_api: endpointWireApi(descriptors, engine) } : undefined,
    };
    setCredentials((current) => [...current, next]);
    return next.id;
  }, [availableCredentials, credentials, descriptors]);

  const updateSeat = useCallback((id: string, patch: Partial<Seat>) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const stoppedChannels: string[] = [];
    if (patch.enabled === false && seat.enabled) {
      if (review.engine === id) {
        if (review.enabled) stoppedChannels.push("Review");
        setReview((current) => ({ ...current, enabled: false, engine: "" }));
      }
      if (verifier.engine === id) {
        if (verifier.enabled) stoppedChannels.push("Verifier");
        setVerifier((current) => ({ ...current, enabled: false, engine: "" }));
      }
    }
    setSeats((current) => current.map((item) => item.id === id ? { ...item, ...patch } : item));
    if (["engine", "credential_id", "model", "reasoning_effort"].some((key) => key in patch)) {
      setTestResults((current) => {
        const next = { ...current };
        delete next[id];
        return next;
      });
    }
    markDirty();
    if (stoppedChannels.length) showFeedback(`${seat.label} 已停用，同时关闭 ${stoppedChannels.join(" 和 ")}`);
  }, [markDirty, review.enabled, review.engine, seats, showFeedback, verifier.enabled, verifier.engine]);
  const bindAccount = useCallback((id: string, key: string, resolvedCredential?: GlobalCredential) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const selected = resolvedCredential || availableCredentials.find((item) => globalCredentialKey(item) === key && item.engine === seat.engine);
    if (!globalCredentialUsable(selected)) {
      updateSeat(id, { credential_id: "", model: "" });
      showFeedback(`${engineDisplayName(descriptors, engineOf(seat.engine))} 没有可用凭据，请在 Agent 凭据分区完成配置`);
      return;
    }
    const engine = selected ? engineOf(selected.engine) : engineOf(seat.engine);
    updateSeat(id, {
      engine,
      credential_id: ensureCredential(engine, key, selected || undefined),
      ...(engine === seat.engine ? {} : { model: "", reasoning_effort: "default" }),
    });
  }, [availableCredentials, descriptors, ensureCredential, seats, showFeedback, updateSeat]);

  const changeEngine = useCallback((id: string, engine: Engine) => {
    const matching = availableCredentials.find((credential) => credential.engine === engine && globalCredentialUsable(credential) && (credential.source === "stored" || backend === "local"));
    if (!matching) {
      updateSeat(id, { engine, credential_id: "", model: "", reasoning_effort: "default" });
      showFeedback(`${engineDisplayName(descriptors, engine)} 没有可用凭据，请在 Agent 凭据分区完成配置`);
      return;
    }
    updateSeat(id, { engine, credential_id: ensureCredential(engine, globalCredentialKey(matching), matching), model: matching.default_model || "", reasoning_effort: "default" });
  }, [availableCredentials, backend, descriptors, ensureCredential, showFeedback, updateSeat]);

  const addSeat = useCallback((matching: GlobalCredential, options: NewWorkerOptions) => {
    const engine = engineOf(matching.engine);
    if (!globalCredentialUsable(matching) || (backend !== "local" && (matching.source === "system" || engineLocalOnly(descriptors, engine)))) {
      showFeedback("当前凭据或运行方式不可用，请重新选择。");
      return false;
    }
    const verIssue = workerVersionIssue(versionPolicy.data, engine, backend, activeMode, effectiveNodeId);
    if (verIssue) {
      setSaveState("error");
      showFeedback(verIssue.detail);
      return false;
    }
    const credentialId = ensureCredential(engine, globalCredentialKey(matching), matching);
    if (!credentialId) return false;
    const id = randomId("seat", engine);
    const next: Seat = { id, label: options.label, engine, credential_id: credentialId, model: options.model, reasoning_effort: options.reasoning_effort, roles: [...ORDINARY_ROLES, "review"], capacity: { max_running: options.max_running, max_review_running: 0 }, priority: ordinarySeats.length * 10 + 10, enabled: true };
    setSeats((current) => [...current, next]);
    setHighlightId(id);
    markDirty();
    return true;
  }, [activeMode, backend, descriptors, effectiveNodeId, ensureCredential, markDirty, ordinarySeats, showFeedback, versionPolicy.data]);

  const duplicateSeat = useCallback((id: string) => { const source = seats.find((seat) => seat.id === id); if (!source) return; const next = { ...source, id: randomId("seat", engineOf(source.engine)), label: `${source.label} 副本`, capacity: { ...source.capacity }, roles: [...source.roles], priority: seats.length * 10 + 10 }; setSeats((current) => [...current, next]); setHighlightId(next.id); markDirty(); }, [markDirty, seats]);
  const deleteSeat = useCallback((id: string) => {
    const next = seats.filter((seat) => seat.id !== id);
    setSeats(next);
    if (review.engine === id) {
      setReview((current) => ({ ...current, enabled: false, engine: "" }));
    }
    if (verifier.engine === id) {
      setVerifier((current) => ({ ...current, enabled: false, engine: "" }));
    }
    markDirty();
    const stoppedChannels = [review.enabled && review.engine === id ? "Review" : "", verifier.enabled && verifier.engine === id ? "Verifier" : ""].filter(Boolean);
    showFeedback(`Worker 已移除${stoppedChannels.length ? `，同时关闭 ${stoppedChannels.join(" 和 ")}` : ""}，点击“保存配置”后生效`);
  }, [markDirty, review.enabled, review.engine, verifier.enabled, verifier.engine, seats, showFeedback]);
  const reorderSeats = useCallback((sourceId: string, targetId: string | null) => {
    setSeats((current) => {
      const ordinary = current.filter(isOrdinarySeat);
      const dedicated = current.filter((seat) => !isOrdinarySeat(seat));
      const from = ordinary.findIndex((seat) => seat.id === sourceId);
      if (from < 0) return current;
      const next = [...ordinary];
      const [moved] = next.splice(from, 1);
      if (targetId === null) {
        next.push(moved);
      } else {
        const originalTo = ordinary.findIndex((seat) => seat.id === targetId);
        const to = next.findIndex((seat) => seat.id === targetId);
        if (to < 0) return current;
        next.splice(from < originalTo ? to + 1 : to, 0, moved);
      }
      return [...next, ...dedicated];
    });
    markDirty();
  }, [markDirty]);
  const toggleSeatEnabled = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    if (!seat.enabled) {
      const verIssue = workerVersionIssue(versionPolicy.data, seat.engine, backend, activeMode, effectiveNodeId);
      if (verIssue) {
        setSaveState("error");
        showFeedback(verIssue.detail);
        return;
      }
    }
    updateSeat(id, { enabled: !seat.enabled });
  }, [activeMode, backend, effectiveNodeId, seats, showFeedback, updateSeat, versionPolicy.data]);

  const disableSeats = useCallback((ids: string[]) => {
    if (!ids.length) return;
    const idSet = new Set(ids);
    if (review.engine && idSet.has(review.engine)) {
      setReview((current) => ({ ...current, enabled: false, engine: "" }));
    }
    if (verifier.engine && idSet.has(verifier.engine)) {
      setVerifier((current) => ({ ...current, enabled: false, engine: "" }));
    }
    setSeats((current) => current.map((item) => (idSet.has(item.id) ? { ...item, enabled: false } : item)));
    markDirty();
    showFeedback(`已停用 ${ids.length} 个低版本 Worker 席位，请点击“保存配置”生效`);
  }, [markDirty, review.engine, showFeedback, verifier.engine]);

  const setReviewSeat = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const verIssue = workerVersionIssue(versionPolicy.data, seat.engine, backend, activeMode, effectiveNodeId);
    if (verIssue) {
      setSaveState("error");
      showFeedback(verIssue.detail);
      return;
    }
    setSeats((current) => current.map((item) => item.id === id && !item.roles.includes("review") ? { ...item, roles: [...item.roles, "review"] } : item));
    setReview((current) => ({ ...current, engine: id, enabled: true, max_concurrent: 1 }));
    markDirty();
    showFeedback(`${seat.label} 已设为 Review Worker`);
  }, [activeMode, backend, effectiveNodeId, markDirty, seats, showFeedback, versionPolicy.data]);

  const setVerifierSeat = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const verIssue = workerVersionIssue(versionPolicy.data, seat.engine, backend, activeMode, effectiveNodeId);
    if (verIssue) {
      setSaveState("error");
      showFeedback(verIssue.detail);
      return;
    }
    setSeats((current) => current.map((item) => item.id === id && !item.roles.includes("verifier") ? { ...item, roles: [...item.roles, "verifier"] } : item));
    setVerifier((current) => ({ ...current, engine: id, enabled: true, max_concurrent: Math.max(0, current.max_concurrent ?? 0) }));
    markDirty();
    showFeedback(`${seat.label} 已设为 Verifier Worker`);
  }, [activeMode, backend, effectiveNodeId, markDirty, seats, showFeedback, versionPolicy.data]);

  const updateReview = useCallback((patch: Partial<ReviewPolicy>) => { setReview((current) => ({ ...current, ...patch, max_concurrent: 1 })); markDirty(); }, [markDirty]);
  const updateVerifier = useCallback((patch: Partial<VerifierPolicy>) => {
    setVerifier((current) => ({
      ...current,
      ...patch,
      max_concurrent: Math.max(0, patch.max_concurrent ?? current.max_concurrent ?? 0),
    }));
    markDirty();
  }, [markDirty]);
  const createDedicatedReview = useCallback(() => { const source = seats.find((seat) => seat.id === review.engine) || ordinarySeats[0]; if (!source) { showFeedback("请先添加一个 Worker"); return; } const next: Seat = { ...source, id: randomId("seat", engineOf(source.engine)), label: `${engineDisplayName(descriptors, engineOf(source.engine))} Review`, roles: ["review"], capacity: { max_running: 1, max_review_running: 1 }, priority: seats.length * 10 + 10, enabled: true }; setSeats((current) => [...current, next]); setReview((current) => ({ ...current, engine: next.id, enabled: true, max_concurrent: 1 })); markDirty(); showFeedback("已创建独立 Review 配置"); }, [descriptors, markDirty, ordinarySeats, review.engine, seats, showFeedback]);
  const createDedicatedVerifier = useCallback(() => {
    const source = seats.find((seat) => seat.id === verifier.engine) || ordinarySeats[0];
    if (!source) { showFeedback("请先添加一个 Worker"); return; }
    const next: Seat = {
      ...source,
      id: randomId("seat", engineOf(source.engine)),
      label: `${engineDisplayName(descriptors, engineOf(source.engine))} Verifier`,
      roles: ["verifier"],
      capacity: { max_running: 1, max_review_running: 0 },
      priority: seats.length * 10 + 10,
      enabled: true,
    };
    setSeats((current) => [...current, next]);
    setVerifier((current) => ({ ...current, engine: next.id, enabled: true, max_concurrent: Math.max(0, current.max_concurrent ?? 0) }));
    markDirty();
    showFeedback("已创建独立 Verifier 配置");
  }, [descriptors, markDirty, ordinarySeats, seats, showFeedback, verifier.engine]);

  const testSeat = useCallback(async (seat: Seat, quiet = false): Promise<WorkerModelTestResult> => {
    const credential = credentials.find((item) => item.id === seat.credential_id);
    const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
    const connection = credential?.kind === "system_inherit"
      ? "system"
      : selectedGlobalCredential?.connection === "custom_endpoint" || credential?.kind === "custom_endpoint"
        ? "custom_endpoint"
        : "official";
    setTestingIds((current) => { const next = new Set(current); next.add(seat.id); return next; });
    setTestResults((current) => { const next = { ...current }; delete next[seat.id]; return next; });
    const accountId = credential?.kind === "system_inherit" ? "__system__" : credential?.secret_ref || "";
    const worker = workerProfileSpec(descriptors, seat.engine);
    try {
      const probed: WorkerModelTestResult = !worker
        ? {
            ok: false,
            detail: `引擎描述未加载或不包含 ${seat.engine}，无法构造 Worker 配置`,
            model: seat.model || "",
            engine: seat.engine,
            backend,
            layer: "config",
            logs: [{ stream: "error", message: "请重新加载引擎描述后再检查", elapsed_ms: 0 }],
          }
        : connection === "custom_endpoint" && !seat.model?.trim()
        ? {
            ok: false,
            detail: "自定义 API 缺少模型 ID，无法发起真实模型请求",
            model: "",
            engine: seat.engine,
            backend,
            layer: "config",
            logs: [{ stream: "error", message: "请先填写服务实际支持的模型 ID", elapsed_ms: 0 }],
          }
        : await testWorkerProfileModel(buildModelTestProfile({
            worker,
            id: seat.id,
            label: seat.label,
            engine: engineOf(seat.engine),
            accountId,
            connection,
            baseUrl: selectedGlobalCredential?.base_url || credential?.endpoint?.base_url,
            model: seat.model || "",
            reasoningEffort: seat.reasoning_effort || "default",
          }), seat.model || "", backend);
      const systemLoginPresent = connection === "system"
        && backend === "local"
        && Boolean(selectedGlobalCredential?.present);
      const result: WorkerModelTestResult = !probed.ok && systemLoginPresent
        ? {
            ...probed,
            detail: `已检测到系统登录；真实模型请求失败：${probed.detail}`,
            layer: probed.layer === "auth" ? "model" : probed.layer,
          }
        : probed;
      setTestResults((current) => ({ ...current, [seat.id]: result }));
      setHealth((current) => ({
        ...current,
        [seat.id]: {
          profile_id: seat.id,
          engine: seat.engine,
          backend,
          status: modelTestHealth(result),
          layer: result.layer || (result.ok ? null : "auth"),
          blocker: result.ok ? null : result.detail,
          detail: result.detail,
          model: result.model,
          account_id: accountId === "__system__" ? "" : accountId,
        },
      }));
      if (!quiet) showFeedback(result.ok ? `${seat.label} 自检通过` : `${seat.label} 自检失败，终端已保留日志`);
      return result;
    } finally {
      setTestingIds((current) => { const next = new Set(current); next.delete(seat.id); return next; });
    }
  }, [availableCredentials, backend, credentials, descriptors, showFeedback]);

  const testAllSeats = useCallback(async () => {
    const targets = seats.filter((seat) => seat.enabled && (
      isOrdinarySeat(seat)
      || (review.enabled && review.engine === seat.id)
      || (verifier.enabled && verifier.engine === seat.id)
    ));
    if (!targets.length) { showFeedback("当前没有可检查的启用 Worker"); return; }
    if (backend === "container") {
      const workers = targets.map((seat) => workerProfileSpec(descriptors, seat.engine));
      const missing = targets.find((_, index) => !workers[index]);
      if (missing) { showFeedback(`引擎描述未加载或不包含 ${missing.engine}，无法检查 ${missing.label}`); return; }
      setBatchCheck({ running: true, completed: 0, total: targets.length });
      const requests = targets.map((seat, index) => {
        const credential = credentials.find((item) => item.id === seat.credential_id);
        const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
        const connection = credential?.kind === "system_inherit"
          ? "system"
          : selectedGlobalCredential?.connection === "custom_endpoint" || credential?.kind === "custom_endpoint"
            ? "custom_endpoint"
            : "official";
        const accountId = credential?.kind === "system_inherit" ? "__system__" : credential?.secret_ref || "";
        const profile = buildModelTestProfile({
          worker: workers[index] as ProviderWorkerProfileSpec,
          id: seat.id,
          label: seat.label,
          engine: engineOf(seat.engine),
          accountId,
          connection,
          baseUrl: selectedGlobalCredential?.base_url || credential?.endpoint?.base_url,
          model: seat.model || "",
          reasoningEffort: seat.reasoning_effort || "default",
        });
        return {
          seat,
          accountId,
          item: {
            profile_id: seat.id,
            profile,
            model: seat.model || "",
            reasoning_effort: seat.reasoning_effort || "default",
          },
        };
      });
      setTestingIds((current) => {
        const next = new Set(current);
        targets.forEach((seat) => next.add(seat.id));
        return next;
      });
      setTestResults((current) => {
        const next = { ...current };
        targets.forEach((seat) => { delete next[seat.id]; });
        return next;
      });
      try {
        const batch = await testWorkerProfileModelsBatch(requests.map((request) => request.item), backend);
        setTestResults((current) => {
          const next = { ...current };
          requests.forEach((request, index) => { next[request.seat.id] = batch.results[index]; });
          return next;
        });
        setHealth((current) => {
          const next = { ...current };
          requests.forEach((request, index) => {
            const result = batch.results[index];
            next[request.seat.id] = {
              profile_id: request.seat.id,
              engine: request.seat.engine,
              backend: result.backend || batch.backend,
              status: modelTestHealth(result),
              layer: result.layer || (result.ok ? null : "auth"),
              blocker: result.ok ? null : result.detail,
              detail: result.detail,
              model: result.model,
              account_id: request.accountId === "__system__" ? "" : request.accountId,
            };
          });
          return next;
        });
        const passed = batch.results.filter((result) => result.ok).length;
        setBatchCheck({ running: false, completed: targets.length, total: targets.length });
        showFeedback(`一键检查完成：${passed}/${targets.length} 个 Worker 通过真实模型请求 · 使用 ${batch.container_count} 个批量检查容器`);
      } finally {
        setTestingIds((current) => {
          const next = new Set(current);
          targets.forEach((seat) => next.delete(seat.id));
          return next;
        });
      }
      return;
    }
    setBatchCheck({ running: true, completed: 0, total: targets.length });
    const results = await Promise.all(targets.map(async (seat) => {
      const result = await testSeat(seat, true);
      setBatchCheck((current) => ({ ...current, completed: current.completed + 1 }));
      return result;
    }));
    const passed = results.filter((result) => result.ok).length;
    setBatchCheck({ running: false, completed: targets.length, total: targets.length });
    showFeedback(`一键检查完成：${passed}/${targets.length} 个 Worker 通过真实模型请求`);
  }, [availableCredentials, backend, credentials, descriptors, review.enabled, review.engine, verifier.enabled, verifier.engine, seats, showFeedback, testSeat]);

  const saveTemplates = useCallback(async (next: DispatchTemplate[], notice: string): Promise<string | null> => {
    try {
      const saved = await putWorkerSettings({ dispatch_templates: next });
      if (!saved) return "服务未返回生效配置";
      setTemplates(saved.dispatch_templates ?? next);
      showFeedback(notice);
      return null;
    } catch (error) {
      return `保存失败：${error instanceof Error ? error.message : String(error)}`;
    }
  }, [showFeedback]);
  const templateSourceSeats = useMemo(() => seats.filter((seat) => isOrdinarySeat(seat) && seat.enabled).map((seat) => ({
    ...seat,
    credential_id: globalCredentialForLegacy(credentials.find((item) => item.id === seat.credential_id), availableCredentials)?.id || seat.credential_id,
  })), [availableCredentials, credentials, seats]);

  const save = async () => {
    if (!config) return;
    const plannerId = llmProfiles.planner.endpoint_id || llmProfiles.planner.credential_id;
    const plannerEndpoint = modelEndpoints.find((endpoint) => endpoint.id === plannerId);
    if (plannerId && (!plannerEndpoint || (plannerEndpoint.worker_engine !== "pi" && plannerEndpoint.engine !== "pi"))) {
      setSaveState("error"); showFeedback("Planner 需要 Pi 账号；请在模型设置中重新选择 Pi 账号端点"); return;
    }
    const enabledOrdinary = seats.filter((seat) => isOrdinarySeat(seat) && seat.enabled);
    const activeSeatsForCheck = seats.filter((seat) => seat.enabled && (
      isOrdinarySeat(seat)
      || (review.enabled && review.engine === seat.id)
      || (verifier.enabled && verifier.engine === seat.id)
    ));
    for (const seat of activeSeatsForCheck) {
      const verIssue = workerVersionIssue(versionPolicy.data, seat.engine, backend, activeMode, effectiveNodeId);
      if (verIssue) {
        setSaveState("error");
        showFeedback(`「${seat.label}」${verIssue.detail}`);
        return;
      }
    }
    if (review.enabled && !seats.some((seat) => seat.id === review.engine && seat.enabled && canServeChannel(seat, "review"))) { setSaveState("error"); showFeedback("Review 已启用，但没有指定可用 Worker"); return; }
    if (verifier.enabled && !seats.some((seat) => seat.id === verifier.engine && seat.enabled && canServeChannel(seat, "verifier"))) { setSaveState("error"); showFeedback("Verifier 已启用，但没有指定可用 Worker"); return; }
    const invalidTemperature = (["planner", "titler"] as const).find((which) => {
      const profile = llmProfiles[which];
      if (llmTemperatureMode(profile) !== "custom") return false;
      const value = Number(profile.temperature);
      return !Number.isFinite(value) || value < 0 || value > 2;
    });
    if (invalidTemperature) { setSaveState("error"); showFeedback(`${invalidTemperature === "planner" ? "Reason / Planner" : "Titler"} 的 Temperature 需在 0 到 2 之间`); return; }
    const badQuota = invalidQuota(quotas);
    if (badQuota) { setSaveState("error"); showFeedback(`容器配额“${badQuota}”格式不正确`); return; }
    setSaveState("saving");
    const normalizedSeats = seats.map((seat, index) => ({
      ...seat,
      priority: (index + 1) * 10,
      roles: normalizeSeatRoles(seat.roles),
      capacity: { ...seat.capacity },
    }));
    const refs = enabledOrdinary.map((seat) => seat.id);
    let saved: WorkerSettings | null = null;
    try {
      saved = await putWorkerSettings({
      seats: normalizedSeats,
      engines: refs,
      max_workers: Math.max(1, maxWorkers),
      worker_backend: backend,
      worker_node_id: backend === "container" ? nodeId : "local",
      worker_network: network,
      worker_container_scope: containerScope,
      worker_privilege: "root",
      worker_oci_runtime: ociRuntime,
      ...quotaPayload(quotas),
      wall_clock_budget: wallClock,
      max_total_workers: maxTotal,
      cost_budget_usd: costBudget,
      llm_profiles: llmProfiles,
      stage_policy: {
        ...config.stage_policy,
        coordinator: {
          ...config.stage_policy.coordinator,
          wall_clock_budget: wallClock,
          review: { ...review, max_concurrent: 1 },
          verifier: {
            ...verifier,
            max_concurrent: Math.max(0, verifier.max_concurrent ?? 0),
          },
        },
        budgets: { max_total_workers: maxTotal, cost_budget_usd: costBudget },
      },
      });
    } catch (error) {
      setSaveState("error");
      showFeedback(`保存失败：${error instanceof Error ? error.message : String(error)}`);
      return;
    }
    if (!saved) { setSaveState("error"); showFeedback("保存失败：服务未返回生效配置"); return; }
    const savedIdentity = legacyIdentity(saved, descriptors);
    const savedDraft: WorkerDraftSnapshot = {
      seats: savedIdentity.seats,
      review: { ...DEFAULT_REVIEW, ...(saved.stage_policy.coordinator.review || {}) },
      verifier: { ...DEFAULT_VERIFIER, ...(saved.stage_policy.coordinator.verifier || {}) },
      backend: saved.worker_backend || "local",
      nodeId: saved.worker_node_id || "local",
      network: saved.worker_network || "bridge",
      containerScope: saved.worker_container_scope || "run",
      ociRuntime: saved.worker_oci_runtime || "",
      quotas: quotasOf(saved),
      maxTotal: saved.max_total_workers,
      wallClock: saved.wall_clock_budget,
      costBudget: saved.cost_budget_usd,
      llmProfiles: saved.llm_profiles,
    };
    const editedWhileSaving = currentDraftSignatureRef.current !== currentDraftSignature;
    setConfig(saved);
    baselineDraftRef.current = workerDraftSignature(savedDraft);
    if (!editedWhileSaving) {
      setSeats(savedDraft.seats);
      setCredentials(syncCredentialsFromGlobal(savedIdentity.credentials, availableCredentials, descriptors));
      setReview(savedDraft.review);
      setVerifier(savedDraft.verifier);
      setBackend(savedDraft.backend);
      setNodeId(savedDraft.nodeId);
      setOciRuntime(savedDraft.ociRuntime);
      setQuotas(savedDraft.quotas);
      setNetwork(savedDraft.network === "host" ? "host" : "bridge");
      setEffectiveNetwork(saved.effective_network || savedDraft.network);
      setNetworkError(saved.network_error || "");
      setContainerScope(savedDraft.containerScope);
      setMaxTotal(savedDraft.maxTotal);
      setWallClock(savedDraft.wallClock);
      setCostBudget(savedDraft.costBudget);
      setLlmProfiles(savedDraft.llmProfiles);
    }
    setDirty(editedWhileSaving && currentDraftSignatureRef.current !== baselineDraftRef.current);
    setSaveState("saved");
    showFeedback(editedWhileSaving ? "提交时的配置已保存，之后的修改仍未保存" : "Worker 阵容、Review 与 Verifier 配置已保存");
    await refreshHealth();
  };

  const latestRef = useRef({ seats, credentials, availableCredentials, testSeat, save, dirty, saveState });
  latestRef.current = { seats, credentials, availableCredentials, testSeat, save, dirty, saveState };

  useEffect(() => {
    try { setAutoCheck(window.localStorage.getItem(AUTO_CHECK_KEY) !== "off"); } catch {  }
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    listRunnerNodes(controller.signal).then(setRunnerNodes).catch(() => { if (!controller.signal.aborted) setRunnerNodes([]); });
    return () => controller.abort();
  }, [reloadRevision]);
  const changeAutoCheck = useCallback((next: boolean) => {
    setAutoCheck(next);
    try { window.localStorage.setItem(AUTO_CHECK_KEY, next ? "on" : "off"); } catch {  }
  }, []);

  useEffect(() => {
    const signature = (seat: Seat) => [seat.engine, seat.credential_id, seat.model, seat.reasoning_effort, seat.enabled].join("|");
    const next = new Map(seats.map((seat) => [seat.id, signature(seat)]));
    const previous = autoCheckSeenRef.current;
    autoCheckSeenRef.current = next;
    if (!previous || !autoCheck) return;
    const timers = autoCheckTimersRef.current;
    for (const seat of seats) {
      if (previous.get(seat.id) === next.get(seat.id)) continue;
      window.clearTimeout(timers.get(seat.id));
      timers.delete(seat.id);
      if (!seat.enabled || !seat.credential_id) continue;
      timers.set(seat.id, window.setTimeout(() => {
        timers.delete(seat.id);
        const latest = latestRef.current;
        const current = latest.seats.find((item) => item.id === seat.id);
        if (!current?.enabled || !current.credential_id) return;
        const global = globalCredentialForLegacy(latest.credentials.find((item) => item.id === current.credential_id), latest.availableCredentials);
        if (!globalCredentialUsable(global)) return;
        if (global.connection === "custom_endpoint" && !current.model?.trim()) return;
        void latest.testSeat(current, true);
      }, 800));
    }
  }, [autoCheck, seats]);
  useEffect(() => () => { autoCheckTimersRef.current.forEach((timer) => window.clearTimeout(timer)); }, []);

  useEffect(() => {
    if (!highlightId) return;
    const timer = window.setTimeout(() => setHighlightId(null), 2400);
    return () => window.clearTimeout(timer);
  }, [highlightId]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.key.toLocaleLowerCase("en-US") !== "s") return;
      const { dirty: pending, saveState: state, save: commit } = latestRef.current;
      if (!pending) return;
      event.preventDefault();
      if (state !== "saving") void commit();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  useEffect(() => {
    if (!dirty) return;
    const guard = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    window.addEventListener("beforeunload", guard);
    return () => window.removeEventListener("beforeunload", guard);
  }, [dirty]);

  const discardChanges = useCallback(() => {
    setSaveState("idle");
    setReloadRevision((current) => current + 1);
  }, []);

  const refreshCredentials = (action?: "save" | "import" | "create" | "delete") => {
    void getGlobalCredentials().then((rows) => {
      setAvailableCredentials(rows);
      setCredentials((current) => syncCredentialsFromGlobal(current, rows, descriptors));
    }).catch((error) => showFeedback(`刷新凭据失败：${error instanceof Error ? error.message : String(error)}`));
    if (action === "delete" && !dirty) setReloadRevision((current) => current + 1);
  };
  const selectSection = (next: SettingsSection) => {
    if (section === "credentials" && next !== "credentials") refreshCredentials();
    setSection(next);
    const url = new URL(window.location.href);
    url.searchParams.set("section", next);
    window.history.replaceState(window.history.state, "", url);
  };
  const titles: Record<SettingsSection, string> = {
    roster: "出战配置",
    local_pi: "本地 Pi 单会话",
    guardrail: "执行护栏",
    versions: "版本要求",
    credentials: "Agent 凭据",
    runtime: "运行环境",
    nodes: "执行节点",
    scheduling: "调度与预算",
    models: "推理模型",
    retention: "任务保留",
  };
  const item = (id: SettingsSection, icon: SettingsNavGroup["items"][number]["icon"]) => {
    const params = new URLSearchParams(searchParams);
    params.set("section", id);
    return { id, label: titles[id], icon, href: `?${params}` };
  };
  const navGroups: SettingsNavGroup[] = [
    { id: "dispatch", label: "下发配置（蜂群必填）", items: [item("roster", "grid"), item("models", "sparkles")] },
    { id: "agents", label: "Agent 与运行环境", items: [
      item("credentials", "lock"), item("versions", "shield"), item("runtime", "terminal"), item("nodes", "network"),
    ] },
    { id: "execution", label: "执行与调度", items: [
      item(activeMode === "pentest" ? "guardrail" : "local_pi", activeMode === "pentest" ? "shieldAlert" : "terminal"),
      item("scheduling", "gear"),
    ] },
    { id: "data", label: "数据管理", items: [item("retention", "archive")] },
  ];

  if (!config && initialLoadError) return (
    <div className="wsettings-skeleton" role="alert">
      <Alert tone="danger" title="Worker 配置读取失败" className="wruntime-note">
        {initialLoadError}
      </Alert>
      <Button size="sm" variant="secondary" icon="refresh" onClick={() => {
        setInitialLoadError("");
        setReloadRevision((current) => current + 1);
      }}>重试</Button>
    </div>
  );

  if (!config) return (
    <div className="wsettings-skeleton" role="status" aria-label="正在读取 Worker 配置">
      <div className="wsettings-skel-rail">
        <Skeleton className="sk-brand" />
        <Skeleton className="sk-label" />
        <Skeleton className="sk-row" /><Skeleton className="sk-row" /><Skeleton className="sk-row" /><Skeleton className="sk-row" />
      </div>
      <div className="wsettings-skel-main">
        <div className="wsettings-skel-top"><Skeleton className="sk-title" /><Skeleton className="sk-btn" /></div>
        <div className="wsettings-skel-grid"><Skeleton className="sk-card" /><Skeleton className="sk-card" /><Skeleton className="sk-card" /><Skeleton className="sk-card" /></div>
        <span className="wsettings-skel-note">正在读取 Worker 配置…</span>
      </div>
    </div>
  );

  return (
    <div className="cx-root cx-settings-shell workspace-settings" data-section={section} onClickCapture={(event) => {
      if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const target = event.target;
      const sectionLink = target instanceof Element ? target.closest('a[href^="?"]') : null;
      if (sectionLink) {
        const next = new URLSearchParams(sectionLink.getAttribute("href")!.slice(1)).get("section");
        if (next && navGroups.some(group => group.items.some(item => item.id === next)) && isWorkerSection(next)) {
          event.preventDefault();
          selectSection(next);
          return;
        }
      }
      const link = target instanceof Element ? target.closest('a[href="/ctf/workers?section=credentials"]') : null;
      if (!link) return;
      event.preventDefault();
      selectSection("credentials");
    }}>
      <SettingsNav title={workspaceLabels.title} back={{ label: workspaceLabels.back, href: returnTo }} collapsible
        groups={navGroups}
        activeId={section} link={WorkspaceSettingsLink}
        onNavigate={href => selectSection(new URLSearchParams(href.slice(1)).get("section") as SettingsSection)}
        labels={{ search: "搜索设置", clear: "清除搜索", empty: "没有匹配的设置", nav: "工作台设置" }} />

      <AddWorkerDialog
        open={addingWorker && section === "roster"} onOpenChange={setAddingWorker}
        seats={seats} availableCredentials={availableCredentials} models={models} backend={backend}
        mode={activeMode} nodeId={effectiveNodeId}
        onAdd={addSeat} onOpenCredentials={() => selectSection("credentials")} onOpenRuntime={() => selectSection("runtime")}
        onOpenVersions={() => { setAddingWorker(false); selectSection("versions"); }}
      />

      <main className="cx-settings-content workspace-settings-main" >
        {section === "retention" || section === "credentials" ? null : (
          <SettingsPage
            wide
            title={titles[section]}
            description={
              section === "roster"
                ? "选择参与任务的 Worker，配置连接、模型和辅助角色。保存后用于下次任务。"
                : section === "local_pi"
                  ? "CTF 本地 Pi 单会话的默认凭据、模型与 Skills，新建任务时自动带入。"
                : section === "guardrail"
                  ? "为开启生产保护模式的渗透测试配置 Cronus-9B 实时风险评估与人工审批护栏。"
                : section === "versions"
                  ? "以当前宿主机已安装的 Agent CLI 版本作为最低支持基准。低于最低版本的引擎禁止启用为出战 Worker，并提供升级指引；此页设置立即生效。"
                  : section === "runtime"
                    ? "选择 Worker 的执行位置。容器运行时可进一步配置执行节点、隔离、网络、配额与镜像。"
                    : section === "nodes"
                      ? "管理运行 Worker 容器的机器：本机，以及通过 HTTPS 接入的远程 Runner。这里的操作立即生效，不需要点保存。"
                      : section === "scheduling"
                        ? "配置单个任务可使用的时间、次数和成本。保存后用于下次任务。"
                        : "为规划决策与标题生成配置模型和接入点。保存后用于下次任务。"
            }
            actions={
              section === "versions" || section === "nodes" || section === "local_pi" || section === "guardrail"
                ? undefined
                : <span className="text-cx-sm text-cx-fg-3" role="status">{dirty ? "未保存" : "已保存"}</span>
            }
          >
            <span hidden />
          </SettingsPage>
        )}


        <div className="wsettings-content">
          {section === "roster" ? <div className="wsettings-orchestration roster-overview flex flex-col gap-8">
            <DispatchTemplates
              templates={templates} currentSeats={templateSourceSeats} availableCredentials={availableCredentials} models={models}
              backend={backend} mode={activeMode} nodeId={effectiveNodeId} onSave={saveTemplates}
              onOpenCredentials={() => selectSection("credentials")}
            />
            <WorkerRoster
              seats={seats} credentials={credentials} availableCredentials={availableCredentials} models={models} backend={backend}
              mode={activeMode} nodeId={effectiveNodeId}
              review={review} verifier={verifier} testingIds={testingIds} testResults={testResults} batchCheck={batchCheck}
              discoveringModels={discoveringModels} autoCheck={autoCheck} highlightId={highlightId} onAutoCheck={changeAutoCheck}
              onAddWorker={() => setAddingWorker(true)} onUpdateSeat={updateSeat} onBindAccount={bindAccount} onChangeEngine={changeEngine}
              onDuplicate={duplicateSeat} onDelete={deleteSeat} onReorder={reorderSeats} onToggleEnabled={toggleSeatEnabled}
              onDisableSeats={disableSeats}
              onReview={updateReview} onVerifier={updateVerifier} onSetReview={setReviewSeat} onSetVerifier={setVerifierSeat}
              onCreateDedicatedReview={createDedicatedReview} onCreateDedicatedVerifier={createDedicatedVerifier}
              onTest={(seat) => void testSeat(seat)} onTestAll={() => void testAllSeats()}
              onDiscoverModels={refreshWorkerModels} onOpenCredentials={() => selectSection("credentials")}
              onOpenVersions={() => selectSection("versions")} onOpenRuntime={() => selectSection("runtime")}
              onNotice={showFeedback}
            />
          </div>
            : section === "versions" ? (
              <SectionFrame>
                <AgentVersionSupportSection
                  focusMode={activeMode}
                  activeBackend={backend}
                  onOpenRuntime={() => selectSection("runtime")}
                />
              </SectionFrame>
            )
            : section === "local_pi" ? <SectionFrame><LocalPiSection onNotice={showFeedback} /></SectionFrame>
            : section === "guardrail" ? <SectionFrame><ExecutionGuardrailSection onNotice={showFeedback} /></SectionFrame>
            : section === "credentials" ? <div className="wsettings-credentials cx-root cx-settings-content"><SettingsPage wide title="Agent 凭据" description="管理做题 Worker 使用的 Agent 引擎、凭据、运行配置和模型。"><TaskCredentialManager taskSettings onCredentialsChanged={refreshCredentials} /></SettingsPage></div>
            : section === "runtime" ? <SectionFrame><RuntimeSection backend={backend} network={network} effectiveNetwork={effectiveNetwork} networkError={networkError} containerScope={containerScope} seatCount={seats.length} imageStatus={imageStatus} imageLoading={imageLoading} pulling={pullingImage} inContainer={inContainer}
              nodeId={nodeId} nodes={runnerNodes} ociRuntime={ociRuntime} quotas={quotas} quotaDefaults={config.worker_resource_effective} migrationNotices={config.migration_notices ?? []}
              onNodeId={(next) => { setNodeId(next); setTestResults({}); markDirty(); }} onOciRuntime={(next) => { setOciRuntime(next); markDirty(); }}
              onQuota={(key, value) => { setQuotas((current) => ({ ...current, [key]: value })); markDirty(); }} onOpenNodes={() => selectSection("nodes")}
              onBackend={(next) => { setBackend(next); if (next === "local") setNodeId("local"); setTestResults({}); markDirty(); if (next === "container" && !imageStatus) void refreshImage(); }} onNetwork={(next) => { setNetwork(next); setTestResults({}); markDirty(); }} onContainerScope={(next) => { setContainerScope(next); markDirty(); }} onRefreshImage={() => void refreshImage()} onPullImage={() => void pullImage()} /></SectionFrame>
                : section === "nodes" ? <SectionFrame><NodesSection selectedNodeId={backend === "container" ? nodeId : "local"} onNodesChanged={setRunnerNodes} /></SectionFrame>
                : section === "scheduling" ? <SectionFrame><SchedulingSection config={config} mode={activeMode} maxTotal={maxTotal} wallClock={wallClock} costBudget={costBudget} maxWorkers={maxWorkers} onChange={(patch) => { if (patch.maxTotal !== undefined) setMaxTotal(patch.maxTotal); if (patch.wallClock !== undefined) setWallClock(patch.wallClock); if (patch.costBudget !== undefined) setCostBudget(patch.costBudget); markDirty(); }} /></SectionFrame>
                  : section === "models" ? <SectionFrame><ModelsSection value={llmProfiles} endpoints={modelEndpoints} onChange={(next) => { setLlmProfiles(next); markDirty(); }} onEndpointSaved={(endpoint) => setModelEndpoints((current) => [...current.filter((item) => item.id !== endpoint.id), endpoint])} /></SectionFrame>
                    : <RetentionSettings wide mode={pathname.startsWith("/pentest") ? "pentest" : "ctf"} />}
        </div>
        <SaveBar
          dirty={dirty && section !== "credentials" && section !== "retention" && section !== "versions" && section !== "local_pi" && section !== "guardrail"}
          saving={saveState === "saving"}
          failed={saveState === "error"}
          notice={feedback || undefined}
          onSave={() => void save()}
          onDiscard={discardChanges}
        />
      </main>
    </div>
  );
}
