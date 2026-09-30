"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, Checkbox, Input, Label, ListBox, ListBoxItem, Modal, Select, Switch, Tabs, TextArea, TextField } from "@heroui/react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon, type IconName } from "@/components/Icon";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { ConversationModelPicker } from "@/components/conversation/ConversationModelPicker";
import { type ChatDefaultModel, readChatDefaultModel, writeChatDefaultModel } from "@/lib/conversationDefaults";
import { normalizeConversationCredential, type ConversationCredential } from "@/lib/useConversation";
import { recordTaskReceipt } from "@/lib/task-center";
import {
  type GlobalCredential, type WorkerModelTestResult, type WorkerSettings, apiFetch, deleteCredentialAccount,
  getGlobalCredentials, getWorkerSettings, importHostCodexAuth, importHostWorkerLogin,
  putCredentialAccount, refreshCredentialModels, testGlobalCredential,
} from "@/lib/useRun";

type Engine = "claude" | "codex" | "cursor" | "pi" | "omp" | "kimi" | "grok" | "opencode" | "devin" | "dsh";
type Connection = "official" | "custom_endpoint";
type DetailTab = "credentials" | "runtime" | "capabilities";
type StatusKind = "ok" | "warn" | "bad" | "disabled";
type Feedback = { kind: "ok" | "warn" | "bad"; detail: string } | null;
type ModelRefreshFeedback = { kind: "ok" | "bad"; detail: string } | null;
type TransportField = { type?: "string" | "boolean" | "number" | "integer"; title?: string; default?: string | number | boolean };
type VersionCheck = {
  installed_version: string; latest_version: string;
  status: "current" | "update_available" | "unknown";
  update_available: boolean; checked_at?: string; attempted_at?: string;
  detail: string; source?: string; stale?: boolean;
};
type RuntimeInstance = {
  key: string; adapter_id: string; instance_id: string; engine: string; label: string;
  updated_at?: string; binary_path: string; endpoint: string; transport: Record<string, string | number | boolean>;
  config_schema?: { properties?: { transport?: { properties?: Record<string, TransportField> } } };
  enabled: boolean; discovered: boolean; configured: boolean;
  auth: { status: "ok" | "missing" | "unknown" | "unavailable" | "not_applicable"; detail: string; login_command?: string; code?: string };
  health: null | { healthy: boolean; detail: string; runtime_version: string; probed_at?: string; version_check?: VersionCheck; capabilities: Record<string, unknown>; degradations: string[] };
};
type EngineSummary = {
  engine: Engine; label: string; credentials: GlobalCredential[]; instances: RuntimeInstance[];
  primaryCredential: GlobalCredential | null; primaryInstance: RuntimeInstance | null;
  enabled: boolean; version: string; readyCredentialCount: number; modelCount: number;
  environmentCount: number; statusKind: StatusKind; statusText: string;
  supportStatus: "supported" | "temporarily_disabled"; disabledReason: string;
};
type EngineDescriptor = {
  id: Engine; display_name: string; support_status: "supported" | "temporarily_disabled";
  disabled_reason: string; configurable: boolean; executable: boolean; enabled: boolean;
};
type CredentialDraft = {
  accountId: string; connection: Connection; provider: string; baseUrl: string;
  secret: string; defaultModel: string; modelsText: string;
};

const ENGINES: Engine[] = ["codex", "claude", "cursor", "grok", "opencode", "pi", "kimi", "omp", "devin", "dsh"];
const ENGINE_LABELS: Record<Engine, string> = {
  codex: "Codex", claude: "Claude Code", cursor: "Cursor", grok: "Grok", opencode: "OpenCode",
  pi: "Pi", kimi: "Kimi Code", omp: "OMP", devin: "Devin CLI", dsh: "DeepSeek Harness",
};
const ENGINE_ADAPTERS: Record<Engine, string> = {
  codex: "cli.codex", claude: "cli.claude", cursor: "cli.cursor", grok: "cli.grok",
  opencode: "cli.opencode", pi: "cli.pi", kimi: "cli.kimi", omp: "cli.omp", devin: "cli.devin", dsh: "",
};
const IMPORTABLE_ENGINES = new Set<Engine>(["claude", "codex", "kimi", "grok"]);
const FALLBACK_TRANSPORT_FIELDS: Record<string, Record<string, TransportField>> = {
  "codex.app_server": { codex_home: { title: "隔离配置目录" }, experimental_api: { type: "boolean", title: "Experimental API", default: false } },
  "pi.rpc": { session_dir: { title: "会话目录" } }, "omp.rpc_v2": { session_dir: { title: "会话目录" } },
};
const CAPABILITY_LABELS: Record<string, string> = {
  streaming: "流式输出", resume: "会话恢复", steer: "过程转向", interrupt: "即时中断", approval: "权限审批",
  user_input: "用户输入", fork: "分支复制", structured_output: "结构化输出", subagents: "子代理派发",
  skills: "技能支持", mcp: "MCP 协议", native_tool_binding: "原生工具调用", tool_events: "工具事件",
  usage_events: "用量事件", session_persistence: "会话持久化", plan: "结构化计划",
};
const EMPTY_DRAFT: CredentialDraft = { accountId: "", connection: "official", provider: "", baseUrl: "", secret: "", defaultModel: "", modelsText: "" };
const AUTO_REFRESH_MS = 300_000;
const DSH_DISABLED_REASON = "DeepSeek Harness 上游 CLI 暂未提供 Worker 所需的结构化事件、工具结果和会话恢复能力，因此暂不支持。待上游完善后开放。";
const FALLBACK_ENGINE_DESCRIPTORS: EngineDescriptor[] = ENGINES.map((id) => ({
  id,
  display_name: ENGINE_LABELS[id],
  support_status: id === "dsh" ? "temporarily_disabled" : "supported",
  disabled_reason: id === "dsh" ? DSH_DISABLED_REASON : "",
  configurable: id !== "dsh",
  executable: id !== "dsh",
  enabled: id !== "dsh",
}));

function asEngine(value?: string): Engine | null {
  const normalized = (value || "").toLowerCase().replaceAll("_", "-");
  if (normalized === "deepseek" || normalized === "deepseek-harness") return "dsh";
  return ENGINES.includes(normalized as Engine) ? normalized as Engine : null;
}
function splitModels(value: string) { return [...new Set(value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean))]; }
function credentialModels(item: GlobalCredential) {
  return [...new Set([item.default_model || "", ...(item.models || []), ...(item.candidate_models || [])].map(String).map((model) => model.trim()).filter(Boolean))];
}
function credentialReady(item: GlobalCredential) { return item.present && ["ready", "untested"].includes(item.status); }
function credentialName(item: GlobalCredential) {
  return item.source === "system" ? "本机宿主登录" : item.label || item.account_id || item.id.replace(/^account:/, "");
}
function runtimeLabel(instance: RuntimeInstance | null) {
  const path = (instance?.binary_path || "").trim();
  const parts = path.split(/[\\/]/).filter(Boolean);
  const binary = parts[parts.length - 1];
  if (binary) return binary.includes("cli") ? binary : `${binary} CLI`;
  const adapter = instance?.adapter_id.replace(/^cli\./, "").trim();
  return adapter ? `${adapter} CLI` : "本机 CLI";
}
function installedVersion(version: string, check?: VersionCheck) {
  if (check?.installed_version) return check.installed_version;
  return version.match(/\d+(?:\.\d+){1,3}(?:-[0-9a-z._-]+)?/i)?.[0] || "未探测";
}
function versionStatus(check?: VersionCheck) {
  if (!check) return "尚未检查最新版本";
  if (check.status === "update_available" && check.latest_version) {
    return `${check.stale ? "上次结果：" : ""}可更新至 ${check.latest_version}`;
  }
  if (check.status === "current") return check.stale ? "上次检查为最新" : "已是最新";
  return "最新版本未获取";
}
function VersionCell({ version, check }: { version: string; check?: VersionCheck }) {
  return <div className="agent-registry-version" data-tooltip={versionCheckDetail(check)}><strong>{installedVersion(version, check)}</strong><span className={`is-${check?.status || "unknown"}`}>{versionStatus(check)}</span></div>;
}
function versionCheckDetail(check?: VersionCheck) {
  if (!check) return "最新版本尚未检查。";
  const timestamp = check.attempted_at || check.checked_at;
  const date = timestamp ? new Date(timestamp) : null;
  const checkedAt = date && !Number.isNaN(date.getTime())
    ? date.toLocaleString("zh-CN", { hour12: false })
    : "时间未知";
  const source = check.source ? `；来源 ${check.source}` : "";
  return `${check.detail || "最新版本未获取"}${source}；检查时间 ${checkedAt}。只检测版本，不执行更新。`;
}
function StatusPill({ kind, children }: { kind: StatusKind; children: string }) {
  return <span className={`agent-registry-status is-${kind}`}><span aria-hidden="true" />{children}</span>;
}

function catalogSummary(catalog?: GlobalCredential["model_catalog"]) {
  if (!catalog) return "尚未获取该凭据的模型目录。";
  const source = catalog.source ? `来源 ${catalog.source}` : "来源未记录";
  const refreshed = catalog.refreshed_at
    ? new Date(catalog.refreshed_at * 1000).toLocaleString("zh-CN", { hour12: false })
    : "尚未成功刷新";
  if (catalog.refresh_status === "failed") {
    return `模型目录更新失败：${catalog.last_error || "目录请求失败"}`;
  }
  if (catalog.refresh_status === "stale") {
    return `沿用该凭据自己的上次目录（已过期）· ${source} · ${catalog.last_error || refreshed}`;
  }
  if (catalog.refresh_status === "fresh") return `模型目录已更新 · ${source} · ${refreshed}`;
  return `尚未获取该凭据的模型目录 · ${source}`;
}
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
  const models = splitModels(value);
  const statusKind = refreshing ? "loading" : feedback?.kind || catalog?.refresh_status || "missing";
  const statusText = refreshing
    ? "正在更新候选模型…"
    : feedback?.detail || catalogSummary(catalog);
  return (
    <div className="is-wide agent-registry-models-field" aria-busy={refreshing}>
      <div className="agent-registry-models-field-head">
        <span>候选模型清单</span>
        <Button size="sm" variant="outline" className="agent-registry-button" isDisabled={refreshing} onPress={onRefresh}>
          <Icon name="refresh" size={13} className={refreshing ? "spin" : ""} />
          {refreshing ? "更新中…" : "一键更新"}
        </Button>
      </div>
      <details className="agent-registry-models-disclosure">
        <summary>
          <span>{models.length ? `${models.length} 个候选模型` : "尚无候选模型"}</span>
          <Icon name="chevronDown" size={14} />
        </summary>
        {models.length ? (
          <ul aria-label="候选模型清单">
            {models.map((model) => <li key={model}><code>{model}</code></li>)}
          </ul>
        ) : (
          <p>点击“一键更新”从当前凭据读取模型目录。</p>
        )}
      </details>
      <small
        className={`agent-registry-models-field-status is-${statusKind}`}
        role={feedback?.kind === "bad" ? "alert" : "status"}
        aria-live={feedback?.kind === "bad" ? "assertive" : "polite"}
      >
        <Icon
          name={refreshing ? "refresh" : feedback?.kind === "ok" ? "checkCircle" : feedback?.kind === "bad" ? "alert" : "clock"}
          size={12}
          className={refreshing ? "spin" : ""}
        />
        <span>{statusText}</span>
      </small>
    </div>
  );
}

function SystemLoginPanel({
  ready,
  importable,
  importAccountId,
  onImportAccountId,
  importing,
  onImport,
  models,
  verifiedModels,
  selectedModel,
  onSelectModel,
  catalog,
}: {
  ready: boolean;
  importable: boolean;
  importAccountId: string;
  onImportAccountId: (value: string) => void;
  importing: boolean;
  onImport: () => void;
  models: string[];
  verifiedModels: string[];
  selectedModel: string;
  onSelectModel: (model: string) => void;
  catalog?: GlobalCredential["model_catalog"];
}) {
  return (
    <>
      <div className="agent-registry-system-login">
        <div>
          <Icon name={ready ? "checkCircle" : "info"} size={18} />
          <div>
            <strong>{ready ? "本机 CLI 登录可用" : "未检测到可用的本机 CLI 登录"}</strong>
            <p>{ready ? "本地 Worker 可以直接使用。容器 Worker 需要导入为独立凭据。" : "请先完成该 CLI 的登录，再刷新探测。"}</p>
          </div>
        </div>
        {importable ? (
          <div className="agent-registry-inline-form">
            <TextField value={importAccountId} onChange={onImportAccountId}>
              <Label>导入后的凭据 ID</Label>
              <Input />
            </TextField>
            <Button size="sm" variant="outline" className="agent-registry-button" isDisabled={importing || !ready} onPress={onImport}>
              <Icon name="upload" size={14} />{importing ? "导入中…" : "导入为独立凭据"}
            </Button>
          </div>
        ) : null}
      </div>
      <section className="agent-registry-system-models" aria-labelledby="agent-system-models-title">
        <div>
          <h4 id="agent-system-models-title">可用模型</h4>
          <p>{ready ? `来自本机 CLI 与引擎目录。选中的模型会用于真实连通测试。${catalogSummary(catalog)}` : "本机登录可用后，会列出该引擎当前可用的模型。"}</p>
        </div>
        {models.length ? (
          <div className="agent-registry-chips" role="list">
            {models.map((model) => (
              <Button
                size="sm"
                variant={selectedModel === model ? "primary" : "outline"}
                key={model}
                className={`agent-registry-model-chip${selectedModel === model ? " is-selected" : ""}${verifiedModels.includes(model) ? " is-verified" : ""}`}
                aria-pressed={selectedModel === model}
                onPress={() => onSelectModel(model)}
              >
                <code>{model}</code>
                {verifiedModels.includes(model) ? <small>已测通</small> : null}
              </Button>
            ))}
          </div>
        ) : (
          <p className="agent-registry-system-models-empty">尚未发现可用模型。</p>
        )}
      </section>
    </>
  );
}

function lastTestAsResult(item: GlobalCredential, engine: string): WorkerModelTestResult | null {
  const last = item.last_test;
  if (!last) return null;
  const ok = last.ok ?? last.status === "ok";
  const detail = last.detail || (ok ? "真实连通测试完成" : "真实连通测试失败");
  const testedAt = typeof last.tested_at === "number"
    ? last.tested_at
    : last.tested_at
      ? Date.parse(String(last.tested_at)) / 1000
      : undefined;
  return {
    ok,
    detail,
    model: last.model || "",
    engine,
    backend: last.backend === "container" ? "container" : "local",
    tested_at: Number.isFinite(testedAt) ? testedAt : undefined,
    logs: [{ stream: ok ? "success" : "error", message: detail, elapsed_ms: 0 }],
  };
}

function credentialLiveStatus(
  item: GlobalCredential,
  live: WorkerModelTestResult | null | undefined,
  testing: boolean,
): { kind: StatusKind; text: string } {
  if (testing) return { kind: "warn", text: "连通测试中" };
  if (live) return live.ok ? { kind: "ok", text: "连通可用" } : { kind: "bad", text: "连通失败" };
  const last = item.last_test;
  if (last) {
    const ok = last.ok ?? last.status === "ok";
    return ok ? { kind: "ok", text: "连通可用" } : { kind: "bad", text: "连通失败" };
  }
  if (credentialReady(item)) return { kind: "warn", text: "尚未真实连通测试" };
  return { kind: "warn", text: "等待配置或测试" };
}

function formatTestTime(value?: number) {
  if (!value) return "";
  const date = new Date(value > 1e12 ? value : value * 1000);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString("zh-CN", { hour12: false, month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
}

function CredentialTestOverlay({
  open,
  testing,
  result,
  title,
  onClose,
}: {
  open: boolean;
  testing: boolean;
  result: WorkerModelTestResult | null;
  title: string;
  onClose: () => void;
}) {
  return (
    <Modal isOpen={open && (testing || Boolean(result))} onOpenChange={(isOpen) => { if (!isOpen) onClose(); }}>
      <Modal.Backdrop>
        <Modal.Container size="lg" scroll="inside" placement="center">
          <Modal.Dialog aria-label="真实连通测试">
            <Modal.Header className="flex flex-col gap-1">
              <Modal.Heading>{title}</Modal.Heading>
              <small className="font-normal text-muted">
                {testing ? "正在向模型发起真实请求" : result?.ok ? "该凭据当前可用" : "该凭据当前不可用"}
              </small>
            </Modal.Header>
            <Modal.Body>
              <ModelTestTerminal testing={testing} result={result} title="连通测试终端" />
            </Modal.Body>
            <Modal.Footer>
              <Button variant="ghost" onPress={onClose}>关闭</Button>
            </Modal.Footer>
          </Modal.Dialog>
        </Modal.Container>
      </Modal.Backdrop>
    </Modal>
  );
}

export function ProviderManager({ taskSettings = false, onCredentialsChanged }: { taskSettings?: boolean; onCredentialsChanged?: (action: "save" | "import" | "create" | "delete") => void } = {}) {
  const [credentials, setCredentials] = useState<GlobalCredential[]>([]);
  const [instances, setInstances] = useState<RuntimeInstance[]>([]);
  const [backend, setBackend] = useState<WorkerSettings["worker_backend"]>("local");
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [lastCheckedAt, setLastCheckedAt] = useState("");
  const [selectedEngine, setSelectedEngine] = useState<Engine | null>(null);
  const [selectedCredentialId, setSelectedCredentialId] = useState("");
  const [activeTab, setActiveTab] = useState<DetailTab>("credentials");
  const [chatDefault, setChatDefault] = useState<ChatDefaultModel | null>(null);
  const [chatDefaultCredentialId, setChatDefaultCredentialId] = useState("");
  const [chatDefaultModelId, setChatDefaultModelId] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [busyItem, setBusyItem] = useState("");
  const [testOpen, setTestOpen] = useState(false);
  const [testResults, setTestResults] = useState<Record<string, WorkerModelTestResult>>({});
  const [engineDescriptors, setEngineDescriptors] = useState<EngineDescriptor[]>(FALLBACK_ENGINE_DESCRIPTORS);
  const [createDialogOpen, setCreateDialogOpen] = useState(false);
  const [newDraft, setNewDraft] = useState<CredentialDraft>(EMPTY_DRAFT);
  const [createError, setCreateError] = useState("");
  const [creating, setCreating] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<GlobalCredential | null>(null);
  const [deleteError, setDeleteError] = useState("");
  const [deleting, setDeleting] = useState(false);
  const [editSecret, setEditSecret] = useState("");
  const [editBaseRevision, setEditBaseRevision] = useState<string | undefined>();
  const [createEngine, setCreateEngine] = useState<Engine | null>(null);
  const [showSecret, setShowSecret] = useState(false);
  const [editConnection, setEditConnection] = useState<Connection>("official");
  const [editProvider, setEditProvider] = useState("");
  const [editBaseUrl, setEditBaseUrl] = useState("");
  const [editDefaultModel, setEditDefaultModel] = useState("");
  const [editModelsText, setEditModelsText] = useState("");
  const [modelRefreshFeedback, setModelRefreshFeedback] = useState<ModelRefreshFeedback>(null);
  const [editBinaryPath, setEditBinaryPath] = useState("");
  const [runtimeBaseRevision, setRuntimeBaseRevision] = useState("");
  const [editEndpoint, setEditEndpoint] = useState("");
  const [editTransport, setEditTransport] = useState<Record<string, string | number | boolean>>({});
  const [importAccountId, setImportAccountId] = useState("");
  const feedbackTimer = useRef<number | null>(null);
  const loadGeneration = useRef(0);
  const backendInitialized = useRef(false);
  const refreshInFlight = useRef(false);
  const lastAutomaticRefreshAt = useRef(Date.now());

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
      const [rows, runtimePayload, settings, descriptorPayload] = await Promise.all([
        getGlobalCredentials(backend),
        apiFetch("/api/agent-runtimes?transport=cli").then(async (response) => {
          if (!response.ok) throw new Error(`运行环境读取失败（HTTP ${response.status}）`);
          const payload = await response.json();
          if (!Array.isArray(payload.instances)) throw new Error("运行环境目录返回了无效响应");
          return payload;
        }),
        getWorkerSettings().catch(() => null),
        apiFetch("/api/settings/agent-engines").then((r) => r.ok ? r.json() : null).catch(() => null),
      ]);
      if (generation !== loadGeneration.current) return false;
      const nextCredentials = Array.isArray(rows) ? rows : [];
      setCredentials(nextCredentials);
      setInstances(Array.isArray(runtimePayload.instances) ? runtimePayload.instances : []);
      if (!backendInitialized.current && settings?.worker_backend) { setBackend(settings.worker_backend); backendInitialized.current = true; }
      if (Array.isArray(descriptorPayload?.engines)) setEngineDescriptors(descriptorPayload.engines as EngineDescriptor[]);
      if (!taskSettings) {
        const saved = readChatDefaultModel();
        const savedCredential = saved && nextCredentials.find((item) => item.id === saved.credentialId && asEngine(item.engine) !== "dsh" && credentialReady(item));
        const activeCredentials = nextCredentials.filter((item) => asEngine(item.engine) !== "dsh");
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

  const engines = useMemo<EngineSummary[]>(() => ENGINES.map((engine) => {
    const descriptor = engineDescriptors.find((item) => item.id === engine) || FALLBACK_ENGINE_DESCRIPTORS.find((item) => item.id === engine)!;
    if (descriptor.support_status !== "supported") {
      return { engine, label: descriptor.display_name || ENGINE_LABELS[engine], credentials: [], instances: [], primaryCredential: null, primaryInstance: null, enabled: false, version: "", readyCredentialCount: 0, modelCount: 0, environmentCount: 0, statusKind: "disabled", statusText: "暂不支持", supportStatus: descriptor.support_status, disabledReason: descriptor.disabled_reason || DSH_DISABLED_REASON };
    }
    const engineCredentials = credentials.filter((item) => asEngine(item.engine) === engine);
    const engineInstances = instances.filter((item) => asEngine(item.engine) === engine);
    const primaryCredential = engineCredentials.find((item) => item.source === "stored" && credentialReady(item)) || engineCredentials.find(credentialReady) || engineCredentials[0] || null;
    const primaryInstance = engineInstances.find((item) => item.configured) || engineInstances.find((item) => item.discovered) || engineInstances[0] || null;
    const enabled = primaryInstance?.enabled ?? engineCredentials.some((item) => item.usage.some((usage) => usage.enabled !== false));
    const readyCredentialCount = engineCredentials.filter(credentialReady).length;
    const modelCount = new Set(engineCredentials.flatMap(credentialModels)).size;
    let statusKind: StatusKind = "warn";
    let statusText = "尚无可用凭据";
    if (!enabled) { statusKind = "disabled"; statusText = "已停用"; }
    else if (!primaryInstance?.health) statusText = "等待设备探测";
    else if (primaryInstance?.health && !primaryInstance.health.healthy) { statusKind = "bad"; statusText = primaryInstance.health.detail || "运行环境异常"; }
    else if (readyCredentialCount > 0) { statusKind = "ok"; statusText = "已就绪"; }
    else if (primaryInstance?.auth.status === "ok") { statusKind = "ok"; statusText = "宿主登录可用"; }
    else if (primaryInstance?.auth.status === "missing") statusText = "等待登录或添加凭据";
    return { engine, label: descriptor.display_name || ENGINE_LABELS[engine], credentials: engineCredentials, instances: engineInstances, primaryCredential, primaryInstance, enabled, version: primaryInstance?.health?.runtime_version || "", readyCredentialCount, modelCount, environmentCount: engineInstances.length, statusKind, statusText, supportStatus: "supported", disabledReason: "" };
  }), [credentials, engineDescriptors, instances]);
  const currentEngine = useMemo(() => selectedEngine ? engines.find((item) => item.engine === selectedEngine) || null : null, [engines, selectedEngine]);
  const selectedCredential = useMemo(() => currentEngine?.credentials.find((item) => item.id === selectedCredentialId) || currentEngine?.primaryCredential || currentEngine?.credentials[0] || null, [currentEngine, selectedCredentialId]);
  const currentInstance = currentEngine?.primaryInstance || null;
  const currentEngineRef = useRef(currentEngine);
  const selectedCredentialRef = useRef(selectedCredential);
  const currentInstanceRef = useRef(currentInstance);
  currentEngineRef.current = currentEngine;
  selectedCredentialRef.current = selectedCredential;
  currentInstanceRef.current = currentInstance;
  const currentTransportFields = useMemo(() => currentInstance ? currentInstance.config_schema?.properties?.transport?.properties ?? FALLBACK_TRANSPORT_FIELDS[currentInstance.adapter_id] ?? {} : {}, [currentInstance]);
  const conversationCredentials = useMemo(() => credentials.filter((item) => asEngine(item.engine) !== "dsh").map(normalizeConversationCredential).filter((item): item is ConversationCredential => item !== null), [credentials]);
  const supportedEngines = useMemo(() => engines.filter((item) => item.supportStatus === "supported"), [engines]);
  const stats = useMemo(() => ({ supported: supportedEngines.length, enabled: supportedEngines.filter((item) => item.enabled).length, credentials: supportedEngines.reduce((sum, item) => sum + item.readyCredentialCount, 0), attention: supportedEngines.filter((item) => ["warn", "bad"].includes(item.statusKind)).length }), [supportedEngines]);

  useEffect(() => {
    const engine = currentEngineRef.current;
    if (!engine) return;
    const item = selectedCredentialRef.current;
    setSelectedCredentialId(item?.id || "");
    setEditBaseRevision(item?.revision);
    setEditConnection(item?.connection === "custom_endpoint" ? "custom_endpoint" : "official");
    setEditProvider(item?.provider || ""); setEditBaseUrl(item?.base_url || "");
    setEditDefaultModel(item?.default_model || item?.candidate_models?.[0] || item?.models[0] || "");
    setEditModelsText((item?.candidate_models || item?.models || []).join("\n"));
    setModelRefreshFeedback(null);
    setEditSecret(""); setShowSecret(false); setImportAccountId(`${engine.engine}-account`);
  }, [currentEngine?.engine, selectedCredential?.id]);
  useEffect(() => { const instance = currentInstanceRef.current; setRuntimeBaseRevision(instance?.updated_at || ""); setEditBinaryPath(instance?.binary_path || ""); setEditEndpoint(instance?.endpoint || ""); setEditTransport({ ...(instance?.transport || {}) }); }, [currentInstance?.key]);

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
    if (!selectedCredential) return;
    setTestResults((current) => {
      if (!current[selectedCredential.id]) return current;
      const next = { ...current }; delete next[selectedCredential.id]; return next;
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
  const openCreateCredential = () => {
    setNewDraft(EMPTY_DRAFT); setCreateError(""); setCreateEngine(selectedEngine); setCreateDialogOpen(true);
  };
  const closeCreateCredential = () => {
    setCreateDialogOpen(false); setCreateError(""); setNewDraft(EMPTY_DRAFT); setCreateEngine(null);
  };

  const refreshAll = useCallback(async (silent = false) => {
    if (refreshInFlight.current) return;
    refreshInFlight.current = true;
    if (!silent) setRefreshing(true);
    try {
      const response = await apiFetch("/api/agent-runtimes/refresh", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ transport_kind: "cli" }) });
      await assertRuntimeResponse(response, "全部 Agent 探测");
      const catalogResponse = await apiFetch("/api/settings/credential-models/refresh-stale", { method: "POST" });
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
  }, [loadData, showFeedback]);

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
    const key = item.primaryInstance?.key || `${ENGINE_ADAPTERS[item.engine]}:default`;
    setBusyItem(`probe:${item.engine}`);
    try {
      const response = await apiFetch(`/api/agent-runtimes/${encodeURIComponent(key)}/probe`, { method: "POST" });
      await assertRuntimeResponse(response, `探测 ${item.label}`);
      if (await loadData(true)) showFeedback("ok", `${item.label} 探测完成。`);
    } catch (error) { showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `探测失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const toggleEngine = async (item: EngineSummary) => {
    if (item.supportStatus !== "supported") return;
    setBusyItem(`toggle:${item.engine}`);
    try {
      const inst = item.primaryInstance;
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ adapter_id: inst?.adapter_id || ENGINE_ADAPTERS[item.engine], instance_id: inst?.instance_id || "default", expected_revision: inst?.updated_at || "", enabled: !item.enabled }) });
      await assertRuntimeResponse(response, `${item.enabled ? "停用" : "启用"} ${item.label}`);
      if (await loadData(true)) showFeedback("ok", `${item.label} 已${item.enabled ? "停用" : "启用"}。`);
    } catch (error) { showFeedback(error instanceof Error && error.name === "RuntimeOperationPending" ? "warn" : "bad", error instanceof Error && error.name === "RuntimeOperationPending" ? error.message : `操作失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const refreshCandidateModels = async () => {
    if (!currentEngine || !selectedCredential || selectedCredential.source !== "stored") return;
    if (editConnection === "custom_endpoint" && !editBaseUrl.trim()) {
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
        connection: editConnection,
        base_url: editConnection === "custom_endpoint" ? editBaseUrl.trim() : "",
        secret: editSecret.trim(),
        backend,
        runtime_instance: currentInstance?.key || "default",
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
      const secretPatch = editSecret.trim() ? editConnection === "official" && currentEngine.engine === "codex" ? { codex_auth_json: editSecret.trim() } : { secret: editSecret.trim() } : {};
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
      const result = await testGlobalCredential(credentialId, currentEngine.engine, backend, editDefaultModel.trim() || selectedCredential.default_model || credentialModels(selectedCredential)[0] || "", currentInstance?.key || "default");
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
  const saveRuntime = async () => {
    if (!currentEngine) return;
    const targetKey = currentInstance?.key || `${ENGINE_ADAPTERS[currentEngine.engine]}:default`;
    setBusyItem("save-runtime");
    try {
      const response = await apiFetch("/api/agent-runtimes", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ expected_revision: runtimeBaseRevision, adapter_id: currentInstance?.adapter_id || ENGINE_ADAPTERS[currentEngine.engine], instance_id: currentInstance?.instance_id || "default", label: currentInstance?.label || currentEngine.label, binary_path: editBinaryPath.trim(), endpoint: editEndpoint.trim(), transport: editTransport, enabled: currentEngine.enabled }) });
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
      const result = engine === "codex" ? await importHostCodexAuth(id) : engine === "claude" || engine === "kimi" || engine === "grok" ? await importHostWorkerLogin(id, engine) : { ok: false, detail: "该引擎暂不支持导入宿主登录。" };
      if (!result.ok) throw new Error(result.detail || "请检查宿主 CLI 登录状态");
      await loadData(true); onCredentialsChanged?.("import"); setSelectedCredentialId(`account:${id}`); showFeedback("ok", `宿主登录已导入为 ${id}。`);
    } catch (error) { showFeedback("bad", `导入失败：${error instanceof Error ? error.message : String(error)}`); }
    finally { setBusyItem(""); }
  };
  const createCredential = async () => {
    if (!selectedEngine || createEngine !== selectedEngine) { setCreateError("新增目标已改变，请重新打开新增凭据。"); return; }
    const id = newDraft.accountId.trim();
    if (!id) { setCreateError("请填写凭据 ID。"); return; }
    if (credentials.some((item) => item.account_id === id || item.id === `account:${id}`)) { setCreateError("该凭据 ID 已存在，请使用不同 ID。"); return; }
    if (newDraft.connection === "custom_endpoint" && !newDraft.baseUrl.trim()) { setCreateError("请填写 Base URL。"); return; }
    if (newDraft.connection === "custom_endpoint" && !newDraft.secret.trim()) { setCreateError("请填写 API Key。"); return; }
    setCreating(true); setCreateError("");
    try {
      const secretPatch = newDraft.secret.trim() ? newDraft.connection === "official" && selectedEngine === "codex" ? { codex_auth_json: newDraft.secret.trim() } : { secret: newDraft.secret.trim() } : {};
      await putCredentialAccount(id, { create_only: true, engine: newDraft.connection === "custom_endpoint" ? "api" : selectedEngine, worker_engine: selectedEngine, target_engine: newDraft.connection === "custom_endpoint" ? selectedEngine : undefined, connection: newDraft.connection, provider: newDraft.connection === "custom_endpoint" ? newDraft.provider.trim() : undefined, base_url: newDraft.connection === "custom_endpoint" ? newDraft.baseUrl.trim() : "", target_model: newDraft.defaultModel.trim(), models: splitModels(newDraft.modelsText), ...secretPatch });
      setCreateDialogOpen(false); setNewDraft(EMPTY_DRAFT); await loadData(true); onCredentialsChanged?.("create"); setSelectedCredentialId(`account:${id}`); showFeedback("ok", `${ENGINE_LABELS[selectedEngine]} 已新增凭据 ${id}。`);
    } catch (error) { setCreateError(error instanceof Error ? error.message : String(error)); }
    finally { setCreating(false); }
  };
  const deleteCredential = async () => {
    if (!deleteTarget) return;
    const id = deleteTarget.account_id || deleteTarget.id.replace(/^account:/, "");
    setDeleting(true); setDeleteError("");
    try { const deleted = await deleteCredentialAccount(id, true); if (!deleted) throw new Error("服务未确认删除成功，引用保持原状，请刷新后重试。"); setDeleteTarget(null); setSelectedCredentialId(""); await loadData(true); onCredentialsChanged?.("delete"); showFeedback("ok", `凭据 ${id} 及其配置引用已删除。`); }
    catch (error) { setDeleteError(error instanceof Error ? error.message : String(error)); }
    finally { setDeleting(false); }
  };
  const selectChatDefault = (credentialId: string, modelId: string) => {
    if (!credentialId || !modelId) return;
    const next = { credentialId, modelId };
    if (!writeChatDefaultModel(next)) { showFeedback("bad", "默认模型未保存：本地存储不可写。"); return; }
    setChatDefaultCredentialId(credentialId); setChatDefaultModelId(modelId); setChatDefault(next);
  };
  const openEngine = (engine: Engine) => { setSelectedEngine(engine); setSelectedCredentialId(""); setActiveTab("credentials"); setTestOpen(false); };
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

  return <div className={`provider-manager agent-registry${taskSettings ? " is-task-settings" : ""}`}>
    <header className="agent-registry-toolbar">
      <p>{selectedEngine ? `${ENGINE_LABELS[selectedEngine]} 的凭据、接入方式与运行环境` : "统一管理已支持的 Agent 引擎与可用凭据"}</p>
      <div>{lastCheckedAt ? <span className="agent-registry-timestamp"><Icon name="clock" size={13} />{lastCheckedAt}</span> : null}<Button size="sm" variant="outline" className="agent-registry-button" isDisabled={refreshing || loading} onPress={() => void refreshAll()}><Icon name="refresh" size={14} className={refreshing ? "spin" : ""} />{refreshing ? "探测中…" : "刷新探测"}</Button></div>
    </header>
    {feedback ? <div className={`agent-registry-feedback is-${feedback.kind}`} role="alert"><Icon name={feedback.kind === "ok" ? "checkCircle" : feedback.kind === "warn" ? "info" : "alert"} size={15} /><span>{feedback.detail}</span><Button size="sm" variant="ghost" isIconOnly aria-label="关闭消息" onPress={() => setFeedback(null)}><Icon name="x" size={14} /></Button></div> : null}

    {!selectedEngine ? <>
      {!taskSettings ? <section className="agent-registry-default" aria-labelledby="agent-default-title">
        <div><span className="agent-registry-section-icon"><Icon name="star" size={15} /></span><div><h2 id="agent-default-title">新对话默认模型</h2><p>打开新对话时优先使用的凭据和基座模型。</p></div></div>
        <div className="agent-registry-default-controls"><ConversationModelPicker credentials={conversationCredentials} selectedCredentialId={chatDefaultCredentialId} selectedModel={chatDefaultModelId} selectedEffort="" loading={loading} variant="default-model" className="provider-default-model-picker" onSelect={({ credentialId, model }) => selectChatDefault(credentialId, model)} /><span className={chatDefault ? "agent-registry-saved is-active" : "agent-registry-saved"}>{chatDefault ? <Icon name="check" size={12} /> : null}{chatDefault ? "已设定" : "未固定"}</span>{chatDefault ? <Button size="sm" variant="ghost" isIconOnly className="agent-registry-icon-button" aria-label="清除新对话默认模型" onPress={() => { if (!writeChatDefaultModel(null)) { showFeedback("bad", "默认模型未清除：本地存储不可写。"); return; } setChatDefault(null); showFeedback("ok", "新对话默认模型已清除。"); }}><Icon name="trash" size={14} /></Button> : null}</div>
      </section> : null}
      <section className="agent-registry-overview" aria-labelledby="agent-list-title">
        <div className="agent-registry-section-head"><div><h2 id="agent-list-title">{taskSettings ? "做题模式 Agent 凭据" : "全部引擎"}</h2><p>在这里新增、导入、测试和管理做题 Worker 使用的凭据。带 Base URL 的自定义端点也可供 Reason / Titler 使用。</p></div><dl><div><dt>已支持</dt><dd>{stats.supported}</dd></div><div><dt>已启用</dt><dd>{stats.enabled}</dd></div><div><dt>可用凭据</dt><dd>{stats.credentials}</dd></div><div><dt>待处理</dt><dd>{stats.attention}</dd></div></dl></div>
        <div className="agent-registry-list" aria-busy={loading}>
          <div className="agent-registry-list-head" aria-hidden="true"><span>Agent</span><span>运行环境</span><span>版本</span><span>凭据</span><span>模型 / 环境</span><span>启用</span><span /></div>
          {loading ? <div className="agent-registry-loading"><Icon name="refresh" size={18} className="spin" />正在读取引擎状态…</div> : engines.map((item) => <article className={`agent-registry-row${item.supportStatus !== "supported" ? " is-locked" : ""}`} key={item.engine} tabIndex={item.supportStatus !== "supported" ? 0 : undefined} aria-label={item.disabledReason || undefined} data-disabled-reason={item.disabledReason || undefined} data-tooltip={item.disabledReason || undefined}>
            <div className="agent-registry-identity"><span className="agent-registry-logo"><EngineLogo engine={item.engine} size={24} data-tooltip={item.label} /></span><div><h3>{item.label}</h3><StatusPill kind={item.statusKind}>{item.statusText}</StatusPill></div></div>
            <div className="agent-registry-runtime"><strong>{item.supportStatus === "supported" ? runtimeLabel(item.primaryInstance) : "暂不支持"}</strong><span data-tooltip={item.primaryInstance?.binary_path || undefined}>{item.supportStatus === "supported" ? item.primaryInstance?.binary_path || item.primaryInstance?.adapter_id || "等待探测本地运行时" : "不执行设备与登录探测"}</span></div>
            {item.supportStatus === "supported" ? <VersionCell version={item.version} check={item.primaryInstance?.health?.version_check} /> : <div className="agent-registry-version"><strong>—</strong><span>不检查版本</span></div>}
            <div className="agent-registry-credential-count"><strong>{item.readyCredentialCount}<span> / {item.credentials.length}</span></strong><small>可用 / 全部凭据</small></div>
            <div className="agent-registry-resource-counts"><span><strong>{item.modelCount}</strong> 个模型</span><span><strong>{item.environmentCount}</strong> 个环境</span></div>
            <div className="agent-registry-row-actions"><Switch size="sm" aria-label={item.disabledReason ? `${item.label} 暂不支持：${item.disabledReason}` : `${item.label} 启用状态`} isSelected={item.enabled} isDisabled={Boolean(item.disabledReason) || busyItem === `toggle:${item.engine}`} onChange={() => void toggleEngine(item)}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch><Button size="sm" variant="ghost" className="agent-registry-manage" isDisabled={item.supportStatus !== "supported"} aria-label={item.disabledReason || `管理 ${item.label}`} data-disabled-reason={item.disabledReason || undefined} onPress={() => openEngine(item.engine)}>管理<Icon name="chevronRight" size={15} /></Button></div>
          </article>)}
        </div>
      </section>
    </> : currentEngine ? <section className="agent-registry-detail" aria-labelledby="agent-detail-title">
      <Button size="sm" variant="ghost" className="agent-registry-back" onPress={() => setSelectedEngine(null)}><Icon name="arrowRight" size={14} />返回全部引擎</Button>
      <header className="agent-registry-detail-head"><div className="agent-registry-detail-identity"><span className="agent-registry-logo is-large"><EngineLogo engine={currentEngine.engine} size={30} data-tooltip={currentEngine.label} /></span><div><div><h2 id="agent-detail-title">{currentEngine.label}</h2>{currentEngine.version ? <code>{currentEngine.version}</code> : null}</div><StatusPill kind={currentEngine.statusKind}>{currentEngine.statusText}</StatusPill></div></div><div className="agent-registry-detail-actions"><Button size="sm" variant="outline" className="agent-registry-button" isDisabled={busyItem === `probe:${currentEngine.engine}`} onPress={() => void probeEngine(currentEngine)}><Icon name="refresh" size={14} className={busyItem === `probe:${currentEngine.engine}` ? "spin" : ""} />探测</Button><span>启用</span><Switch size="sm" aria-label={`${currentEngine.label} 启用状态`} isSelected={currentEngine.enabled} isDisabled={busyItem === `toggle:${currentEngine.engine}`} onChange={() => void toggleEngine(currentEngine)}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div></header>
      <dl className="agent-registry-detail-stats"><div><dt>凭据</dt><dd>{currentEngine.readyCredentialCount} / {currentEngine.credentials.length} 可用</dd></div><div><dt>模型</dt><dd>{currentEngine.modelCount} 个已登记</dd></div><div><dt>运行环境</dt><dd>{currentEngine.environmentCount} 个实例</dd></div><div><dt>当前后端</dt><dd>{backend === "container" ? "容器 Worker" : "本地 Worker"}</dd></div></dl>
      <Tabs selectedKey={activeTab} onSelectionChange={(key) => { const tab = key as DetailTab; setActiveTab(tab); if (tab !== "credentials") setTestOpen(false); }} className="contents">
      <Tabs.ListContainer className="agent-registry-tabs border-b border-line bg-transparent px-1 py-1.5">
        <Tabs.List aria-label={`${currentEngine.label} 设置`} className="flex w-max min-w-0 items-center gap-1">
          {([
            ["credentials", `凭据 (${currentEngine.credentials.length})`, "lock"],
            ["runtime", "接入与环境", "terminal"],
            ["capabilities", "模型与能力", "network"],
          ] as [DetailTab, string, IconName][]).map(([tab, label, icon]) => (
            <Tabs.Tab
              key={tab}
              id={tab}
              className="relative inline-flex h-9 w-auto shrink-0 items-center gap-1.5 whitespace-nowrap rounded-control px-3 text-[12px] font-medium text-ink-3 hover:bg-hover hover:text-ink selected:text-ink"
            >
              <Icon name={icon} size={14} />
              {label}
              <Tabs.Indicator className="absolute inset-0 -z-10 rounded-control bg-surface shadow-hairline" />
            </Tabs.Tab>
          ))}
        </Tabs.List>
      </Tabs.ListContainer>

      <Tabs.Panel id="credentials" className="agent-registry-credential-layout">
        <aside className="agent-registry-credential-list" aria-label={`${currentEngine.label} 凭据`}><div><h3>凭据</h3><Button size="sm" variant="ghost" type="button" onClick={openCreateCredential}><Icon name="plus" size={13} />新增</Button></div>{currentEngine.credentials.length ? currentEngine.credentials.map((item) => <Button type="button" variant="ghost" key={item.id} data-selected={selectedCredential?.id === item.id ? "true" : undefined} className={selectedCredential?.id === item.id ? "is-selected" : ""} onClick={() => { setSelectedCredentialId(item.id); setTestOpen(false); }}><span className={`agent-registry-credential-dot ${(testResults[item.id] || lastTestAsResult(item, currentEngine.engine))?.ok ? "is-ready" : (testResults[item.id] || item.last_test) ? "is-bad" : ""}`} /><span><strong>{credentialName(item)}</strong><small>{item.source === "system" ? "宿主登录" : item.connection === "custom_endpoint" ? "自定义端点" : "官方账号"}</small></span>{chatDefault?.credentialId === item.id ? <Icon name="star" size={13} data-tooltip="新对话默认凭据" /> : null}</Button>) : <div className="agent-registry-empty-small"><Icon name="lock" size={18} /><p>尚未添加凭据</p><span>可新增官方账号或自定义端点。</span></div>}</aside>
        <div className="agent-registry-editor">{selectedCredential && liveStatus ? <><header className="agent-registry-editor-head"><div><h3>{credentialName(selectedCredential)}</h3><p>{selectedCredential.source === "system" ? "系统检测到的宿主登录状态" : `凭据 ID：${selectedCredential.account_id || selectedCredential.id.replace(/^account:/, "")}`}</p></div><StatusPill kind={liveStatus.kind}>{liveStatus.text}</StatusPill></header>
          {selectedCredential.source === "stored" && selectedCredential.revision !== editBaseRevision ? <div role="alert" className="agent-registry-dialog-note">配置已在其它视图更新，本地编辑仍保留；请载入最新配置后重新核对。<Button type="button" variant="ghost" onClick={() => {
            setEditBaseRevision(selectedCredential.revision); setEditConnection(selectedCredential.connection === "custom_endpoint" ? "custom_endpoint" : "official");
            setEditProvider(selectedCredential.provider || ""); setEditBaseUrl(selectedCredential.base_url || "");
            setEditDefaultModel(selectedCredential.default_model || ""); setEditModelsText((selectedCredential.candidate_models || selectedCredential.models).join("\n")); setEditSecret("");
          }}>载入最新配置并清除本地编辑</Button></div> : null}
          {selectedCredential.source === "system" ? <SystemLoginPanel ready={credentialReady(selectedCredential)} importable={IMPORTABLE_ENGINES.has(currentEngine.engine)} importAccountId={importAccountId} onImportAccountId={setImportAccountId} importing={busyItem === "import-system"} onImport={() => void importSystemLogin()} models={systemModels} verifiedModels={verifiedModels} selectedModel={editDefaultModel} onSelectModel={setEditDefaultModel} catalog={selectedCredential.model_catalog} /> : <div className="agent-registry-form-grid"><label><span>连接方式</span><Select aria-label="连接方式" selectedKey={editConnection} onSelectionChange={(key) => setEditConnection(key as Connection)}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="official">官方账号</ListBoxItem><ListBoxItem id="custom_endpoint">自定义端点</ListBoxItem></ListBox></Select.Popover></Select></label>{editConnection === "custom_endpoint" ? <><p className="agent-registry-endpoint-scope-note"><Icon name="info" size={14} /><span><strong>两种使用方式</strong>Worker 使用当前 {currentEngine.label} 引擎调用；Reason / Titler 复用同一端点时，由 Muteki 后端直接发送 HTTP 请求，不会启动该 Agent。</span></p><label><span>Base URL</span><Input placeholder="https://api.example.com/v1" value={editBaseUrl} onChange={(event) => setEditBaseUrl(event.target.value)} /></label><label><span>服务名称</span><Input placeholder="团队网关 / 模型服务" value={editProvider} onChange={(event) => setEditProvider(event.target.value)} /></label></> : null}<label className="agent-registry-secret"><span>{currentEngine.engine === "codex" && editConnection === "official" ? "Auth JSON" : "Secret / Token / API Key"}</span><div><Input type={showSecret ? "text" : "password"} placeholder="留空时保留已保存的凭据" value={editSecret} onChange={(event) => setEditSecret(event.target.value)} /><Button type="button" aria-label={showSecret ? "隐藏凭据" : "显示凭据"} onClick={() => setShowSecret((value) => !value)}><Icon name={showSecret ? "eyeOff" : "eye"} size={15} /></Button></div></label><label><span>默认模型</span><Input list={`agent-credential-models-${selectedCredential.id}`} autoComplete="off" placeholder="输入或选择模型 ID" value={editDefaultModel} onChange={(event) => setEditDefaultModel(event.target.value)} /><datalist id={`agent-credential-models-${selectedCredential.id}`}>{splitModels(editModelsText).map((model) => <option key={model} value={model} />)}</datalist></label><CandidateModelsField value={editModelsText} refreshing={busyItem === "refresh-models"} onRefresh={() => void refreshCandidateModels()} catalog={selectedCredential.model_catalog} feedback={modelRefreshFeedback} /></div>}
          {displayedTestResult ? <Button type="button" variant="ghost" className={`agent-registry-last-test is-${displayedTestResult.ok ? "ok" : "bad"}`} onClick={() => setTestOpen(true)}><Icon name={displayedTestResult.ok ? "checkCircle" : "alert"} size={15} /><span className="agent-registry-last-test-copy"><strong>{displayedTestResult.ok ? "最近一次真实连通：可用" : "最近一次真实连通：不可用"}</strong><small>{[displayedTestResult.detail, displayedTestResult.backend === "container" ? "容器环境" : "本地环境", formatTestTime(displayedTestResult.tested_at)].filter(Boolean).join(" · ")}</small></span><Icon name="chevronRight" size={14} /></Button> : <p className="agent-registry-last-test is-idle">还没有真实连通记录。点下方按钮后会弹出测试终端，显示该凭据是否可用。</p>}
          <footer className="agent-registry-editor-actions"><Select aria-label="连通测试后端" selectedKey={backend} onSelectionChange={(key) => setBackend(key as WorkerSettings["worker_backend"])}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="local">本地环境测试</ListBoxItem><ListBoxItem id="container">容器环境测试</ListBoxItem></ListBox></Select.Popover></Select><Button type="button" className="agent-registry-button" isDisabled={testingCredential} onClick={() => void testCredential()}><Icon name="plug" size={14} />{testingCredential ? "测试中…" : "真实连通测试"}</Button>{selectedCredential.source === "stored" ? <><Button type="button" className="agent-registry-button is-primary" isDisabled={busyItem === "save-credential"} onClick={() => void saveCredential()}><Icon name="check" size={14} />{busyItem === "save-credential" ? "保存中…" : "保存凭据"}</Button><Button type="button" className="agent-registry-button is-danger" onClick={() => { setDeleteError(""); setDeleteTarget(selectedCredential); }}><Icon name="trash" size={14} />删除</Button></> : null}</footer>
        </> : <div className="agent-registry-empty"><Icon name="lock" size={24} /><h3>先为 {currentEngine.label} 添加凭据</h3><p>凭据可以是官方账号，也可以是带 Base URL 的自定义端点。</p><Button type="button" className="agent-registry-button is-primary" onClick={openCreateCredential}><Icon name="plus" size={14} />新增凭据</Button></div>}</div>
      </Tabs.Panel>

      <Tabs.Panel id="runtime" className="agent-registry-panel"><div className="agent-registry-panel-head"><div><h3>接入与运行环境</h3><p>设置本机二进制、服务端点和该引擎的传输参数。</p></div><code>{currentInstance?.adapter_id || ENGINE_ADAPTERS[currentEngine.engine]}</code></div>{(currentInstance?.updated_at || "") !== runtimeBaseRevision ? <div role="alert" className="agent-registry-dialog-note">运行配置已在其它视图更新，请重新核对。<Button type="button" variant="ghost" onClick={() => { setRuntimeBaseRevision(currentInstance?.updated_at || ""); setEditBinaryPath(currentInstance?.binary_path || ""); setEditEndpoint(currentInstance?.endpoint || ""); setEditTransport({ ...(currentInstance?.transport || {}) }); }}>载入最新配置并清除本地编辑</Button></div> : null}<div className="agent-registry-form-grid"><label><span>二进制路径</span><Input placeholder="留空时自动探测" value={editBinaryPath} onChange={(event) => setEditBinaryPath(event.target.value)} /></label><label><span>服务端点</span><Input placeholder="留空时使用本地进程" value={editEndpoint} onChange={(event) => setEditEndpoint(event.target.value)} /></label>{Object.entries(currentTransportFields).map(([key, field]) => field.type === "boolean" ? <Checkbox className="agent-registry-check" key={key} isSelected={Boolean(editTransport[key] ?? field.default)} onChange={(selected) => setEditTransport((value) => ({ ...value, [key]: selected }))}><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control>{field.title || key}</Checkbox.Content></Checkbox> : <label key={key}><span>{field.title || key}</span><Input type={field.type === "integer" || field.type === "number" ? "number" : "text"} value={String(editTransport[key] ?? field.default ?? "")} onChange={(event) => setEditTransport((value) => ({ ...value, [key]: field.type === "integer" || field.type === "number" ? Number(event.target.value) : event.target.value }))} /></label>)}</div><div className="agent-registry-runtime-note"><Icon name={currentInstance?.health?.healthy ? "checkCircle" : "info"} size={16} /><div><strong>{currentInstance?.health?.healthy ? "运行环境健康" : "尚未得到健康结果"}</strong><p>{currentInstance?.health?.detail || currentInstance?.auth.detail || "保存后执行探测，以读取版本、登录状态和运行能力。"} {versionCheckDetail(currentInstance?.health?.version_check)}</p></div></div><footer className="agent-registry-editor-actions"><Button type="button" className="agent-registry-button" onClick={() => void probeEngine(currentEngine)}><Icon name="refresh" size={14} />重新探测</Button><Button type="button" className="agent-registry-button is-primary" isDisabled={busyItem === "save-runtime"} onClick={() => void saveRuntime()}><Icon name="check" size={14} />{busyItem === "save-runtime" ? "保存中…" : "保存接入配置"}</Button></footer></Tabs.Panel>

      <Tabs.Panel id="capabilities" className="agent-registry-panel"><div className="agent-registry-panel-head"><div><h3>模型与能力</h3><p>汇总该引擎所有凭据的模型，以及运行时探测到的能力。</p></div></div><div className="agent-registry-capability-grid"><section><h4>已登记模型</h4><div className="agent-registry-chips">{[...new Set(currentEngine.credentials.flatMap(credentialModels))].length ? [...new Set(currentEngine.credentials.flatMap(credentialModels))].map((model) => <code key={model}>{model}</code>) : <p>尚未登记模型。</p>}</div></section><section><h4>运行能力</h4><div className="agent-registry-capability-list">{currentInstance?.health?.capabilities && Object.keys(currentInstance.health.capabilities).length ? Object.entries(currentInstance.health.capabilities).map(([key, value]) => <div key={key}><Icon name={value ? "checkCircle" : "minus"} size={14} /><span>{CAPABILITY_LABELS[key] || key}</span><small>{typeof value === "boolean" ? value ? "支持" : "未启用" : String(value)}</small></div>) : <p>探测运行环境后显示能力。</p>}</div></section></div></Tabs.Panel>
      </Tabs>
    </section> : null}

    <Modal isOpen={createDialogOpen} onOpenChange={(open) => { if (open) openCreateCredential(); else closeCreateCredential(); }}>
      <Modal.Backdrop isDismissable={!creating}><Modal.Container size="lg" scroll="inside"><Modal.Dialog>
        <Modal.Header className="flex flex-col gap-1"><Modal.Heading>为 {selectedEngine ? ENGINE_LABELS[selectedEngine] : "Agent"} 新增凭据</Modal.Heading><small className="font-normal text-muted">{newDraft.connection === "custom_endpoint" ? "该端点可供当前 Agent 的 Worker 使用，也可供 Reason / Titler 直接发起 HTTP 模型请求。" : "新增内容会归入当前引擎，不会创建新的顶层 Agent。"}</small></Modal.Header>
        <Modal.Body>
          <div className="agent-registry-dialog-form">
            {createError ? <p className="agent-registry-dialog-error" role="alert">{createError}</p> : null}
            <TextField value={newDraft.accountId} onChange={(accountId) => setNewDraft((value) => ({ ...value, accountId }))}><Label>凭据 ID</Label><Input autoComplete="off" placeholder="例如 work-account" /></TextField>
            <Select selectedKey={newDraft.connection} onSelectionChange={(connection) => setNewDraft((value) => ({ ...value, connection: connection as Connection }))}><Label>连接方式</Label><Select.Trigger><Select.Value /><Select.Indicator /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="official">官方账号</ListBoxItem><ListBoxItem id="custom_endpoint">自定义端点</ListBoxItem></ListBox></Select.Popover></Select>
            {newDraft.connection === "custom_endpoint" ? <><p className="agent-registry-dialog-note">Reason / Titler 只复用这里保存的 Base URL、API Key 和模型目录，不会调用当前 Agent。</p><TextField value={newDraft.baseUrl} onChange={(baseUrl) => setNewDraft((value) => ({ ...value, baseUrl }))}><Label>Base URL</Label><Input placeholder="https://api.example.com/v1" /></TextField><TextField value={newDraft.provider} onChange={(provider) => setNewDraft((value) => ({ ...value, provider }))}><Label>服务名称</Label><Input /></TextField></> : null}
            <TextField value={newDraft.secret} onChange={(secret) => setNewDraft((value) => ({ ...value, secret }))}><Label>{selectedEngine === "codex" && newDraft.connection === "official" ? "Auth JSON" : "Secret / Token / API Key"}</Label><TextArea className="resize-none" rows={3} /></TextField>
            <TextField value={newDraft.defaultModel} onChange={(defaultModel) => setNewDraft((value) => ({ ...value, defaultModel }))}><Label>默认模型</Label><Input placeholder="model-id" /></TextField>
            <TextField value={newDraft.modelsText} onChange={(modelsText) => setNewDraft((value) => ({ ...value, modelsText }))}><Label>候选模型</Label><TextArea className="resize-none" rows={3} placeholder="每行一个模型 ID" /></TextField>
          </div>
        </Modal.Body>
        <Modal.Footer><Button variant="ghost" onPress={closeCreateCredential} isDisabled={creating}>取消</Button><Button variant="primary" isPending={creating} isDisabled={!newDraft.accountId.trim()} onPress={() => void createCredential()}>新增凭据</Button></Modal.Footer>
      </Modal.Dialog></Modal.Container></Modal.Backdrop>
    </Modal>
    <Modal isOpen={Boolean(deleteTarget)} onOpenChange={(open) => { if (!open) { setDeleteTarget(null); setDeleteError(""); } }}>
      <Modal.Backdrop isDismissable={!deleting}><Modal.Container><Modal.Dialog>
        <Modal.Header className="flex flex-col gap-1"><Modal.Heading>删除凭据</Modal.Heading><small className="font-normal text-muted">将删除 {deleteTarget ? credentialName(deleteTarget) : "该凭据"}。该操作不会删除对应的 Agent 引擎。</small></Modal.Header>
        <Modal.Body>{deleteError ? <p className="agent-registry-dialog-error" role="alert">{deleteError}</p> : deleteTarget?.usage?.length ? <p className="agent-registry-dialog-note">将同步解除以下配置引用：{deleteTarget.usage.map((item) => item.label || item.id).join("、")}。关联对话和产物会保留，相关 Worker 会停用，系统角色会恢复默认接入。</p> : <p className="agent-registry-dialog-note">该凭据当前没有配置引用，可以直接删除。</p>}</Modal.Body>
        <Modal.Footer><Button variant="ghost" onPress={() => { setDeleteTarget(null); setDeleteError(""); }} isDisabled={deleting}>取消</Button><Button variant="danger" isPending={deleting} onPress={() => void deleteCredential()}>{deleteTarget?.usage?.length ? `解除 ${deleteTarget.usage.length} 处引用并删除` : "确认删除"}</Button></Modal.Footer>
      </Modal.Dialog></Modal.Container></Modal.Backdrop>
    </Modal>
    <CredentialTestOverlay open={testOpen} testing={testingCredential} result={testingCredential ? liveTestResult : displayedTestResult} title={`${currentEngine?.label || "Agent"} · ${selectedCredential ? credentialName(selectedCredential) : "凭据"}`} onClose={() => setTestOpen(false)} />
  </div>;
}
