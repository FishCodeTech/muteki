"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { createPortal } from "react-dom";
import { Icon, type IconName } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { NumberField } from "@/components/NumberField";
import { PlatformUpdate } from "@/components/PlatformUpdate";
import {
  type CredentialAccount,
  type ProfileHealth,
  type WorkerModelTestResult,
  type WorkerModelOptions,
  type WorkerImageStatus,
  type LlmProfile,
  type LlmTemperatureMode,
  type WorkerSettings,
  deleteCredentialAccount,
  discoverWorkerModels,
  fetchProfilesHealth,
  getWorkerModelOptions,
  getWorkerImageStatus,
  getWorkerSettings,
  getSystemLogin,
  importHostCodexAuth,
  importHostWorkerLogin,
  listCredentialAccounts,
  putCredentialAccount,
  putWorkerSettings,
  pullWorkerImage,
  testCredentialAccount,
  testLlmEndpoint,
  testWorkerProfileModel,
  testWorkerProfileModelsBatch,
} from "@/lib/useRun";
import {
  SCHEMES,
  buildPalette,
  buildPaletteFromHue,
  applySelection,
  readSavedSelection,
  readSavedTheme,
  type SchemeSelection,
  type ThemeMode,
} from "@/lib/palette-engine";

type Seat = NonNullable<WorkerSettings["seats"]>[number];
type Credential = NonNullable<WorkerSettings["credentials"]>[number];
type ReviewPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["review"]>;
type VerifierPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["verifier"]>;
type SettingsSection = "roster" | "runtime" | "scheduling" | "models" | "appearance" | "system";
type Engine = "claude" | "codex" | "cursor" | "pi" | "omp" | "kimi" | "grok" | "opencode" | "dsh";
type SaveState = "idle" | "saving" | "saved" | "error";
type AccountConnection = "official" | "custom_endpoint";
type LlmProfileName = "planner" | "titler";

const DEFAULT_LLM_PROFILES: WorkerSettings["llm_profiles"] = {
  planner: { provider: "deepseek", model: "deepseek-v4-pro", base_url: "", connection: "default", temperature_mode: "default", temperature: 1 },
  titler: { provider: "deepseek", model: "deepseek-v4-flash", base_url: "", connection: "default", temperature_mode: "default", temperature: 1 },
};
const WORKER_IMAGE_ENV = "MUTEKI_WORKER_IMAGE";

function llmTemperatureMode(profile: LlmProfile): LlmTemperatureMode {
  const mode = profile.temperature_mode;
  if (mode === "custom" || mode === "omit" || mode === "default") return mode;
  return "default";
}
type ModelDiscoveryOutcome = { ok: boolean; detail: string };
type BatchCheckState = { running: boolean; completed: number; total: number };

const ENGINES: Engine[] = ["pi", "claude", "codex", "cursor", "omp", "opencode", "dsh", "kimi", "grok"];
const PRIMARY_ENGINES: Engine[] = ["codex", "claude", "pi", "grok", "dsh"];
const MORE_ENGINES: Engine[] = ENGINES.filter((engine) => !PRIMARY_ENGINES.includes(engine));
const ORDINARY_ROLES = ["race", "bootstrap", "explore", "respond"];
const ENGINE_META: Record<Engine, { label: string; wireApi: string; protocol: string; transport: string; localOnly?: boolean; modelDiscovery?: boolean }> = {
  pi: { label: "Pi", wireApi: "", protocol: "OpenAI 兼容接口", transport: "pi" },
  claude: { label: "Claude Code", wireApi: "", protocol: "Anthropic Messages", transport: "claude_code", modelDiscovery: false },
  codex: { label: "Codex", wireApi: "responses", protocol: "OpenAI Responses", transport: "codex_cli" },
  cursor: { label: "Cursor", wireApi: "", protocol: "Cursor CLI 接口", transport: "cursor_agent" },
  omp: { label: "OMP", wireApi: "", protocol: "OpenAI 兼容接口", transport: "omp" },
  kimi: { label: "Kimi Code", wireApi: "", protocol: "Kimi Code CLI", transport: "kimi_code" },
  grok: { label: "Grok", wireApi: "", protocol: "Grok Build CLI", transport: "grok_build" },
  opencode: { label: "OpenCode", wireApi: "chat_completions", protocol: "OpenAI 兼容接口", transport: "opencode_cli" },
  dsh: { label: "DeepSeek Harness", wireApi: "chat_completions", protocol: "DeepSeek API", transport: "dsh_sdk_worker", modelDiscovery: false },
};

const EFFORT_LABELS: Record<string, string> = {
  default: "跟随模型默认",
  inherit: "继承 Worker 设置",
  none: "关闭",
  minimal: "Minimal",
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "XHigh",
  max: "Max",
};
const ENGINE_EFFORT_LEVELS: Record<Engine, string[]> = {
  claude: ["low", "medium", "high", "xhigh", "max"],
  codex: ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
  cursor: ["low", "medium", "high", "xhigh", "max"],
  pi: ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
  omp: ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
  kimi: ["low", "high", "max"],
  grok: ["low", "medium", "high", "xhigh"],
  opencode: ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
  dsh: [],
};

const DEFAULT_REVIEW: ReviewPolicy = {
  enabled: true,
  engine: "",
  timeout: 420,
  after_race: true,
  after_fruitless_workers: 3,
  after_duplicate_intents: 2,
  on_course_correct: true,
  on_reason_dry: true,
  on_candidate_spike: true,
  on_operator_hint: true,
  allow_review_fallback: false,
  every_completed_workers: 6,
  candidate_spike_threshold: 5,
  max_concurrent: 1,
  cooldown_events: 8,
  max_review_workers: 12,
  reasoning_effort: "inherit",
};

const DEFAULT_VERIFIER: VerifierPolicy = {
  enabled: true,
  engine: "",
  timeout: 240,
  max_concurrent: 0,
  allow_verifier_fallback: false,
  max_verifier_workers: 24,
  reasoning_effort: "inherit",
};

function engineOf(value: string): Engine {
  return ENGINES.includes(value as Engine) ? value as Engine : "claude";
}

function effortDefinition(
  engine: Engine,
  model: string,
  options: WorkerModelOptions["models"][string],
): { levels: string[]; defaultLevel: string; supported: boolean } {
  const selected = options.find((item) => item.id === model);
  if (selected?.reasoning) {
    const levels = selected.reasoning.levels.filter((level) => ENGINE_EFFORT_LEVELS[engine].includes(level));
    return {
      levels,
      defaultLevel: selected.reasoning.default || "",
      supported: selected.reasoning.supported && levels.length > 0,
    };
  }
  const levels = ENGINE_EFFORT_LEVELS[engine];
  return { levels, defaultLevel: "", supported: levels.length > 0 };
}

function normalizeEffortForModel(
  value: string | undefined,
  engine: Engine,
  model: string,
  options: WorkerModelOptions["models"][string],
): string {
  const effort = String(value || "default").toLowerCase();
  const definition = effortDefinition(engine, model, options);
  return effort === "default" || definition.levels.includes(effort) ? effort : "default";
}

function effortSummary(value?: string, defaultLevel = ""): string {
  const effort = String(value || "default").toLowerCase();
  if (effort === "default") return defaultLevel ? `默认 ${EFFORT_LABELS[defaultLevel] || defaultLevel}` : "默认强度";
  return EFFORT_LABELS[effort] || effort;
}

function isOrdinarySeat(seat: Seat): boolean {
  return seat.roles.some((role) => ORDINARY_ROLES.includes(role));
}

function canServeChannel(seat: Seat, role: "review" | "verifier"): boolean {
  return isOrdinarySeat(seat) || seat.roles.includes(role);
}

function accountWorkerEngine(account?: CredentialAccount | null): Engine | null {
  if (!account) return null;
  const target = account.worker_engine || account.details?.target_engine || account.engine;
  return typeof target === "string" && ENGINES.includes(target as Engine) ? target as Engine : null;
}

function accountConnection(account: CredentialAccount): AccountConnection {
  if (account.connection === "official" || account.connection === "custom_endpoint") return account.connection;
  const baseUrl = String(account.base_url || account.details?.base_url_value || "").trim();
  return baseUrl || (account.mode === "custom_endpoint" && account.engine === "api") ? "custom_endpoint" : "official";
}

function accountBaseUrl(account: CredentialAccount): string {
  return String(account.base_url || account.details?.base_url_value || "").trim();
}

function accountProvider(account: CredentialAccount): string {
  const explicit = String(account.provider || account.details?.provider || "").trim();
  if (explicit) return explicit;
  const engine = accountWorkerEngine(account);
  if (accountConnection(account) === "custom_endpoint") return "自定义 API";
  return engine === "claude" ? "Anthropic" : engine === "codex" ? "OpenAI" : engine === "cursor" ? "Cursor" : engine ? ENGINE_META[engine].label : "未指定";
}

function connectionLabel(account: CredentialAccount): string {
  return accountConnection(account) === "custom_endpoint" ? "自定义 API" : "官方账号";
}

function accountMatchesEngine(account: CredentialAccount, engine: string): boolean {
  return accountWorkerEngine(account) === engine;
}

function randomId(prefix: string, engine: Engine): string {
  const suffix = typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID().replaceAll("-", "").slice(0, 8)
    : `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  return `${prefix}_${engine}_${suffix}`;
}

function credentialKey(credential?: Credential): string {
  if (!credential) return "";
  return credential.kind === "system_inherit" ? "__system__" : credential.secret_ref;
}

function syncCredentialFromAccount(credential: Credential, accounts: CredentialAccount[]): Credential {
  if (!credential.secret_ref || credential.kind === "system_inherit") return credential;
  const account = accounts.find((item) => item.account_id === credential.secret_ref);
  const engine = account ? accountWorkerEngine(account) : null;
  if (!account || !engine) return credential;
  const connection = accountConnection(account);
  return {
    ...credential,
    label: account.account_id,
    engine,
    kind: connection === "custom_endpoint" ? "custom_endpoint" : "engine_key",
    target_engine: connection === "custom_endpoint" ? engine : undefined,
    endpoint: connection === "custom_endpoint"
      ? { base_url: accountBaseUrl(account), wire_api: ENGINE_META[engine].wireApi }
      : undefined,
  };
}

function syncCredentialsFromAccounts(credentials: Credential[], accounts: CredentialAccount[]): Credential[] {
  return credentials.map((credential) => syncCredentialFromAccount(credential, accounts));
}

function legacyIdentity(config: WorkerSettings): {
  seats: Seat[];
  credentials: Credential[];
} {
  if (config.seats?.length) {
    return {
      seats: config.seats.map((item) => ({
        ...item,
        reasoning_effort: item.reasoning_effort || "default",
        capacity: { ...item.capacity },
        roles: [...item.roles],
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
      label: account || `${ENGINE_META[engine].label} 系统登录`,
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
      roles: [...profile.roles],
      race: profile.race,
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

function healthLabel(health?: ProfileHealth): string {
  if (!health) return "待校验";
  if (health.status === "ok") return "可用";
  if (health.status === "auth_failed") return "认证失败";
  if (health.status === "disabled") return "已停用";
  return "不可用";
}

function SelfCheckStatus({
  enabled,
  testing,
  result,
  compact = false,
}: {
  enabled: boolean;
  testing: boolean;
  result?: WorkerModelTestResult;
  compact?: boolean;
}) {
  const state = !enabled ? "off" : testing ? "checking" : result?.ok ? "ok" : result ? "bad" : "idle";
  const label = !enabled ? "已停用" : testing ? "自检中" : result?.ok ? "自检通过" : result ? "自检失败" : "待自检";
  const elapsed = result?.elapsed_ms ? `${(result.elapsed_ms / 1000).toFixed(1)}s` : "";
  const detail = testing
    ? "正在发起真实模型请求"
    : result
      ? [result.detail || (result.ok ? "真实模型请求完成" : "真实模型请求失败"), elapsed].filter(Boolean).join(" · ")
      : enabled ? "尚未发起真实模型请求" : "停用的 Worker 不参与检查";
  return (
    <span className={`wroster-self-check ${state}${compact ? " compact" : ""}`} title={detail} aria-live="polite">
      <i aria-hidden="true" />
      <span><strong>{label}</strong>{compact ? null : <small>{detail}</small>}</span>
    </span>
  );
}

function buildModelTestProfile({
  id,
  label,
  engine,
  accountId,
  connection,
  baseUrl,
  model,
  reasoningEffort,
}: {
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
  const transport = ENGINE_META[engine].transport;
  return {
    id,
    name: label,
    engine,
    transport,
    auth: custom || ["cursor", "pi", "omp", "opencode", "dsh"].includes(engine)
      ? "api_key" : "subscription",
    credential_mode: custom || ["cursor", "pi", "omp", "opencode", "dsh"].includes(engine)
      ? "api_key" : "subscription",
    credential_account: accountId === "__system__" ? "" : accountId,
    api_key_ref: "",
    base_url: custom ? String(baseUrl || "") : "",
    wire_api: ENGINE_META[engine].wireApi,
    roles: [...ORDINARY_ROLES, "review"],
    race: true,
    max_running: 1,
    max_review_running: 0,
    priority: 10,
    model: model || "",
    reasoning_effort: reasoningEffort || "default",
    enabled: true,
  };
}

function ModelTestTerminal({
  testing,
  result,
  compact = false,
}: {
  testing: boolean;
  result: WorkerModelTestResult | null;
  compact?: boolean;
}) {
  const [collapsed, setCollapsed] = useState(false);
  if (!testing && !result) return null;
  const stateClass = testing ? "running" : result?.ok ? "ok" : "bad";
  const stateLabel = testing ? "运行中" : result?.ok ? "已通过" : "未通过";
  if (collapsed) {
    return (
      <button type="button" className="wmodel-terminal-collapsed" onClick={() => setCollapsed(false)} aria-label="展开模型测试终端">
        <span><i className={stateClass} />模型测试日志</span>
        <em>{stateLabel}{result?.elapsed_ms ? ` · ${(result.elapsed_ms / 1000).toFixed(1)}s` : ""}</em>
        <Icon name="chevronDown" size={13} />
      </button>
    );
  }
  const logs = result?.logs?.length
    ? result.logs
    : result
      ? [{ stream: result.ok ? "success" as const : "error" as const, message: result.detail, elapsed_ms: result.elapsed_ms || 0 }]
      : [];
  return (
    <section className={`wmodel-terminal${compact ? " compact" : ""}`} aria-live="polite">
      <header>
        <span><i className={stateClass} />模型测试终端</span>
        <div><em>{stateLabel}</em><button type="button" onClick={() => setCollapsed(true)} aria-label="收起模型测试终端" title="收起日志"><Icon name="chevronDown" size={13} /></button></div>
      </header>
      <div className="wmodel-terminal-body">
        {testing ? <div className="system"><b>00</b><pre>正在启动真实 Worker 模型对话，请等待模型返回…</pre></div> : null}
        {logs.map((log, index) => (
          <div className={log.stream} key={`${log.stream}-${index}`}>
            <b>{String(index + (testing ? 1 : 0)).padStart(2, "0")}</b>
            <pre>{log.message}</pre>
            <time>{log.elapsed_ms ? `${log.elapsed_ms}ms` : ""}</time>
          </div>
        ))}
      </div>
      {result ? <footer><strong>{result.detail}</strong><span>{result.engine} · {result.model || "默认模型"}{typeof result.exit_code === "number" ? ` · exit ${result.exit_code}` : ""}</span></footer> : null}
    </section>
  );
}

function Toggle({ value, onChange, label }: { value: boolean; onChange: (next: boolean) => void; label: string }) {
  return (
    <button
      type="button"
      className={`wset-toggle${value ? " on" : ""}`}
      role="switch"
      aria-checked={value}
      aria-label={label}
      onClick={() => onChange(!value)}
    ><i /></button>
  );
}

function ReasoningEffortSelect({
  engine,
  model,
  options,
  value,
  onChange,
  inherit = false,
}: {
  engine: Engine;
  model: string;
  options: WorkerModelOptions["models"][string];
  value: string;
  onChange: (value: string) => void;
  inherit?: boolean;
}) {
  const definition = effortDefinition(engine, model, options);
  const selected = inherit
    ? (value === "inherit" || definition.levels.includes(value) ? value : "inherit")
    : normalizeEffortForModel(value, engine, model, options);
  const defaultLabel = definition.defaultLevel
    ? `跟随模型默认（${EFFORT_LABELS[definition.defaultLevel] || definition.defaultLevel}）`
    : "跟随模型默认";
  const mechanism = engine === "claude"
    ? "保存后通过 Claude Code --effort 传入"
    : engine === "codex"
      ? "保存后通过 model_reasoning_effort 传入"
      : engine === "cursor"
        ? "保存后选择对应强度的 Cursor 模型变体"
        : engine === "pi" || engine === "omp"
          ? "保存后通过 --thinking 传入"
          : engine === "opencode"
            ? "保存后通过 --variant 传入"
            : engine === "kimi"
              ? "保存后通过 Kimi thinking effort 配置传入"
              : "保存后通过 --reasoning-effort 传入";
  return (
    <div className="wset-effort-control">
      <label>
        <span>{inherit ? "Review 模型等级" : "模型等级"}</span>
        <select value={selected} onChange={(event) => onChange(event.target.value)}>
          {inherit ? <option value="inherit">继承 Worker 设置</option> : <option value="default">{defaultLabel}</option>}
          {definition.levels.map((level) => <option key={level} value={level}>{EFFORT_LABELS[level] || level}</option>)}
        </select>
      </label>
      <small>{definition.supported ? mechanism : "当前模型不提供可配置的模型等级，将跟随模型默认值"}</small>
    </div>
  );
}

function WorkerCard({
  seat,
  order,
  credential,
  testing,
  testResult,
  selected,
  dragging,
  dropTarget,
  onSelect,
  onDragStart,
  onDragEnter,
  onDragOver,
  onDrop,
  onDragEnd,
  onToggleEnabled,
  onOpenMenu,
}: {
  seat: Seat;
  order: number;
  credential?: Credential;
  testing: boolean;
  testResult?: WorkerModelTestResult;
  selected: boolean;
  dragging: boolean;
  dropTarget: boolean;
  onSelect: () => void;
  onDragStart: (event: React.DragEvent<HTMLElement>) => void;
  onDragEnter: () => void;
  onDragOver: (event: React.DragEvent<HTMLElement>) => void;
  onDrop: (event: React.DragEvent<HTMLElement>) => void;
  onDragEnd: () => void;
  onToggleEnabled: () => void;
  onOpenMenu: (point: { x: number; y: number }) => void;
}) {
  const engine = engineOf(seat.engine);
  const account = credentialKey(credential) === "__system__"
    ? "系统登录"
    : credential?.secret_ref || credential?.label || "未配置连接";
  return (
    <article
      className={`wroster-card${selected ? " selected" : ""}${seat.enabled ? "" : " disabled"}${dragging ? " dragging" : ""}${dropTarget ? " drop-target" : ""}${testing ? " checking" : ""}`}
      draggable
      tabIndex={0}
      role="button"
      aria-haspopup="menu"
      aria-pressed={selected}
      onClick={onSelect}
      onContextMenu={(event) => {
        event.preventDefault();
        onOpenMenu({ x: event.clientX, y: event.clientY });
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onSelect(); }
        if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
          event.preventDefault();
          const rect = event.currentTarget.getBoundingClientRect();
          onOpenMenu({ x: rect.left + Math.min(56, rect.width / 2), y: rect.top + 44 });
        }
      }}
      onDragStart={onDragStart}
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDrop={onDrop}
      onDragEnd={onDragEnd}
    >
      <div className="wroster-card-head">
        <span className="wroster-order">{String(order).padStart(2, "0")}</span>
        <span className="wroster-engine"><EngineLogo engine={engine} size={18} title={ENGINE_META[engine].label} /></span>
        <span className="wroster-title"><strong>{seat.label}</strong><small>{ENGINE_META[engine].label}</small></span>
        <span
          className="wroster-card-quick-toggle"
          title={seat.enabled ? `停用 ${seat.label}` : `启用 ${seat.label}`}
          onPointerDown={(event) => event.stopPropagation()}
          onClick={(event) => event.stopPropagation()}
          onKeyDown={(event) => event.stopPropagation()}
        >
          <Toggle
            value={seat.enabled}
            onChange={onToggleEnabled}
            label={seat.enabled ? `停用 ${seat.label}` : `启用 ${seat.label}`}
          />
        </span>
        <Icon name="rows" size={13} className="wroster-grip" />
      </div>
      <dl className="wroster-meta">
        <div><dt>连接</dt><dd>{account}</dd></div>
        <div><dt>模型</dt><dd>{seat.model || "Worker 默认模型"} · {effortSummary(seat.reasoning_effort)}</dd></div>
      </dl>
      <div className="wroster-card-foot">
        <SelfCheckStatus enabled={seat.enabled} testing={testing} result={testResult} />
        <span>{seat.race ? "参与首轮" : "协调阶段"}</span>
        <strong>×{Math.max(1, seat.capacity.max_running || 1)}</strong>
      </div>
    </article>
  );
}

function RosterWorkspace({
  seats,
  credentials,
  backend,
  testingIds,
  testResults,
  batchCheck,
  selectedId,
  review,
  verifier,
  onSelect,
  onSelectReview,
  onSelectVerifier,
  onAdd,
  onReorder,
  onDuplicate,
  onToggleEnabled,
  onToggleRace,
  onSetReview,
  onSetVerifier,
  onTestAll,
  onTest,
  onDelete,
}: {
  seats: Seat[];
  credentials: Credential[];
  backend: WorkerSettings["worker_backend"];
  testingIds: Set<string>;
  testResults: Record<string, WorkerModelTestResult>;
  batchCheck: BatchCheckState;
  selectedId: string | null;
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  onSelect: (id: string) => void;
  onSelectReview: () => void;
  onSelectVerifier: () => void;
  onAdd: (engine: Engine) => void;
  onReorder: (source: string, target: string | null) => void;
  onDuplicate: (id: string) => void;
  onToggleEnabled: (id: string) => void;
  onToggleRace: (id: string) => void;
  onSetReview: (id: string) => void;
  onSetVerifier: (id: string) => void;
  onTestAll: () => void;
  onTest: (seat: Seat) => void;
  onDelete: (id: string) => void;
}) {
  const ordinary = seats.filter(isOrdinarySeat);
  const reviewSeat = seats.find((seat) => seat.id === review.engine);
  const reviewCredential = credentials.find((item) => item.id === reviewSeat?.credential_id);
  const verifierSeat = seats.find((seat) => seat.id === verifier.engine);
  const verifierCredential = credentials.find((item) => item.id === verifierSeat?.credential_id);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<{ seatId: string; x: number; y: number } | null>(null);
  const [addMenuOpen, setAddMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement | null>(null);
  const addMenuRef = useRef<HTMLDivElement | null>(null);
  const maxWorkers = ordinary.filter((seat) => seat.enabled).reduce((sum, seat) => sum + Math.max(1, seat.capacity.max_running || 1), 0);
  const enabledOrdinary = ordinary.filter((seat) => seat.enabled);
  const checkedOrdinary = enabledOrdinary.filter((seat) => testResults[seat.id]);
  const passedOrdinary = checkedOrdinary.filter((seat) => testResults[seat.id]?.ok).length;
  const checkSummary = batchCheck.running
    ? `正在检查 ${batchCheck.completed}/${batchCheck.total}`
    : checkedOrdinary.length === enabledOrdinary.length && enabledOrdinary.length > 0
      ? `自检 ${passedOrdinary}/${enabledOrdinary.length} 通过`
      : checkedOrdinary.length
        ? `已检查 ${checkedOrdinary.length}/${enabledOrdinary.length} · 通过 ${passedOrdinary}`
      : `待自检 ${enabledOrdinary.length}`;
  const menuSeat = contextMenu ? ordinary.find((seat) => seat.id === contextMenu.seatId) || null : null;

  useEffect(() => {
    if (!contextMenu) return;
    menuRef.current?.querySelector<HTMLButtonElement>("button")?.focus();
    const close = () => setContextMenu(null);
    const onKey = (event: KeyboardEvent) => { if (event.key === "Escape") close(); };
    window.addEventListener("resize", close);
    window.addEventListener("scroll", close, true);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("resize", close);
      window.removeEventListener("scroll", close, true);
      window.removeEventListener("keydown", onKey);
    };
  }, [contextMenu]);

  useEffect(() => {
    if (!addMenuOpen) return;
    addMenuRef.current?.querySelector<HTMLButtonElement>("[role='menuitem']:not(:disabled)")?.focus();
    const onPointerDown = (event: PointerEvent) => {
      if (!addMenuRef.current?.contains(event.target as Node)) setAddMenuOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setAddMenuOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [addMenuOpen]);

  const openContextMenu = (seat: Seat, point: { x: number; y: number }) => {
    const width = 232;
    const height = 318;
    const x = Math.max(8, Math.min(point.x, window.innerWidth - width - 8));
    const y = Math.max(8, Math.min(point.y, window.innerHeight - height - 8));
    onSelect(seat.id);
    setContextMenu({ seatId: seat.id, x, y });
  };

  const runMenuAction = (action: () => void) => {
    setContextMenu(null);
    action();
  };

  return (
    <section className="wroster-workspace">
      <header className="wroster-head">
        <div className="wsettings-section-head wroster-head-main">
          <div className="wsettings-section-copy"><h2>普通 Worker 池</h2><p>{enabledOrdinary.length} 个启用 · 共 {ordinary.length} 个配置 · 普通并发上限 {maxWorkers} · {checkSummary}</p></div>
          <button type="button" className={`wroster-check-all${batchCheck.running ? " running" : ""}`} disabled={!enabledOrdinary.length || batchCheck.running || testingIds.size > 0} onClick={onTestAll} title="向所有启用的出战 Worker 发起真实模型请求"><Icon name="refresh" size={13} />{batchCheck.running ? `正在检查 ${batchCheck.completed}/${batchCheck.total}` : "一键检查"}</button>
        </div>
        <div className="wroster-addbar">
          <span>添加 Worker</span>
          <div className="wroster-primary-engines">{PRIMARY_ENGINES.map((engine) => {
            const meta = ENGINE_META[engine];
            const unavailable = Boolean(meta.localOnly && backend !== "local");
            return <button key={engine} type="button" disabled={unavailable} title={unavailable ? `${meta.label} 当前仅支持本地运行` : `添加 ${meta.label}`} onClick={() => onAdd(engine)}><EngineLogo engine={engine} size={15} title={meta.label} /><span>{engine === "dsh" ? "DSH" : meta.label}</span></button>;
          })}</div>
          <div className="wroster-add" ref={addMenuRef}>
            <button type="button" className="wroster-add-trigger" aria-haspopup="menu" aria-expanded={addMenuOpen} onClick={() => setAddMenuOpen((open) => !open)}><span>更多</span><Icon name="chevronDown" size={12} /></button>
            {addMenuOpen ? <div className="wroster-add-menu" role="menu" aria-label="更多 Worker 引擎">
              <header><strong>更多 Worker 引擎</strong><small>选择后继续配置连接、模型与并发</small></header>
              <div className="wroster-add-options">{MORE_ENGINES.map((engine) => {
                const meta = ENGINE_META[engine];
                const unavailable = Boolean(meta.localOnly && backend !== "local");
                return <button key={engine} type="button" role="menuitem" disabled={unavailable} title={unavailable ? `${meta.label} 当前仅支持本地运行` : `添加 ${meta.label}`} onClick={() => { setAddMenuOpen(false); onAdd(engine); }}><span className="wroster-add-logo"><EngineLogo engine={engine} size={17} title={meta.label} /></span><span><strong>{meta.label}</strong><small>{unavailable ? "当前仅支持本地运行" : meta.localOnly ? "仅支持本地运行" : meta.protocol}</small></span><Icon name="chevronRight" size={12} /></button>;
              })}</div>
            </div> : null}
          </div>
        </div>
      </header>

      <div
        className="wroster-list"
        onDragOver={(event) => {
          // Allow dropping between/after cards, not just on top of one.
          if (!draggingId) return;
          event.preventDefault();
          event.dataTransfer.dropEffect = "move";
        }}
        onDrop={(event) => {
          // Drops on a card are handled by the card itself (it stops
          // propagation); anything landing in a grid gap or the trailing
          // space moves the dragged card to the end of the roster.
          event.preventDefault();
          if (draggingId) onReorder(draggingId, null);
          setDraggingId(null);
          setDragOverId(null);
        }}
      >
        {ordinary.length ? ordinary.map((seat, index) => (
          <WorkerCard
            key={seat.id}
            seat={seat}
            order={index + 1}
            credential={credentials.find((item) => item.id === seat.credential_id)}
            testing={testingIds.has(seat.id)}
            testResult={testResults[seat.id]}
            selected={selectedId === seat.id}
            dragging={draggingId === seat.id}
            dropTarget={dragOverId === seat.id && draggingId !== seat.id}
            onSelect={() => onSelect(seat.id)}
            onDragStart={(event) => {
              // Firefox/Safari refuse to initiate a drag unless data is set
              // during dragstart (Chrome is lenient). Always set it so the
              // drag starts in every browser.
              event.dataTransfer.setData("text/plain", seat.id);
              event.dataTransfer.effectAllowed = "move";
              setDraggingId(seat.id);
            }}
            onDragEnter={() => { if (draggingId && draggingId !== seat.id) setDragOverId(seat.id); }}
            onDragOver={(event) => {
              if (!draggingId) return;
              event.preventDefault();
              event.dataTransfer.dropEffect = "move";
            }}
            onDrop={(event) => {
              event.preventDefault();
              event.stopPropagation();
              if (draggingId && draggingId !== seat.id) onReorder(draggingId, seat.id);
              setDraggingId(null);
              setDragOverId(null);
            }}
            onDragEnd={() => { setDraggingId(null); setDragOverId(null); }}
            onToggleEnabled={() => onToggleEnabled(seat.id)}
            onOpenMenu={(point) => openContextMenu(seat, point)}
          />
        )) : (
          <div className="wroster-empty"><Icon name="grid" size={22} /><strong>还没有普通 Worker</strong><span>从上方选择一个 Worker 程序加入阵容。</span></div>
        )}
      </div>

      <section className={`wreview-slot${review.enabled ? "" : " disabled"}`}>
        <div className="wreview-slot-copy">
          <span className="wreview-symbol"><Icon name="eye" size={18} /></span>
          <div><h3>Review Worker</h3><p>独立审查通道，不占普通 Worker 并发。</p></div>
        </div>
        <button type="button" className="wreview-selection" onClick={onSelectReview}>
          {reviewSeat ? (
            <>
              <span className="wreview-engine"><EngineLogo engine={reviewSeat.engine} size={18} title={ENGINE_META[engineOf(reviewSeat.engine)].label} /></span>
              <span><strong>{reviewSeat.label}</strong><small>{reviewCredential?.secret_ref || "系统登录"} · {reviewSeat.model || "默认模型"} · {effortSummary(reviewSeat.reasoning_effort)}</small></span>
              <em>{isOrdinarySeat(reviewSeat) ? "复用 Worker" : "独立配置"}</em>
            </>
          ) : (
            <><Icon name="alert" size={16} /><span><strong>尚未指定</strong><small>选择一个可用 Worker 作为 Review</small></span></>
          )}
          <Icon name="chevronRight" size={14} />
        </button>
        <div className="wreview-slot-state">{reviewSeat ? <SelfCheckStatus enabled={Boolean(review.enabled && reviewSeat.enabled)} testing={testingIds.has(reviewSeat.id)} result={testResults[reviewSeat.id]} compact /> : <span><i />待配置</span>}<strong>并发 1</strong></div>
      </section>

      <section className={`wreview-slot wverifier-slot${verifier.enabled ? "" : " disabled"}`}>
        <div className="wreview-slot-copy">
          <span className="wreview-symbol"><Icon name="check" size={18} /></span>
          <div><h3>Verifier Worker</h3><p>独立复现验证通道，不占普通 Worker 并发。</p></div>
        </div>
        <button type="button" className="wreview-selection" onClick={onSelectVerifier}>
          {verifierSeat ? (
            <>
              <span className="wreview-engine"><EngineLogo engine={verifierSeat.engine} size={18} title={ENGINE_META[engineOf(verifierSeat.engine)].label} /></span>
              <span><strong>{verifierSeat.label}</strong><small>{verifierCredential?.secret_ref || "系统登录"} · {verifierSeat.model || "默认模型"} · {effortSummary(verifierSeat.reasoning_effort)}</small></span>
              <em>{isOrdinarySeat(verifierSeat) ? "复用 Worker" : "独立配置"}</em>
            </>
          ) : (
            <><Icon name="alert" size={16} /><span><strong>尚未指定</strong><small>选择一个可用 Worker 作为 Verifier</small></span></>
          )}
          <Icon name="chevronRight" size={14} />
        </button>
        <div className="wreview-slot-state">{verifierSeat ? <SelfCheckStatus enabled={Boolean(verifier.enabled && verifierSeat.enabled)} testing={testingIds.has(verifierSeat.id)} result={testResults[verifierSeat.id]} compact /> : <span><i />待配置</span>}<strong>并发 {verifier.max_concurrent && verifier.max_concurrent > 0 ? verifier.max_concurrent : "按报告"}</strong></div>
      </section>

      <footer className="wroster-note"><Icon name="rows" size={13} />使用卡片右上角开关快速启停；选中 Worker 后可在右侧配置连接、模型与并发，拖动卡片可调整优先级。普通并发与 Verifier 并发相互独立。</footer>

      {contextMenu && menuSeat ? createPortal(
        <div className="wroster-context-layer" onMouseDown={(event) => { if (event.target === event.currentTarget) setContextMenu(null); }} onContextMenu={(event) => { event.preventDefault(); if (event.target === event.currentTarget) setContextMenu(null); }}>
          <div
            ref={menuRef}
            className="wroster-context-menu"
            role="menu"
            aria-label={`${menuSeat.label} 快捷操作`}
            style={{ left: contextMenu.x, top: contextMenu.y }}
            onKeyDown={(event) => {
              if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
              event.preventDefault();
              const items = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
              const current = items.indexOf(document.activeElement as HTMLButtonElement);
              const next = event.key === "ArrowDown"
                ? (current + 1) % items.length
                : (current - 1 + items.length) % items.length;
              items[next]?.focus();
            }}
          >
            <header>
              <span className="wroster-context-logo"><EngineLogo engine={menuSeat.engine} size={16} title={ENGINE_META[engineOf(menuSeat.engine)].label} /></span>
              <span><strong>{menuSeat.label}</strong><small>{ENGINE_META[engineOf(menuSeat.engine)].label} · {menuSeat.enabled ? "已启用" : "已停用"}</small></span>
            </header>
            <div className="wroster-context-group">
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onSelect(menuSeat.id))}><Icon name="pencil" size={14} /><span>编辑配置</span><kbd>↵</kbd></button>
              <button type="button" role="menuitem" onClick={() => runMenuAction(() => onDuplicate(menuSeat.id))}><Icon name="copy" size={14} /><span>复制 Worker</span></button>
              <button type="button" role="menuitem" disabled={testingIds.has(menuSeat.id)} onClick={() => runMenuAction(() => onTest(menuSeat))}><Icon name="plug" size={14} /><span>{testingIds.has(menuSeat.id) ? "正在自检" : "单独自检"}</span></button>
            </div>
            <div className="wroster-context-separator" />
            <div className="wroster-context-group">
              <button type="button" role="menuitemcheckbox" aria-checked={menuSeat.enabled} onClick={() => runMenuAction(() => onToggleEnabled(menuSeat.id))}><Icon name={menuSeat.enabled ? "pause" : "play"} size={14} /><span>{menuSeat.enabled ? "停用 Worker" : "启用 Worker"}</span></button>
              <button type="button" role="menuitemcheckbox" aria-checked={menuSeat.race} onClick={() => runMenuAction(() => onToggleRace(menuSeat.id))}><Icon name="radio" size={14} /><span>{menuSeat.race ? "退出首轮" : "参与首轮"}</span></button>
              <button type="button" role="menuitemradio" aria-checked={review.engine === menuSeat.id} onClick={() => runMenuAction(() => onSetReview(menuSeat.id))}><Icon name="eye" size={14} /><span>设为 Review Worker</span>{review.engine === menuSeat.id ? <Icon name="check" size={13} /> : null}</button>
              <button type="button" role="menuitemradio" aria-checked={verifier.engine === menuSeat.id} onClick={() => runMenuAction(() => onSetVerifier(menuSeat.id))}><Icon name="check" size={14} /><span>设为 Verifier Worker</span>{verifier.engine === menuSeat.id ? <Icon name="check" size={13} /> : null}</button>
            </div>
            <div className="wroster-context-separator" />
            <div className="wroster-context-group">
              <button type="button" role="menuitem" className="danger" onClick={() => runMenuAction(() => onDelete(menuSeat.id))}><Icon name="x" size={14} /><span>移除配置</span></button>
            </div>
          </div>
        </div>,
        document.body,
      ) : null}
    </section>
  );
}

function ModelPicker({
  value,
  options,
  onChange,
}: {
  value: string;
  options: WorkerModelOptions["models"][string];
  onChange: (value: string) => void;
}) {
  const rootRef = useRef<HTMLDivElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const [position, setPosition] = useState<{ left: number; width: number; top?: number; bottom?: number; placement: "top" | "bottom"; listHeight: number } | null>(null);
  const selected = options.find((item) => item.id === value);
  const normalizedQuery = query.trim().toLowerCase();
  const visibleOptions = normalizedQuery
    ? options.filter((item) => `${item.id} ${item.label}`.toLowerCase().includes(normalizedQuery))
    : options;
  const showDefault = !normalizedQuery || "worker 程序默认模型".includes(normalizedQuery);
  const hasExactOption = options.some((item) => item.id.toLowerCase() === normalizedQuery);
  const entries: { id: string; label: string; detail: string; custom?: boolean }[] = [
    ...(showDefault ? [{ id: "", label: "Worker 程序默认模型", detail: "由当前 Worker 程序决定" }] : []),
    ...visibleOptions.map((item) => ({ id: item.id, label: item.id, detail: item.label })),
    ...(normalizedQuery && !hasExactOption ? [{ id: query.trim(), label: query.trim(), detail: "使用自定义模型名称", custom: true }] : []),
  ];

  const updatePosition = useCallback(() => {
    const rect = rootRef.current?.getBoundingClientRect();
    if (!rect) return;
    const spaceBelow = window.innerHeight - rect.bottom - 10;
    const spaceAbove = rect.top - 10;
    const placement = spaceBelow >= 270 || spaceBelow >= spaceAbove ? "bottom" : "top";
    const available = placement === "bottom" ? spaceBelow : spaceAbove;
    const listHeight = Math.max(112, Math.min(238, available - 118));
    setPosition({
      left: Math.max(8, Math.min(rect.left, window.innerWidth - rect.width - 8)),
      width: rect.width,
      ...(placement === "bottom" ? { top: rect.bottom + 6 } : { bottom: window.innerHeight - rect.top + 6 }),
      placement,
      listHeight,
    });
  }, []);

  useEffect(() => {
    if (!open) return;
    const closeOnOutsidePress = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!rootRef.current?.contains(target) && !popoverRef.current?.contains(target)) setOpen(false);
    };
    const reposition = () => updatePosition();
    updatePosition();
    document.addEventListener("pointerdown", closeOnOutsidePress);
    document.addEventListener("scroll", reposition, true);
    window.addEventListener("resize", reposition);
    return () => {
      document.removeEventListener("pointerdown", closeOnOutsidePress);
      document.removeEventListener("scroll", reposition, true);
      window.removeEventListener("resize", reposition);
    };
  }, [open, updatePosition]);

  const openPicker = () => {
    setQuery("");
    setActiveIndex(Math.max(0, entries.findIndex((item) => item.id === value)));
    updatePosition();
    setOpen(true);
    window.requestAnimationFrame(() => searchRef.current?.focus());
  };
  const commit = (next: string) => {
    onChange(next);
    setOpen(false);
    setQuery("");
  };
  const handleSearchKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setActiveIndex((current) => Math.min(Math.max(0, entries.length - 1), current + 1));
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setActiveIndex((current) => Math.max(0, current - 1));
    } else if (event.key === "Enter" && entries[activeIndex]) {
      event.preventDefault();
      commit(entries[activeIndex].id);
    } else if (event.key === "Escape") {
      event.preventDefault();
      setOpen(false);
    }
  };

  return (
    <div className={`wmodel-picker${open ? " open" : ""}`} ref={rootRef}>
      <button
        type="button"
        className="wmodel-picker-trigger"
        role="combobox"
        aria-label="模型"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => open ? setOpen(false) : openPicker()}
        onKeyDown={(event) => {
          if (!open && (event.key === "ArrowDown" || event.key === "Enter" || event.key === " ")) {
            event.preventDefault();
            openPicker();
          }
        }}
      >
        <span>
          <strong>{value || "Worker 程序默认模型"}</strong>
          <small>{value ? selected?.label || "自定义模型" : "自动跟随 Worker 程序"}</small>
        </span>
        <Icon name="chevronDown" size={14} />
      </button>

      {open && position && typeof document !== "undefined" ? createPortal(<div
        className={`wmodel-picker-popover placement-${position.placement}`}
        ref={popoverRef}
        style={{ left: position.left, width: position.width, top: position.top, bottom: position.bottom }}
      >
        <div className="wmodel-picker-search">
          <Icon name="search" size={13} />
          <input
            ref={searchRef}
            value={query}
            aria-label="搜索模型"
            placeholder="搜索或输入模型名称"
            onChange={(event) => { setQuery(event.target.value); setActiveIndex(0); }}
            onKeyDown={handleSearchKeyDown}
          />
          {query ? <button type="button" aria-label="清空搜索" onClick={() => { setQuery(""); setActiveIndex(0); searchRef.current?.focus(); }}><Icon name="x" size={12} /></button> : null}
        </div>
        <div className="wmodel-picker-meta"><span>{normalizedQuery ? "搜索结果" : "可用模型"}</span><em>{entries.length}</em></div>
        <div className="wmodel-picker-list" role="listbox" aria-label="模型选项" style={{ maxHeight: position.listHeight }}>
          {entries.length ? entries.map((item, index) => {
            const current = item.id === value;
            return <button
              type="button"
              role="option"
              aria-selected={current}
              className={`${current ? "selected" : ""}${activeIndex === index ? " active" : ""}${item.custom ? " custom" : ""}`}
              key={`${item.id || "__default__"}-${index}`}
              onMouseEnter={() => setActiveIndex(index)}
              onClick={() => commit(item.id)}
            >
              <span className="wmodel-picker-mark">{current ? <Icon name="check" size={13} /> : item.custom ? <Icon name="pencil" size={12} /> : <i />}</span>
              <span><strong>{item.label}</strong><small>{item.detail}</small></span>
              {item.custom ? <em>自定义</em> : null}
            </button>;
          }) : <div className="wmodel-picker-empty"><Icon name="search" size={14} /><span>没有匹配的模型</span></div>}
        </div>
        <footer><span><kbd>↑↓</kbd> 选择</span><span><kbd>↵</kbd> 确认</span><span><kbd>esc</kbd> 关闭</span></footer>
      </div>, document.body) : null}
    </div>
  );
}

function CredentialBindingEditor({
  engine,
  profileId,
  accountKey,
  model,
  modelOptions,
  accounts,
  backend,
  onBind,
  onModelChange,
  onAccountsChanged,
  onDiscoverModels,
  discoveringModels,
}: {
  engine: Engine;
  accountKey: string;
  model: string;
  modelOptions: WorkerModelOptions["models"][string];
  accounts: CredentialAccount[];
  backend: WorkerSettings["worker_backend"];
  onBind: (key: string, account?: CredentialAccount) => void;
  onModelChange: (model: string) => void;
  onAccountsChanged: () => Promise<void>;
  profileId: string;
  onDiscoverModels: (profileId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
}) {
  const selected = accounts.find((item) => item.account_id === accountKey) || null;
  const matchingAccounts = accounts.filter((item) => accountMatchesEngine(item, engine));
  const [editing, setEditing] = useState(false);
  const [draftId, setDraftId] = useState("");
  const [connection, setConnection] = useState<AccountConnection>("official");
  const [provider, setProvider] = useState("");
  const [baseUrl, setBaseUrl] = useState("");
  const [secret, setSecret] = useState("");
  const [draftModel, setDraftModel] = useState(model);
  const [state, setState] = useState<SaveState>("idle");
  const [validationError, setValidationError] = useState("");
  const [testing, setTesting] = useState(false);
  const [importingHost, setImportingHost] = useState(false);
  const [testResult, setTestResult] = useState<WorkerModelTestResult | null>(null);
  const [discoveryResult, setDiscoveryResult] = useState<ModelDiscoveryOutcome | null>(null);
  const editingExisting = Boolean(selected && draftId === selected.account_id);
  const editingSystem = accountKey === "__system__" && draftId === "__system__";

  useEffect(() => {
    if (!editing) setDraftModel(model);
  }, [editing, model]);

  const beginNew = () => {
    setDraftId("");
    setConnection("official");
    setProvider("");
    setBaseUrl("");
    setSecret("");
    setDraftModel(model);
    setState("idle");
    setValidationError("");
    setTestResult(null);
    setDiscoveryResult(null);
    setEditing(true);
  };
  const beginEdit = () => {
    if (!selected && accountKey === "__system__") {
      setDraftId("__system__");
      setConnection("official");
      setProvider("");
      setBaseUrl("");
      setSecret("");
      setDraftModel(model);
      setState("idle");
      setValidationError("");
      setTestResult(null);
      setDiscoveryResult(null);
      setEditing(true);
      return;
    }
    if (!selected) { beginNew(); return; }
    setDraftId(selected.account_id);
    setConnection(accountConnection(selected));
    setProvider(accountProvider(selected));
    setBaseUrl(accountBaseUrl(selected));
    setSecret("");
    setDraftModel(model);
    setState("idle");
    setValidationError("");
    setTestResult(null);
    setDiscoveryResult(null);
    setEditing(true);
  };
  const persist = async (closeAfterSave: boolean): Promise<{ accountId: string; connection: AccountConnection | "system"; baseUrl: string } | null> => {
    const nextModel = draftModel.trim();
    if (editingSystem) {
      onModelChange(nextModel);
      setEditing(!closeAfterSave);
      setState("saved");
      return { accountId: "__system__", connection: "system", baseUrl: "" };
    }
    const id = draftId.trim();
    if (!id) {
      setValidationError("请填写连接 ID。");
      setState("error");
      return null;
    }
    if (!editingExisting && !secret.trim()) {
      setValidationError(`请填写 ${credentialLabel}。`);
      setState("error");
      return null;
    }
    if (connection === "custom_endpoint" && !baseUrl.trim()) {
      setValidationError("请填写自定义 API 的 Base URL。");
      setState("error");
      return null;
    }
    if (connection === "custom_endpoint" && !nextModel) {
      setValidationError("请填写这个端点实际提供的模型 ID。显示名称和 Base URL 无法代替模型 ID。");
      setState("error");
      return null;
    }
    setValidationError("");
    setState("saving");
    const result = await putCredentialAccount(id, {
      engine: connection === "custom_endpoint" ? "api" : engine,
      worker_engine: engine,
      target_engine: connection === "custom_endpoint" ? engine : undefined,
      connection,
      provider: connection === "custom_endpoint" ? provider.trim() : undefined,
      base_url: connection === "custom_endpoint" ? baseUrl.trim() : "",
      ...(connection === "official" && engine === "codex" ? { codex_auth_json: secret } : { secret }),
    });
    if (!result) { setState("error"); return null; }
    onBind(result.account_id, result);
    // 绑定连接也会更新 Worker；最后写入模型，避免旧状态恢复为默认模型。
    onModelChange(nextModel);
    await onAccountsChanged();
    setEditing(!closeAfterSave);
    setState("saved");
    return {
      accountId: result.account_id,
      connection: accountConnection(result),
      baseUrl: accountBaseUrl(result),
    };
  };
  const save = async () => { await persist(true); };
  const testConnection = async () => {
    setTestResult(null);
    const saved = await persist(false);
    if (!saved) return;
    setTesting(true);
    const profile = buildModelTestProfile({
      id: `connection-test-${engine}`,
      label: `${draftId.trim()} 连接测试`,
      engine,
      accountId: saved.accountId,
      connection: saved.connection,
      baseUrl: saved.baseUrl,
      model: draftModel.trim(),
    });
    const result = await testWorkerProfileModel(profile, draftModel.trim(), backend);
    setTestResult(result);
    setTesting(false);
  };
  const refreshModels = async () => {
    setDiscoveryResult(null);
    setDiscoveryResult(await onDiscoverModels(profileId, engine));
  };
  const importHostLogin = async () => {
    const id = draftId.trim();
    if (!id) {
      setValidationError("请先填写连接 ID。");
      setState("error");
      return;
    }
    if (engine !== "claude" && engine !== "codex" && engine !== "kimi" && engine !== "grok") return;
    setImportingHost(true);
    setValidationError("");
    const result = engine === "codex"
      ? await importHostCodexAuth(id)
      : await importHostWorkerLogin(id, engine);
    setImportingHost(false);
    if (!result.ok || !result.account) {
      setValidationError(result.detail || "宿主登录导入失败。");
      setState("error");
      return;
    }
    onBind(result.account.account_id, result.account);
    onModelChange(result.account.suggested_model || draftModel.trim());
    await onAccountsChanged();
    setState("saved");
    setEditing(false);
  };

  const canDiscoverModels = ENGINE_META[engine].modelDiscovery !== false;
  const officialProvider = engine === "claude" ? "Anthropic" : engine === "codex" ? "OpenAI" : engine === "cursor" ? "Cursor" : ENGINE_META[engine].label;
  const credentialLabel = connection === "custom_endpoint" ? "API Key" : engine === "claude" ? "Claude OAuth Token" : engine === "codex" ? "Codex auth.json" : engine === "cursor" ? "Cursor API Key" : "API Key";

  return (
    <div className="wbinding-editor">
      <label><span>模型服务连接</span><select value={accountKey} onChange={(event) => {
        if (event.target.value === "__new__") beginNew();
        else { setEditing(false); onBind(event.target.value); }
      }}>
        <option value="">选择连接</option>
        {backend === "local" ? <option value="__system__">系统登录 · {ENGINE_META[engine].label}</option> : null}
        {matchingAccounts.map((item) => <option key={item.account_id} value={item.account_id}>{item.account_id} · {accountProvider(item)}</option>)}
        {!ENGINE_META[engine].localOnly ? <option value="__new__">＋ 新建模型服务连接</option> : null}
      </select></label>

      {ENGINE_META[engine].localOnly ? <p className="wbinding-local-note"><Icon name="terminal" size={12} />当前使用宿主 {ENGINE_META[engine].label} 登录与配置，仅支持本地运行。</p> : null}

      {selected && !editing ? <div className="wbinding-summary">
        <div><span>模型服务</span><strong>{accountProvider(selected)}</strong><small>{connectionLabel(selected)}</small></div>
        <div><span>Base URL</span><strong>{accountBaseUrl(selected) || "默认服务地址"}</strong><small>{ENGINE_META[engine].protocol}</small></div>
        <div><span>调用模型</span><strong>{model || "程序默认模型"}</strong><small>随当前 Worker 保存</small></div>
        <button type="button" onClick={beginEdit}>编辑连接与模型</button>
      </div> : accountKey === "__system__" && !editing ? <div className="wbinding-summary system"><div><span>认证来源</span><strong>宿主系统登录</strong><small>使用当前电脑已登录的账号</small></div><div><span>Worker 程序</span><strong>{ENGINE_META[engine].label}</strong><small>仅适用于本地运行</small></div><div><span>调用模型</span><strong>{model || "程序默认模型"}</strong><small>随当前 Worker 保存</small></div><button type="button" onClick={beginEdit}>配置模型</button></div> : null}

      {editing ? <div className="wbinding-form">
        <header><div><strong>{editingSystem ? "配置系统登录模型" : editingExisting ? "编辑连接与模型" : "新建连接与模型"}</strong><span>测试与后续运行将使用同一组配置</span></div><button type="button" onClick={() => setEditing(false)} aria-label="关闭编辑"><Icon name="x" size={13} /></button></header>
        {!editingSystem ? <>
          <div className="wbinding-form-group-title"><span>连接与凭据</span><small>保存后可供同类 Worker 复用</small></div>
          <label><span>连接 ID</span><input value={draftId} disabled={editingExisting} placeholder="例如 gateway-claude" onChange={(event) => setDraftId(event.target.value)} /></label>
          <label><span>连接方式</span><select value={connection} onChange={(event) => setConnection(event.target.value as AccountConnection)}><option value="official">官方账号</option><option value="custom_endpoint">自定义 API</option></select></label>
          {connection === "custom_endpoint" ? <><label><span>显示名称</span><input value={provider} placeholder="可选，例如公司代理" onChange={(event) => setProvider(event.target.value)} /></label><label><span>Base URL</span><input value={baseUrl} placeholder="https://api.example.com/v1" onChange={(event) => setBaseUrl(event.target.value)} /></label></> : null}
        </> : null}
        <div className="wbinding-form-group-title model">
          <span>模型调用</span>
          <div className="wbinding-model-tools">
            <small>{connection === "custom_endpoint" ? "填写服务实际支持的模型 ID" : canDiscoverModels ? "该值只应用到当前 Worker" : "CLI 不提供模型列表命令"}</small>
            {connection !== "custom_endpoint" ? <button type="button" className={discoveringModels ? "loading" : !canDiscoverModels ? "unsupported" : ""} disabled={discoveringModels || !canDiscoverModels} onClick={refreshModels} title={canDiscoverModels ? `从 ${ENGINE_META[engine].label} CLI 读取可用模型` : `${ENGINE_META[engine].label} CLI 不提供模型列表命令`}><Icon name="refresh" size={11} />{discoveringModels ? "刷新中…" : canDiscoverModels ? "刷新模型" : "不支持刷新"}</button> : null}
          </div>
        </div>
        <div className="wbinding-model-field"><span>模型</span>{connection === "custom_endpoint" ? <input className="wbinding-model-input" value={draftModel} placeholder="例如 k3[1m]、glm-4.7 或 deepseek-chat" onChange={(event) => { setDraftModel(event.target.value); setValidationError(""); setState("idle"); }} /> : <ModelPicker value={draftModel} options={modelOptions} onChange={(nextModel) => { setDraftModel(nextModel); setValidationError(""); setState("idle"); }} />}</div>
        {discoveryResult ? <p className={`wbinding-discovery-result ${discoveryResult.ok ? "ok" : "failed"}`}><Icon name={discoveryResult.ok ? "check" : "alert"} size={11} />{discoveryResult.detail}</p> : null}
        {!editingSystem ? <label className="secret"><span>{credentialLabel}</span><textarea rows={4} value={secret} placeholder={editingExisting ? "留空以保留当前凭据" : `输入 ${credentialLabel}`} onChange={(event) => setSecret(event.target.value)} /></label> : null}
        {!editingSystem && connection === "official" && (engine === "claude" || engine === "codex" || engine === "kimi" || engine === "grok") ? <div className="wbinding-host-import"><span>已经在本机登录？</span><p>{engine === "claude" ? "导入宿主 Claude 自定义网关配置；官方订阅账号请运行 claude setup-token 后粘贴长效令牌。" : "复制当前宿主机的最小登录配置，容器不会挂载完整用户目录。"}</p><button type="button" disabled={importingHost || testing || state === "saving"} onClick={importHostLogin}><Icon name="terminal" size={12} />{importingHost ? "导入中…" : `导入宿主 ${ENGINE_META[engine].label} 登录`}</button></div> : null}
        <div className="waccount-protocol"><span>接口协议</span><strong>{ENGINE_META[engine].protocol}</strong><p>{editingSystem ? `${ENGINE_META[engine].label} 将使用宿主系统登录和上方模型。` : connection === "custom_endpoint" ? `测试会先保存当前 Base URL 与凭据，再使用上方模型发起真实请求。` : `${officialProvider} 官方账号使用默认服务地址和上方模型。`}</p></div>
        {state === "error" ? <p className="wbinding-error">{validationError || "请检查连接与模型配置。"}</p> : null}
        <ModelTestTerminal testing={testing} result={testResult} compact />
        <footer><button type="button" onClick={() => setEditing(false)}>取消</button><button type="button" disabled={state === "saving" || testing} onClick={testConnection}><Icon name="plug" size={12} />{testing ? "测试中…" : "保存并测试"}</button><button type="button" className="primary" disabled={state === "saving" || testing} onClick={save}>{state === "saving" ? "保存中…" : editingSystem ? "应用模型" : "保存并应用"}</button></footer>
      </div> : null}
    </div>
  );
}

function SeatInspector({
  seat,
  credentials,
  accounts,
  models,
  backend,
  health,
  testing,
  testResult,
  onUpdate,
  onEngine,
  onAccount,
  onAccountsChanged,
  onDiscoverModels,
  discoveringModels,
  onDuplicate,
  onDelete,
  onTest,
}: {
  seat: Seat | null;
  credentials: Credential[];
  accounts: CredentialAccount[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  health?: ProfileHealth;
  testing: boolean;
  testResult: WorkerModelTestResult | null;
  onUpdate: (patch: Partial<Seat>) => void;
  onEngine: (engine: Engine) => void;
  onAccount: (key: string, account?: CredentialAccount) => void;
  onAccountsChanged: () => Promise<void>;
  onDiscoverModels: (profileId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
  onDuplicate: () => void;
  onDelete: () => void;
  onTest: () => void;
}) {
  if (!seat) return <aside className="wset-inspector empty"><Icon name="grid" size={24} /><strong>选择一个 Worker</strong><span>模型服务连接、模型与并发设置会在这里显示。</span></aside>;
  const engine = engineOf(seat.engine);
  const credential = credentials.find((item) => item.id === seat.credential_id);
  const accountKey = credentialKey(credential);
  const selectedAccount = accounts.find((item) => item.account_id === accountKey);
  const requiresExplicitModel = Boolean(selectedAccount && accountConnection(selectedAccount) === "custom_endpoint" && !seat.model?.trim());
  const modelOptions = models.models[engine] || [];
  return (
    <aside className="wset-inspector">
      <header className="wset-inspector-head">
        <span>Worker 配置</span>
        <strong>{seat.label}</strong>
        <p>{ENGINE_META[engine].label} · {accountKey === "__system__" ? "系统登录" : accountKey || "未绑定"} · {seat.model || "默认模型"} · {effortSummary(seat.reasoning_effort, effortDefinition(engine, seat.model || "", modelOptions).defaultLevel)}</p>
      </header>
      <div className="wset-inspector-scroll">
        <section className="wset-form-section">
          <h3>身份</h3>
          <label><span>名称</span><input value={seat.label} onChange={(event) => onUpdate({ label: event.target.value })} /></label>
          <label><span>Worker 程序</span><select value={engine} onChange={(event) => onEngine(engineOf(event.target.value))}>{ENGINES.map((item) => <option key={item} value={item} disabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</option>)}</select></label>
        </section>

        <section className="wset-form-section">
          <h3>模型服务与凭据</h3>
          <CredentialBindingEditor engine={engine} profileId={seat.id} accountKey={accountKey} model={seat.model || ""} modelOptions={modelOptions} accounts={accounts} backend={backend} onBind={onAccount} onModelChange={(model) => { if (model !== (seat.model || "")) onUpdate({ model, reasoning_effort: normalizeEffortForModel(seat.reasoning_effort, engine, model, modelOptions) }); }} onAccountsChanged={onAccountsChanged} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={engine} model={seat.model || ""} options={modelOptions} value={seat.reasoning_effort || "default"} onChange={(reasoning_effort) => onUpdate({ reasoning_effort })} />
        </section>

        <section className="wset-form-section">
          <h3>普通调度</h3>
          <label><span>并发上限</span><NumberField min={1} max={32} value={seat.capacity.max_running} onChange={(next) => onUpdate({ capacity: { ...seat.capacity, max_running: Math.max(1, Number(next) || 1) } })} /></label>
          <div className="wset-switch-row"><span><b>参与首轮 Race</b><small>冷启动时参加并行快速侦察</small></span><Toggle label="参与首轮 Race" value={seat.race} onChange={(race) => onUpdate({ race })} /></div>
          <div className="wset-switch-row"><span><b>启用 Worker</b><small>停用后保留连接和模型配置</small></span><Toggle label="启用 Worker" value={seat.enabled} onChange={(enabled) => onUpdate({ enabled })} /></div>
        </section>

        <section className="wset-test-summary">
          <div><span className={`wroster-health ${health?.status === "ok" ? "ok" : health?.status === "auth_failed" || health?.status === "blocked" ? "bad" : "pending"}`}><i />{healthLabel(health)}</span><p>{health?.detail || "静态配置检查完成后，可在下方进行真实模型测试。"}</p></div>
        </section>
      </div>

      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}

      <footer className="wset-inspector-dock">
        <button type="button" className="primary" onClick={onTest} disabled={testing || requiresExplicitModel}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : requiresExplicitModel ? "请先选择模型" : "真实模型测试"}</button>
        <div><button type="button" onClick={onDuplicate}><Icon name="copy" size={13} />复制</button><button type="button" onClick={() => onUpdate({ enabled: false })}><Icon name="pause" size={13} />停用</button><button type="button" className="danger" onClick={onDelete}><Icon name="x" size={13} />删除</button></div>
      </footer>
    </aside>
  );
}

function ReviewInspector({
  seats,
  credentials,
  accounts,
  models,
  backend,
  review,
  onReview,
  onCreateDedicated,
  onSeatUpdate,
  onSeatEngine,
  onSeatAccount,
  onAccountsChanged,
  onDiscoverModels,
  discoveringModels,
  onEditOrdinary,
  onTest,
  testing,
  testResult,
}: {
  seats: Seat[];
  credentials: Credential[];
  accounts: CredentialAccount[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  review: ReviewPolicy;
  onReview: (patch: Partial<ReviewPolicy>) => void;
  onCreateDedicated: () => void;
  onSeatUpdate: (id: string, patch: Partial<Seat>) => void;
  onSeatEngine: (id: string, engine: Engine) => void;
  onSeatAccount: (id: string, key: string, account?: CredentialAccount) => void;
  onAccountsChanged: () => Promise<void>;
  onDiscoverModels: (profileId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
  onEditOrdinary: (id: string) => void;
  onTest: () => void;
  testing: boolean;
  testResult: WorkerModelTestResult | null;
}) {
  const options = seats.filter((seat) => canServeChannel(seat, "review"));
  const selected = options.find((seat) => seat.id === review.engine);
  const credential = credentials.find((item) => item.id === selected?.credential_id);
  const dedicated = Boolean(selected && !isOrdinarySeat(selected));
  const selectedEngine = engineOf(selected?.engine || "claude");
  const selectedAccount = credentialKey(credential);
  const modelOptions = models.models[selectedEngine] || [];
  return (
    <aside className="wset-inspector review">
      <header className="wset-inspector-head">
        <span>Review Worker</span>
        <strong>{selected?.label || "尚未指定"}</strong>
        <p>{selected ? `${ENGINE_META[engineOf(selected.engine)].label} · ${credential?.secret_ref || "系统登录"} · ${selected.model || "默认模型"} · ${effortSummary(selected.reasoning_effort)}` : "为独立审查通道指定一个 Worker 配置。"}</p>
      </header>
      <div className="wset-inspector-scroll">
      <section className="wset-form-section">
        <h3>Review 配置</h3>
        <div className="wset-switch-row"><span><b>启用 Review</b><small>按触发条件启动独立审查进程</small></span><Toggle label="启用 Review" value={review.enabled ?? true} onChange={(enabled) => onReview({ enabled })} /></div>
        <label><span>使用 Worker</span><select value={review.engine || ""} onChange={(event) => onReview({ engine: event.target.value })}>
          <option value="">选择 Worker</option>
          {options.map((seat) => <option key={seat.id} value={seat.id} disabled={!seat.enabled}>{seat.label} · {credentials.find((item) => item.id === seat.credential_id)?.secret_ref || "系统登录"}{seat.enabled ? "" : " · 已停用"}</option>)}
        </select></label>
        <div className="wset-binding-help"><span>普通并发与 Review 并发相互独立</span><button type="button" onClick={onCreateDedicated}>创建独立配置</button></div>
        <ReasoningEffortSelect engine={selectedEngine} model={selected?.model || ""} options={modelOptions} value={review.reasoning_effort || "inherit"} inherit onChange={(reasoning_effort) => onReview({ reasoning_effort })} />
        <label><span>超时</span><NumberField min={60} suffix="秒" value={review.timeout ?? 420} onChange={(next) => onReview({ timeout: Math.max(60, Number(next) || 420) })} /></label>
        <div className="wset-switch-row"><span><b>不可用时降级</b><small>改用下一个健康的 Review 配置</small></span><Toggle label="Review 不可用时降级" value={review.allow_review_fallback ?? false} onChange={(allow_review_fallback) => onReview({ allow_review_fallback })} /></div>
      </section>

      {selected ? dedicated ? (
        <section className="wset-form-section">
          <h3>独立 Review 运行绑定</h3>
          <label><span>名称</span><input value={selected.label} onChange={(event) => onSeatUpdate(selected.id, { label: event.target.value })} /></label>
          <label><span>Worker 程序</span><select value={selectedEngine} onChange={(event) => onSeatEngine(selected.id, engineOf(event.target.value))}>{ENGINES.map((item) => <option key={item} value={item} disabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</option>)}</select></label>
          <CredentialBindingEditor engine={selectedEngine} profileId={selected.id} accountKey={selectedAccount} model={selected.model || ""} modelOptions={modelOptions} accounts={accounts} backend={backend} onBind={(key, account) => onSeatAccount(selected.id, key, account)} onModelChange={(model) => { if (model !== (selected.model || "")) onSeatUpdate(selected.id, { model, reasoning_effort: normalizeEffortForModel(selected.reasoning_effort, selectedEngine, model, modelOptions) }); }} onAccountsChanged={onAccountsChanged} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={selectedEngine} model={selected.model || ""} options={modelOptions} value={selected.reasoning_effort || "default"} onChange={(reasoning_effort) => onSeatUpdate(selected.id, { reasoning_effort })} />
          <div className="wset-switch-row"><span><b>启用 Review Worker</b><small>停用后保留连接和模型配置</small></span><Toggle label="启用 Review Worker" value={selected.enabled} onChange={(enabled) => onSeatUpdate(selected.id, { enabled })} /></div>
        </section>
      ) : (
        <section className="wset-form-section wreview-reuse-note">
          <h3>复用普通 Worker</h3>
          <p>Review 使用该 Worker 的模型服务连接、模型、模型等级和运行环境，同时保持独立并发；上方可以单独覆盖模型等级。</p>
          <button type="button" onClick={() => onEditOrdinary(selected.id)}><Icon name="chevronRight" size={13} />编辑 {selected.label}</button>
        </section>
      ) : null}

      <details className="wset-review-triggers">
        <summary><span><Icon name="gear" size={13} />触发条件</span><Icon name="chevronDown" size={13} /></summary>
        <div>
          {[
            ["after_race", "首轮 Race 未解决"],
            ["on_course_correct", "协调器判断需要纠偏"],
            ["on_reason_dry", "Reason 无新计划"],
            ["on_candidate_spike", "候选结果突然增加"],
            ["on_operator_hint", "收到操作员提示"],
          ].map(([key, label]) => (
            <div className="wset-switch-row" key={key}><span><b>{label}</b></span><Toggle label={label} value={Boolean(review[key as keyof ReviewPolicy])} onChange={(value) => onReview({ [key]: value })} /></div>
          ))}
          <label><span>连续无成果 Worker</span><NumberField min={0} value={review.after_fruitless_workers ?? 3} onChange={(next) => onReview({ after_fruitless_workers: Math.max(0, Number(next) || 0) })} /></label>
          <label><span>候选结果阈值</span><NumberField min={1} value={review.candidate_spike_threshold ?? 5} onChange={(next) => onReview({ candidate_spike_threshold: Math.max(1, Number(next) || 1) })} /></label>
        </div>
      </details>

      <section className="wset-test-summary">
        <span className="wreview-independent"><Icon name="eye" size={13} />独立并发 1</span>
        <p>Review 只审计共享图并输出控制建议，不占普通 Worker 的并发上限。</p>
      </section>
      </div>
      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}
      <footer className="wset-inspector-dock review">
        <button type="button" className="primary" onClick={onTest} disabled={!selected || testing}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : "测试 Review 模型"}</button>
      </footer>
    </aside>
  );
}

function VerifierInspector({
  seats,
  credentials,
  accounts,
  models,
  backend,
  verifier,
  onVerifier,
  onCreateDedicated,
  onSeatUpdate,
  onSeatEngine,
  onSeatAccount,
  onAccountsChanged,
  onDiscoverModels,
  discoveringModels,
  onEditOrdinary,
  onTest,
  testing,
  testResult,
}: {
  seats: Seat[];
  credentials: Credential[];
  accounts: CredentialAccount[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  verifier: VerifierPolicy;
  onVerifier: (patch: Partial<VerifierPolicy>) => void;
  onCreateDedicated: () => void;
  onSeatUpdate: (id: string, patch: Partial<Seat>) => void;
  onSeatEngine: (id: string, engine: Engine) => void;
  onSeatAccount: (id: string, key: string, account?: CredentialAccount) => void;
  onAccountsChanged: () => Promise<void>;
  onDiscoverModels: (profileId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
  onEditOrdinary: (id: string) => void;
  onTest: () => void;
  testing: boolean;
  testResult: WorkerModelTestResult | null;
}) {
  const options = seats.filter((seat) => canServeChannel(seat, "verifier"));
  const selected = options.find((seat) => seat.id === verifier.engine);
  const credential = credentials.find((item) => item.id === selected?.credential_id);
  const dedicated = Boolean(selected && !isOrdinarySeat(selected));
  const selectedEngine = engineOf(selected?.engine || "claude");
  const selectedAccount = credentialKey(credential);
  const modelOptions = models.models[selectedEngine] || [];
  const concurrentLabel = verifier.max_concurrent && verifier.max_concurrent > 0
    ? String(verifier.max_concurrent)
    : "按报告";
  return (
    <aside className="wset-inspector verifier">
      <header className="wset-inspector-head">
        <span>Verifier Worker</span>
        <strong>{selected?.label || "尚未指定"}</strong>
        <p>{selected ? `${ENGINE_META[engineOf(selected.engine)].label} · ${credential?.secret_ref || "系统登录"} · ${selected.model || "默认模型"} · ${effortSummary(selected.reasoning_effort)}` : "为独立复现验证通道指定一个 Worker 配置。"}</p>
      </header>
      <div className="wset-inspector-scroll">
      <section className="wset-form-section">
        <h3>Verifier 配置</h3>
        <div className="wset-switch-row"><span><b>启用 Verifier</b><small>race-scout 期间独立复现已提交报告</small></span><Toggle label="启用 Verifier" value={verifier.enabled ?? true} onChange={(enabled) => onVerifier({ enabled })} /></div>
        <label><span>使用 Worker</span><select value={verifier.engine || ""} onChange={(event) => onVerifier({ engine: event.target.value })}>
          <option value="">选择 Worker</option>
          {options.map((seat) => <option key={seat.id} value={seat.id} disabled={!seat.enabled}>{seat.label} · {credentials.find((item) => item.id === seat.credential_id)?.secret_ref || "系统登录"}{seat.enabled ? "" : " · 已停用"}</option>)}
        </select></label>
        <div className="wset-binding-help"><span>普通并发与 Verifier 并发相互独立</span><button type="button" onClick={onCreateDedicated}>创建独立配置</button></div>
        <ReasoningEffortSelect engine={selectedEngine} model={selected?.model || ""} options={modelOptions} value={verifier.reasoning_effort || "inherit"} inherit onChange={(reasoning_effort) => onVerifier({ reasoning_effort })} />
        <label><span>超时</span><NumberField min={60} suffix="秒" value={verifier.timeout ?? 240} onChange={(next) => onVerifier({ timeout: Math.max(60, Number(next) || 240) })} /></label>
        <label><span>独立并发</span><NumberField min={0} value={verifier.max_concurrent ?? 0} onChange={(next) => onVerifier({ max_concurrent: Math.max(0, Number(next) || 0) })} /><small>0 = 每份待审报告派一个 Verifier</small></label>
        <div className="wset-switch-row"><span><b>不可用时降级</b><small>改用下一个健康的 Verifier 配置</small></span><Toggle label="Verifier 不可用时降级" value={verifier.allow_verifier_fallback ?? false} onChange={(allow_verifier_fallback) => onVerifier({ allow_verifier_fallback })} /></div>
      </section>

      {selected ? dedicated ? (
        <section className="wset-form-section">
          <h3>独立 Verifier 运行绑定</h3>
          <label><span>名称</span><input value={selected.label} onChange={(event) => onSeatUpdate(selected.id, { label: event.target.value })} /></label>
          <label><span>Worker 程序</span><select value={selectedEngine} onChange={(event) => onSeatEngine(selected.id, engineOf(event.target.value))}>{ENGINES.map((item) => <option key={item} value={item} disabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</option>)}</select></label>
          <CredentialBindingEditor engine={selectedEngine} profileId={selected.id} accountKey={selectedAccount} model={selected.model || ""} modelOptions={modelOptions} accounts={accounts} backend={backend} onBind={(key, account) => onSeatAccount(selected.id, key, account)} onModelChange={(model) => { if (model !== (selected.model || "")) onSeatUpdate(selected.id, { model, reasoning_effort: normalizeEffortForModel(selected.reasoning_effort, selectedEngine, model, modelOptions) }); }} onAccountsChanged={onAccountsChanged} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={selectedEngine} model={selected.model || ""} options={modelOptions} value={selected.reasoning_effort || "default"} onChange={(reasoning_effort) => onSeatUpdate(selected.id, { reasoning_effort })} />
          <div className="wset-switch-row"><span><b>启用 Verifier Worker</b><small>停用后保留连接和模型配置</small></span><Toggle label="启用 Verifier Worker" value={selected.enabled} onChange={(enabled) => onSeatUpdate(selected.id, { enabled })} /></div>
        </section>
      ) : (
        <section className="wset-form-section wreview-reuse-note">
          <h3>复用普通 Worker</h3>
          <p>Verifier 使用该 Worker 的模型服务连接、模型和运行环境，同时保持独立并发；上方可以单独覆盖模型等级与并发上限。</p>
          <button type="button" onClick={() => onEditOrdinary(selected.id)}><Icon name="chevronRight" size={13} />编辑 {selected.label}</button>
        </section>
      ) : null}

      <section className="wset-test-summary">
        <span className="wreview-independent"><Icon name="check" size={13} />独立并发 {concurrentLabel}</span>
        <p>Verifier 只复现已提交漏洞报告，不占普通 Worker 的并发上限。</p>
      </section>
      </div>
      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}
      <footer className="wset-inspector-dock verifier">
        <button type="button" className="primary" onClick={onTest} disabled={!selected || testing}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : "测试 Verifier 模型"}</button>
      </footer>
    </aside>
  );
}

function AccountsWorkspace({
  accounts,
  seats,
  credentials,
  backend,
  onChanged,
  onAddWorker,
}: {
  accounts: CredentialAccount[];
  seats: Seat[];
  credentials: Credential[];
  backend: WorkerSettings["worker_backend"];
  onChanged: () => Promise<void>;
  onAddWorker: (accountId: string) => void;
}) {
  const [selectedId, setSelectedId] = useState<string | null>(accounts[0]?.account_id || null);
  const selected = accounts.find((account) => account.account_id === selectedId) || null;
  const [accountId, setAccountId] = useState(selected?.account_id || "");
  const [engine, setEngine] = useState<Engine>(accountWorkerEngine(selected || accounts[0]) || "claude");
  const [connection, setConnection] = useState<AccountConnection>(selected ? accountConnection(selected) : "official");
  const [provider, setProvider] = useState(selected ? accountProvider(selected) : "");
  const [secret, setSecret] = useState("");
  const [baseUrl, setBaseUrl] = useState(selected ? accountBaseUrl(selected) : "");
  const [state, setState] = useState<SaveState>("idle");
  const [testResult, setTestResult] = useState<{ ok: boolean; detail: string } | null>(null);

  const usageByAccount = useMemo(() => {
    const credentialToAccount = new Map(credentials.map((credential) => [credential.id, credential.secret_ref]));
    const result = new Map<string, Seat[]>();
    seats.forEach((seat) => {
      const key = credentialToAccount.get(seat.credential_id);
      if (!key) return;
      result.set(key, [...(result.get(key) || []), seat]);
    });
    return result;
  }, [credentials, seats]);
  const selectedUsage = selected ? usageByAccount.get(selected.account_id) || [] : [];

  useEffect(() => {
    if (!selected) return;
    setAccountId(selected.account_id);
    setEngine(accountWorkerEngine(selected) || "claude");
    setConnection(accountConnection(selected));
    setProvider(accountProvider(selected));
    setSecret("");
    setBaseUrl(accountBaseUrl(selected));
    setState("idle");
    setTestResult(null);
  }, [selected]);

  const newAccount = () => { setSelectedId(null); setAccountId(""); setEngine("claude"); setConnection("official"); setProvider(""); setSecret(""); setBaseUrl(""); setState("idle"); setTestResult(null); };
  const save = async () => {
    if (!accountId.trim() || (!selected && !secret.trim()) || (connection === "custom_endpoint" && !baseUrl.trim())) { setState("error"); return; }
    setState("saving");
    const body = {
      engine: connection === "custom_endpoint" ? "api" : engine,
      worker_engine: engine,
      target_engine: connection === "custom_endpoint" ? engine : undefined,
      connection,
      provider: connection === "custom_endpoint" ? provider.trim() : undefined,
      base_url: connection === "custom_endpoint" ? baseUrl.trim() : "",
      ...(connection === "official" && engine === "codex" ? { codex_auth_json: secret } : { secret }),
    };
    const result = await putCredentialAccount(accountId.trim(), body);
    setState(result ? "saved" : "error");
    if (result) { setSelectedId(result.account_id); await onChanged(); }
  };
  const test = async () => { setTestResult(null); setTestResult(await testCredentialAccount(accountId, engine, backend)); };
  const officialProvider = engine === "claude" ? "Anthropic" : engine === "codex" ? "OpenAI" : engine === "cursor" ? "Cursor" : ENGINE_META[engine].label;
  const credentialLabel = connection === "custom_endpoint" ? "API Key" : engine === "claude" ? "Claude OAuth Token" : engine === "codex" ? "Codex auth.json" : engine === "cursor" ? "Cursor API Key" : "API Key";

  return (
    <div className="wsettings-subworkspace accounts-v2">
      <section className="wsettings-resource-list">
        <div className="wsettings-subhead"><div><strong>账号与模型服务</strong><span>每个账号明确指定一个 Worker 引擎；保存后才能加入出战阵容。</span></div><button type="button" onClick={newAccount}><Icon name="grid" size={13} />添加账号</button></div>
        <div className="waccount-column-head"><span>账号与模型服务</span><span>Worker 引擎</span><span>使用情况</span></div>
        <div className="wsettings-account-list v2">
          {accounts.map((account) => {
            const accountEngine = accountWorkerEngine(account);
            const uses = usageByAccount.get(account.account_id) || [];
            const baseUrl = accountBaseUrl(account);
            return (
              <article key={account.account_id} className={selectedId === account.account_id ? "on" : ""}>
                <button type="button" className="waccount-select" onClick={() => setSelectedId(account.account_id)}>
                  <span className="wsettings-account-icon">{accountEngine ? <EngineLogo engine={accountEngine} size={18} title={ENGINE_META[accountEngine].label} /> : <Icon name="alert" size={16} />}</span>
                  <span className="waccount-copy"><strong>{account.account_id}</strong><small>{accountProvider(account)} · {connectionLabel(account)}</small><code>{baseUrl || "使用默认服务地址"}</code></span>
                  <span className="waccount-engine">{accountEngine ? ENGINE_META[accountEngine].label : "需要指定"}</span>
                  <span className={`waccount-status${account.present ? " ok" : ""}`}><i />{account.present ? "凭据已保存" : "缺少凭据"}</span>
                  <Icon name="chevronRight" size={13} />
                </button>
                <div className="waccount-usage"><strong>{uses.length}</strong><span>Worker</span></div>
              </article>
            );
          })}
          {!accounts.length ? <div className="wroster-empty"><Icon name="lock" size={22} /><strong>先添加一个账号</strong><span>确定 Worker 引擎和模型服务后，再把账号加入阵容。</span></div> : null}
        </div>
      </section>
      <aside className="wsettings-editor">
        <header className="wset-inspector-head"><span>{selected ? "账号映射" : "添加账号"}</span><strong>{accountId || "新账号"}</strong><p>账号保存模型服务信息，并限定可使用它的 Worker 引擎。</p></header>
        <section className="waccount-relation" aria-label="账号对应关系">
          <div><span>模型服务</span><strong>{connection === "custom_endpoint" ? provider || "待填写" : officialProvider}</strong><small>{connection === "custom_endpoint" ? "自定义 API" : "官方账号"}</small></div>
          <Icon name="chevronRight" size={15} />
          <div><span>用于</span><strong>{ENGINE_META[engine].label}</strong><small>Worker 引擎</small></div>
        </section>
        <section className="wset-form-section">
          <h3>账号定义</h3>
          <label><span>账号 ID</span><input value={accountId} disabled={Boolean(selected)} placeholder="例如 gateway-claude" onChange={(event) => setAccountId(event.target.value)} /></label>
          <label><span>Worker 程序</span><select value={engine} disabled={Boolean(selected && selectedUsage.length)} onChange={(event) => setEngine(engineOf(event.target.value))}>{ENGINES.map((item) => <option key={item} value={item} disabled={Boolean(ENGINE_META[item].localOnly)}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（使用系统登录配置）" : ""}</option>)}</select></label>
          <label><span>模型服务连接</span><select value={connection} disabled={Boolean(selected && selectedUsage.length)} onChange={(event) => setConnection(event.target.value as AccountConnection)}><option value="official">官方账号</option><option value="custom_endpoint">自定义 API</option></select></label>
          {connection === "custom_endpoint" ? <><label><span>显示名称</span><input value={provider} placeholder="可选，例如公司代理" onChange={(event) => setProvider(event.target.value)} /></label><label><span>Base URL</span><input value={baseUrl} placeholder="https://api.example.com/v1" onChange={(event) => setBaseUrl(event.target.value)} /></label></> : null}
          <label><span>{credentialLabel}</span><textarea value={secret} rows={5} placeholder={selected ? "留空以保留已保存凭据" : `输入 ${credentialLabel}`} onChange={(event) => setSecret(event.target.value)} /></label>
          <div className="waccount-protocol"><span>接口协议</span><strong>{ENGINE_META[engine].protocol}</strong><p>{connection === "custom_endpoint" ? `该 Base URL 必须兼容 ${ENGINE_META[engine].label} 使用的 ${ENGINE_META[engine].protocol}。` : `${officialProvider} 官方账号使用默认服务地址。`}</p></div>
          {selected && selectedUsage.length ? <p className="waccount-lock-note"><Icon name="lock" size={12} />该账号已被 Worker 使用。移除关联 Worker 后，才可更改 Worker 程序或连接方式。</p> : null}
          <p className="waccount-model-note"><Icon name="crosshair" size={13} />具体模型在 Worker 中设置，同一账号可以创建多个使用不同模型的 Worker。</p>
        </section>
        <section className="waccount-bound-workers"><header><span>已绑定 Worker</span><strong>{selectedUsage.length}</strong></header>{selectedUsage.length ? selectedUsage.map((seat) => <div key={seat.id}><EngineLogo engine={seat.engine} size={14} /><span><strong>{seat.label}</strong><small>{seat.model || "默认模型"}</small></span><em>{seat.enabled ? "启用" : "停用"}</em></div>) : <p>当前账号还没有加入出战阵容。</p>}</section>
        {testResult ? <p className={`wsettings-result ${testResult.ok ? "ok" : "bad"}`}>{testResult.ok ? "连接成功" : "连接失败"} · {testResult.detail}</p> : null}
        <footer className="wsettings-editor-actions accounts">{selected ? <button type="button" className="danger" disabled={selectedUsage.length > 0} title={selectedUsage.length ? "请先从阵容中移除使用该账号的 Worker" : "删除账号"} onClick={async () => { if (await deleteCredentialAccount(selected.account_id)) { newAccount(); await onChanged(); } }}><Icon name="x" size={13} />删除</button> : <span />}<button type="button" onClick={test} disabled={!selected}><Icon name="plug" size={13} />测试</button><button type="button" className="primary" onClick={save} disabled={state === "saving"}><Icon name="check" size={13} />{state === "saving" ? "保存中…" : "保存账号"}</button>{selected ? <button type="button" className="next" disabled={!selected.present} onClick={() => onAddWorker(selected.account_id)}><Icon name="grid" size={13} />加入阵容</button> : null}</footer>
      </aside>
    </div>
  );
}

function RuntimeModeOption({
  active,
  description,
  icon,
  label,
  onSelect,
}: {
  active: boolean;
  description: string;
  icon: IconName;
  label: string;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      className={`wruntime-mode${active ? " on" : ""}`}
      aria-pressed={active}
      onClick={onSelect}
    >
      <span className="wruntime-mode-radio" aria-hidden="true"><i /></span>
      <span className="wruntime-mode-icon"><Icon name={icon} size={26} /></span>
      <span className="wruntime-mode-copy"><strong>{label}</strong><small>{description}</small></span>
    </button>
  );
}

function RuntimeWorkspace({ backend, network, seatCount, imageStatus, imageLoading, pulling, onBackend, onNetwork, onRefreshImage, onPullImage }: {
  backend: WorkerSettings["worker_backend"];
  network: WorkerSettings["worker_network"];
  seatCount: number;
  imageStatus: WorkerImageStatus | null;
  imageLoading: boolean;
  pulling: boolean;
  onBackend: (backend: WorkerSettings["worker_backend"]) => void;
  onNetwork: (network: WorkerSettings["worker_network"]) => void;
  onRefreshImage: () => void;
  onPullImage: () => void;
}) {
  const networkOptions: { id: WorkerSettings["worker_network"]; label: string; detail: string }[] = [
    { id: "bridge", label: "Bridge", detail: "容器可访问外部网络，使用 Docker 隔离网络" },
    { id: "host", label: "Host", detail: "容器直接使用宿主机网络" },
    { id: "none", label: "None", detail: "请求 none 时会被改写为 bridge，Worker shell 仍可访问网络" },
  ];
  return (
    <div className="wruntime-workspace">
      <section className="wruntime-stage">
        <header className="wsettings-section-head wruntime-intro">
          <div className="wsettings-section-copy"><h2>执行环境</h2>
          <p>所有 Worker 与 Review Worker 使用同一运行环境。</p></div>
        </header>

        <div className="wruntime-modes" role="group" aria-label="执行环境">
          <RuntimeModeOption active={backend === "local"} icon="terminal" label="本地运行" description="使用宿主机 CLI，可继承系统登录" onSelect={() => onBackend("local")} />
          <RuntimeModeOption active={backend === "container"} icon="layers" label="容器运行" description="隔离运行，需要绑定可注入凭据" onSelect={() => onBackend("container")} />
        </div>

        {backend === "container" ? <>
          <section className="wruntime-network" aria-labelledby="wruntime-network-title">
            <h3 id="wruntime-network-title">容器网络</h3>
            <div className="wruntime-network-list">
              {networkOptions.map((option) => <button key={option.id} type="button" className={`wruntime-network-option${network === option.id ? " on" : ""}`} aria-pressed={network === option.id} onClick={() => onNetwork(option.id)}><span><strong>{option.label}</strong><small>{option.detail}</small></span><Icon name={network === option.id ? "check" : "chevronRight"} size={15} /></button>)}
            </div>
          </section>

          <section className="wruntime-image" aria-labelledby="wruntime-image-title">
            <header><div><h3 id="wruntime-image-title">Worker 镜像</h3><p>检查 Docker 服务、镜像是否存在以及镜像版本。</p></div><code>{imageStatus?.image || "读取中…"}</code></header>
            <div className="wruntime-image-checks">
              <span className={imageStatus?.daemon.ok ? "ok" : "bad"}><i />Docker 服务<strong>{imageLoading ? "检查中" : imageStatus?.daemon.ok ? "可用" : "不可用"}</strong></span>
              <span className={imageStatus?.pulled.ok ? "ok" : "bad"}><i />本地镜像<strong>{imageLoading ? "检查中" : imageStatus?.pulled.ok ? "已存在" : "未找到"}</strong></span>
              <span className={imageStatus?.version.status === "match" ? "ok" : imageStatus?.version.status === "mismatch" ? "bad" : "unknown"}><i />镜像版本<strong>{imageLoading ? "检查中" : imageStatus?.version.actual || "未知"}</strong></span>
            </div>
            {imageStatus?.version.status === "mismatch" && imageStatus.version.expected ? <p className="wruntime-image-warning"><Icon name="alert" size={14} />期望版本 {imageStatus.version.expected}，建议重新拉取镜像。</p> : null}
            <footer><span><Icon name="terminal" size={13} />可通过 <code>{WORKER_IMAGE_ENV}</code> 指定镜像</span><div><button type="button" onClick={onRefreshImage} disabled={imageLoading || pulling}><Icon name="refresh" size={13} />刷新</button><button type="button" className="primary" onClick={onPullImage} disabled={pulling || !imageStatus?.daemon.ok}><Icon name="layers" size={13} />{pulling ? "拉取中…" : "拉取镜像"}</button></div></footer>
          </section>
        </> : <div className="wruntime-local-note"><Icon name="terminal" size={18} /><span><strong>本地运行不使用 Worker 镜像</strong><small>Worker 直接调用宿主机上的 CLI，并继承宿主机登录状态。</small></span></div>}
      </section>

      <aside className="wruntime-summary" aria-label="当前生效配置">
        <header><h2>当前生效配置</h2></header>
        <dl>
          <div><dt>执行位置</dt><dd>{backend === "local" ? "宿主机" : "Docker 容器"}</dd></div>
          <div><dt>容器网络</dt><dd>{backend === "container" ? <code>{network}</code> : "不适用"}</dd></div>
          <div><dt>影响范围</dt><dd>{seatCount} 个 Worker</dd></div>
        </dl>
        <p><Icon name="help" size={15} />保存后统一应用到普通 Worker 与 Review Worker。</p>
      </aside>
    </div>
  );
}

function SchedulingWorkspace({ config, raceScout, raceTimeout, startWorkers, maxTotal, wallClock, costBudget, maxWorkers, onChange }: { config: WorkerSettings; raceScout: boolean; raceTimeout: number; startWorkers: number; maxTotal: number; wallClock: number; costBudget: number; maxWorkers: number; onChange: (patch: Partial<{ raceScout: boolean; raceTimeout: number; startWorkers: number; maxTotal: number; wallClock: number; costBudget: number }>) => void }) {
  return (
    <div className="wsettings-simple-page">
      <header className="wsettings-section-head"><div className="wsettings-section-copy"><h2>调度与预算</h2><p>这里只控制阶段与上限，不定义 Worker 职业。</p></div></header>
      <section className="wsettings-simple-grid">
        <div className="wsettings-setting-group"><h3>启动策略</h3><div className="wset-switch-row"><span><b>首轮 Race</b><small>冷启动时让已勾选 Worker 并行快速侦察</small></span><Toggle label="首轮 Race" value={raceScout} onChange={(value) => onChange({ raceScout: value })} /></div><label><span>Race 超时</span><NumberField min={60} suffix="秒" value={raceTimeout} onChange={(next) => onChange({ raceTimeout: Math.max(60, Number(next) || 60) })} /></label><label><span>协调阶段初始 Worker</span><NumberField min={1} max={Math.max(1, maxWorkers)} value={startWorkers} onChange={(next) => onChange({ startWorkers: Math.max(1, Number(next) || 1) })} /></label></div>
        <div className="wsettings-setting-group"><h3>运行上限</h3><div className="wsettings-derived"><span>普通并发上限</span><strong>{maxWorkers}</strong><small>由阵容中各 Worker 的并发数相加得出。此处看到的是当前设置，对下一次派发生效。</small></div><label><span>累计 Worker 上限</span><NumberField min={0} value={maxTotal} onChange={(next) => onChange({ maxTotal: Math.max(0, Number(next) || 0) })} /></label><label><span>最长运行时间</span><NumberField min={0} suffix="秒" value={wallClock} onChange={(next) => onChange({ wallClock: Math.max(0, Number(next) || 0) })} /></label><label><span>成本预算</span><NumberField min={0} step={0.1} suffix="USD" value={costBudget} onChange={(next) => onChange({ costBudget: Math.max(0, Number(next) || 0) })} /></label></div>
      </section>
      {Object.keys(config.overrides || {}).length ? <p className="wsettings-inline-note"><Icon name="alert" size={13} />当前还存在 {Object.keys(config.overrides).length} 个按题型覆盖配置；本轮重构会原样保留，不会静默删除。</p> : null}
    </div>
  );
}

function LlmProfileCard({
  profile,
  result,
  testing,
  visible,
  which,
  onTest,
  onToggleVisible,
  onUpdate,
}: {
  profile: WorkerSettings["llm_profiles"][LlmProfileName];
  result?: { ok: boolean; detail: string };
  testing: boolean;
  visible: boolean;
  which: LlmProfileName;
  onTest: () => void;
  onToggleVisible: () => void;
  onUpdate: (patch: Partial<WorkerSettings["llm_profiles"]["planner"]>) => void;
}) {
  const custom = (profile.connection || (profile.base_url ? "custom_endpoint" : "default")) === "custom_endpoint";
  const temperatureMode = llmTemperatureMode(profile);
  const customTemperature = temperatureMode !== "default";
  const enteredKey = Boolean(profile.api_key?.trim());
  const credentialText = enteredKey
    ? "已输入新 Key，保存配置后生效"
    : profile.clear_api_key
      ? "保存配置后清除当前 Key"
      : profile.credential_source === "saved"
        ? "正在使用已保存的专用 Key"
        : profile.credential_source === "environment"
          ? "正在使用环境变量 MUTEKI_DEEPSEEK_API_KEY"
          : "当前没有可用 Key";
  return (
    <article className="wsettings-setting-group wmodel-settings-card">
      <header><div><h3>{which === "planner" ? "Reason / Planner" : "Titler"}</h3><p>{which === "planner" ? "负责协调器规划与进度收敛" : "负责会话标题与辅助命名"}</p></div><span className={profile.credential_source === "missing" && !enteredKey ? "missing" : ""}>{custom ? "自定义端点" : "默认 DeepSeek"}</span></header>
      <label><span>连接方式</span><select value={custom ? "custom_endpoint" : "default"} onChange={(event) => onUpdate(event.target.value === "custom_endpoint" ? { connection: "custom_endpoint" } : { connection: "default", provider: "deepseek", base_url: "" })}><option value="default">默认 DeepSeek</option><option value="custom_endpoint">自定义端点</option></select></label>
      {custom ? <>
        <label><span>Provider</span><input value={profile.provider} placeholder="例如 deepseek、openrouter" onChange={(event) => onUpdate({ provider: event.target.value })} /></label>
        <label><span>Base URL</span><input value={profile.base_url || ""} placeholder="https://api.example.com/v1" onChange={(event) => onUpdate({ base_url: event.target.value })} /></label>
      </> : <div className="wmodel-default-endpoint"><span>服务地址</span><strong>https://api.deepseek.com/v1</strong><small>可通过 MUTEKI_DEEPSEEK_BASE_URL 修改部署默认值</small></div>}
      <label><span>模型</span><input value={profile.model} placeholder="输入服务实际支持的模型 ID" onChange={(event) => onUpdate({ model: event.target.value })} /></label>
      <div className="wset-switch-row wmodel-temperature-switch"><span><b>自定义 Temperature</b><small>关闭时沿用各调用的内置默认值；开启后可指定数值，或不发送该参数</small></span><Toggle label="自定义 Temperature" value={customTemperature} onChange={(on) => onUpdate({ temperature_mode: on ? (temperatureMode === "omit" ? "omit" : "custom") : "default", temperature: profile.temperature ?? 1 })} /></div>
      {customTemperature ? <>
        <label><span>发送策略</span><select value={temperatureMode === "omit" ? "omit" : "custom"} onChange={(event) => onUpdate({ temperature_mode: event.target.value === "omit" ? "omit" : "custom" })}><option value="custom">发送指定值</option><option value="omit">不发送该参数</option></select></label>
        {temperatureMode === "omit" ? <p className="wmodel-temperature-note">请求体不带 temperature，用于拒绝该字段的推理模型。</p> : <label><span>Temperature</span><NumberField min={0} max={2} step={0.1} value={profile.temperature ?? 1} onChange={(next) => onUpdate({ temperature: Number(next) })} /></label>}
      </> : null}
      <label className="wmodel-key-field"><span>API Key</span><div><input type={visible ? "text" : "password"} value={profile.api_key || ""} autoComplete="new-password" placeholder={profile.credential_source === "saved" ? "已保存，留空保持不变" : profile.credential_source === "environment" ? "当前使用环境变量，输入后覆盖" : "输入 API Key"} onChange={(event) => onUpdate({ api_key: event.target.value, clear_api_key: false })} /><button type="button" onClick={onToggleVisible} aria-label={visible ? "隐藏 API Key" : "显示 API Key"} title={visible ? "隐藏 API Key" : "显示 API Key"}><Icon name={visible ? "eyeOff" : "eye"} size={14} /></button>{profile.credential_source === "saved" && !enteredKey && !profile.clear_api_key ? <button type="button" className="clear" onClick={() => onUpdate({ api_key: "", clear_api_key: true, credential_source: "missing" })}>清除</button> : null}</div></label>
      <p className={`wmodel-credential-note${profile.credential_source === "missing" && !enteredKey ? " missing" : ""}`}><Icon name={profile.credential_source === "missing" && !enteredKey ? "alert" : "lock"} size={13} />{credentialText}</p>
      {result ? <p className={`wmodel-test-result ${result.ok ? "ok" : "bad"}`}><Icon name={result.ok ? "check" : "xCircle"} size={13} />{result.ok ? "连接成功" : "连接失败"} · {result.detail}</p> : null}
      <button type="button" className="wsettings-test-button" onClick={onTest} disabled={testing || !profile.model.trim() || (custom && !profile.base_url?.trim())}><Icon name="plug" size={13} />{testing ? "测试中…" : "测试端点"}</button>
    </article>
  );
}

function ModelsWorkspace({ value, onChange }: { value: WorkerSettings["llm_profiles"]; onChange: (next: WorkerSettings["llm_profiles"]) => void }) {
  const [testing, setTesting] = useState<LlmProfileName | null>(null);
  const [results, setResults] = useState<Partial<Record<LlmProfileName, { ok: boolean; detail: string }>>>({});
  const [visibleKeys, setVisibleKeys] = useState<Record<LlmProfileName, boolean>>({ planner: false, titler: false });
  const update = (which: LlmProfileName, patch: Partial<WorkerSettings["llm_profiles"]["planner"]>) => onChange({ ...value, [which]: { ...value[which], ...patch } });
  const test = async (which: LlmProfileName) => {
    setTesting(which);
    setResults((current) => { const next = { ...current }; delete next[which]; return next; });
    const row = value[which];
    const next = await testLlmEndpoint(which, row.base_url || "", row.model, row.api_key || "", llmTemperatureMode(row), row.temperature);
    setResults((current) => ({ ...current, [which]: next }));
    setTesting(null);
  };
  return (
    <div className="wsettings-simple-page wmodels-page">
      <header className="wsettings-section-head"><div className="wsettings-section-copy"><h2>推理模型</h2><p>Reason 负责规划节奏，Titler 负责短文本与辅助命名；两者可以分别使用默认 DeepSeek 或自定义 OpenAI 兼容端点。</p></div></header>
      <section className="wsettings-simple-grid models">
        {(["planner", "titler"] as const).map((which) => <LlmProfileCard key={which} which={which} profile={value[which]} testing={testing === which} result={results[which]} visible={visibleKeys[which]} onToggleVisible={() => setVisibleKeys((current) => ({ ...current, [which]: !current[which] }))} onUpdate={(patch) => update(which, patch)} onTest={() => void test(which)} />)}
      </section>
    </div>
  );
}

const SCHEME_NAMES: Record<string, string> = { azure: "湛蓝", violet: "紫罗兰", teal: "青", ember: "焦橙" };
const APPEARANCE_TOKENS = ["--blue", "--green", "--amber", "--cyan", "--pink", "--violet", "--magenta", "--red", "--gold"];

/**
 * 外观配色 — palette-engine 的控制台。预设方案是四个命名主色；自定义滑杆把
 * 任意 OKLCH 色相喂给引擎，语义色保持固定、装饰色自动避让、对比度由引擎
 * 保证 ≥ WCAG AA。所有改动即时应用到当前页面并持久化到本浏览器，
 * 主工作台下次加载（或切换亮暗模式）时沿用。
 */
function AppearanceWorkspace() {
  const [mode, setMode] = useState<ThemeMode>(() => (typeof window === "undefined" ? "dark" : readSavedTheme()));
  const [sel, setSel] = useState<SchemeSelection>(() => (typeof window === "undefined" ? { kind: "preset", id: "azure" } : readSavedSelection()));

  useEffect(() => {
    document.documentElement.dataset.theme = mode;
    try { window.localStorage.setItem("muteki.theme", mode); } catch { /* session-only theming */ }
    applySelection(sel, mode);
  }, [sel, mode]);

  const hue = Math.round(sel.kind === "custom" ? sel.hue : (SCHEMES.find((s) => s.id === sel.id)?.hue ?? 268));
  const palette = sel.kind === "custom" ? buildPaletteFromHue(sel.hue, mode) : buildPalette(sel.id, mode);
  const previewVars = palette as CSSProperties;

  return (
    <div className="wsettings-simple-page wappearance-page">
      <header className="wsettings-section-head"><div className="wsettings-section-copy">
        <h2>外观配色</h2>
        <p>配色引擎以主色色相为输入自动生成全套强调色：绿/琥珀/红/金等语义色固定不变，青/紫/品红/粉等装饰色与主色冲突时自动避让，主色对比度始终不低于 WCAG AA（4.5:1）。改动即时生效并保存在本浏览器。</p>
      </div></header>

      <section className="wappearance-card" aria-labelledby="wappearance-mode">
        <header><h3 id="wappearance-mode">显示模式</h3><span>{mode === "light" ? "亮色" : "暗色"}</span></header>
        <div className="wappearance-modes" role="radiogroup" aria-label="显示模式">
          <button type="button" role="radio" aria-checked={mode === "light"} className={mode === "light" ? "on" : ""} onClick={() => setMode("light")}><Icon name="sun" size={14} />亮色</button>
          <button type="button" role="radio" aria-checked={mode === "dark"} className={mode === "dark" ? "on" : ""} onClick={() => setMode("dark")}><Icon name="moon" size={14} />暗色</button>
        </div>
      </section>

      <section className="wappearance-card" aria-labelledby="wappearance-presets">
        <header><h3 id="wappearance-presets">预设方案</h3><span>{sel.kind === "preset" ? SCHEME_NAMES[sel.id] : "自定义"}</span></header>
        <div className="wappearance-presets">
          {SCHEMES.map((s) => {
            const p = buildPalette(s.id, mode);
            const on = sel.kind === "preset" && sel.id === s.id;
            return (
              <button key={s.id} type="button" className={`wappearance-preset${on ? " on" : ""}`} onClick={() => setSel({ kind: "preset", id: s.id })} aria-pressed={on}>
                <i style={{ background: p["--accent"] }} />
                <strong>{SCHEME_NAMES[s.id]}</strong>
                <code>{p["--accent"]}</code>
              </button>
            );
          })}
        </div>
      </section>

      <section className="wappearance-card" aria-labelledby="wappearance-custom">
        <header><h3 id="wappearance-custom">自定义主色</h3><span>{sel.kind === "custom" ? `色相 ${hue}° · ${palette["--accent"]}` : "拖动滑杆即生成"}</span></header>
        <input
          className="wappearance-hue"
          type="range"
          min={0}
          max={359}
          value={hue}
          onChange={(e) => setSel({ kind: "custom", hue: Number(e.target.value) })}
          aria-label="主色色相"
        />
        <div className="wappearance-family">
          {APPEARANCE_TOKENS.map((token) => (
            <span key={token} className="wappearance-swatch"><i style={{ background: palette[token] }} /><em>{token.slice(2)}</em><code>{palette[token]}</code></span>
          ))}
        </div>
      </section>

      <section className="wappearance-card" aria-labelledby="wappearance-preview">
        <header><h3 id="wappearance-preview">实时预览</h3><span>{mode === "light" ? "亮色" : "暗色"}模式</span></header>
        <div className="wappearance-preview" style={previewVars}>
          <div className="wp-chips">
            <span style={{ ["--c" as string]: "var(--blue)" }}>control</span>
            <span style={{ ["--c" as string]: "var(--green)" }}>worker</span>
            <span style={{ ["--c" as string]: "var(--amber)" }}>tool</span>
            <span style={{ ["--c" as string]: "var(--violet)" }}>evidence</span>
            <span style={{ ["--c" as string]: "var(--pink)" }}>review</span>
            <span style={{ ["--c" as string]: "var(--red)" }}>error</span>
            <span style={{ ["--c" as string]: "var(--gold)" }}>★ flag</span>
          </div>
          <div className="wp-ledger">
            <div><time>12:03:41</time><b style={{ color: "var(--blue)" }}>coordinator</b><span>dispatch intent #42 → worker-claude-1</span></div>
            <div><time>12:04:12</time><b style={{ color: "var(--green)" }}>flag</b><span>flag{"{a7f3…}"} verified from stdout</span></div>
          </div>
          <div className="wp-foot">
            <button type="button" className="wp-primary">＋ New Solve</button>
            <span className="wp-live"><i />LIVE · 3 workers</span>
          </div>
        </div>
      </section>
    </div>
  );
}

export function WorkerOrchestration() {
  const [config, setConfig] = useState<WorkerSettings | null>(null);
  const [seats, setSeats] = useState<Seat[]>([]);
  const [credentials, setCredentials] = useState<Credential[]>([]);
  const [accounts, setAccounts] = useState<CredentialAccount[]>([]);
  const [models, setModels] = useState<WorkerModelOptions>({
    allow_custom: true,
    manual_models: {},
    discovered_models: {},
    models_by_profile: {},
    discovery: {},
    models: {},
  });
  const [health, setHealth] = useState<Record<string, ProfileHealth>>({});
  const [section, setSection] = useState<SettingsSection>("roster");
  const [inspector, setInspector] = useState<"seat" | "review" | "verifier">("seat");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [review, setReview] = useState<ReviewPolicy>(DEFAULT_REVIEW);
  const [verifier, setVerifier] = useState<VerifierPolicy>(DEFAULT_VERIFIER);
  const [backend, setBackend] = useState<WorkerSettings["worker_backend"]>("local");
  const [network, setNetwork] = useState<WorkerSettings["worker_network"]>("bridge");
  const [imageStatus, setImageStatus] = useState<WorkerImageStatus | null>(null);
  const [imageLoading, setImageLoading] = useState(false);
  const [pullingImage, setPullingImage] = useState(false);
  const [raceScout, setRaceScout] = useState(true);
  const [raceTimeout, setRaceTimeout] = useState(720);
  const [startWorkers, setStartWorkers] = useState(1);
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
  const [systemLogins, setSystemLogins] = useState<Record<string, "present" | "absent" | "unknown">>({});
  const [feedback, setFeedback] = useState("");

  const returnTo = useMemo(() => {
    if (typeof window === "undefined") return "/";
    const value = new URLSearchParams(window.location.search).get("return") || "/";
    return value.startsWith("/") ? value : "/";
  }, []);

  const markDirty = useCallback(() => { setDirty(true); setSaveState("idle"); }, []);
  const showFeedback = useCallback((message: string) => { setFeedback(message); window.setTimeout(() => setFeedback(""), 3200); }, []);
  const refreshAccounts = useCallback(async () => {
    const rows = await listCredentialAccounts();
    setAccounts(rows);
    setCredentials((current) => syncCredentialsFromAccounts(current, rows));
  }, []);
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
  const refreshWorkerModels = useCallback(async (profileId: string, engine: Engine): Promise<ModelDiscoveryOutcome> => {
    setDiscoveringModels(true);
    try {
      const result = await discoverWorkerModels(profileId);
      if (Object.keys(result.models).length > 0) setModels(result);
      const engineRows = (result.discovery_results || []).filter((row) => row.engine === engine);
      const successful = engineRows.filter((row) => row.ok);
      if (successful.length > 0) {
        const modelCount = new Set(successful.flatMap((row) => row.models.map((item) => item.id))).size;
        const detail = `已从 ${ENGINE_META[engine].label} CLI 读取 ${modelCount} 个模型，列表已更新。`;
        showFeedback(detail);
        return { ok: true, detail };
      }
      if (engineRows.length > 0) {
        return { ok: false, detail: [...new Set(engineRows.map((row) => row.detail).filter(Boolean))].join(" · ") || `${ENGINE_META[engine].label} 未返回可用模型。` };
      }
      return { ok: false, detail: `当前已保存配置中没有可用于 ${ENGINE_META[engine].label} 模型发现的 Worker。` };
    } finally {
      setDiscoveringModels(false);
    }
  }, [showFeedback]);

  useEffect(() => {
    let alive = true;
    Promise.all([getWorkerSettings(), listCredentialAccounts(), getWorkerModelOptions(), fetchProfilesHealth(), getWorkerImageStatus(), getSystemLogin()]).then(([cfg, accountRows, modelRows, healthRows, workerImage, loginRows]) => {
      if (!alive || !cfg) return;
      const identity = legacyIdentity(cfg);
      const ordinary = identity.seats.filter(isOrdinarySeat);
      setConfig(cfg);
      setSeats(identity.seats);
      setCredentials(syncCredentialsFromAccounts(identity.credentials, accountRows));
      setAccounts(accountRows);
      setModels(modelRows);
      setHealth(Object.fromEntries(healthRows.map((item) => [item.profile_id, item])));
      setSelectedId(ordinary[0]?.id || identity.seats[0]?.id || null);
      setReview({ ...DEFAULT_REVIEW, ...(cfg.stage_policy.coordinator.review || {}) });
      setVerifier({ ...DEFAULT_VERIFIER, ...(cfg.stage_policy.coordinator.verifier || {}) });
      setBackend(cfg.worker_backend || "local");
      setNetwork(cfg.worker_network || "bridge");
      setImageStatus(workerImage);
      setSystemLogins(loginRows);
      setRaceScout(cfg.race_scout);
      setRaceTimeout(cfg.race_timeout);
      setStartWorkers(cfg.start_workers);
      setMaxTotal(cfg.max_total_workers);
      setWallClock(cfg.wall_clock_budget);
      setCostBudget(cfg.cost_budget_usd);
      setLlmProfiles(cfg.llm_profiles);
    });
    try { document.documentElement.dataset.theme = window.localStorage.getItem("muteki.theme") === "light" ? "light" : "dark"; } catch { document.documentElement.dataset.theme = "dark"; }
    return () => { alive = false; };
  }, []);

  const ordinarySeats = seats.filter(isOrdinarySeat);
  const selectedSeat = seats.find((seat) => seat.id === selectedId) || null;
  const maxWorkers = ordinarySeats.filter((seat) => seat.enabled).reduce((sum, seat) => sum + Math.max(1, seat.capacity.max_running || 1), 0);

  const ensureCredential = useCallback((engine: Engine, key: string, resolvedAccount?: CredentialAccount): string => {
    const existing = credentials.find((credential) => credential.engine === engine && credentialKey(credential) === key);
    const account = resolvedAccount || accounts.find((item) => item.account_id === key);
    const connection = account ? accountConnection(account) : null;
    if (existing) {
      if (account) {
        setCredentials((current) => current.map((credential) => credential.id === existing.id ? syncCredentialFromAccount(credential, [account]) : credential));
      }
      return existing.id;
    }
    const next: Credential = {
      id: randomId("cred", engine),
      label: key === "__system__" ? `${ENGINE_META[engine].label} 系统登录` : key || "未配置模型服务",
      engine,
      kind: key === "__system__" ? "system_inherit" : connection === "custom_endpoint" ? "custom_endpoint" : "engine_key",
      secret_ref: key === "__system__" ? "" : key,
      target_engine: connection === "custom_endpoint" ? engine : undefined,
      endpoint: connection === "custom_endpoint" ? { base_url: account ? accountBaseUrl(account) : "", wire_api: ENGINE_META[engine].wireApi } : undefined,
    };
    setCredentials((current) => [...current, next]);
    return next.id;
  }, [accounts, credentials]);

  const updateSeat = useCallback((id: string, patch: Partial<Seat>) => { setSeats((current) => current.map((seat) => seat.id === id ? { ...seat, ...patch } : seat)); markDirty(); }, [markDirty]);
  const bindAccount = useCallback((id: string, key: string, resolvedAccount?: CredentialAccount) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const account = resolvedAccount || accounts.find((item) => item.account_id === key);
    const engine = accountWorkerEngine(account) || engineOf(seat.engine);
    updateSeat(id, {
      engine,
      credential_id: ensureCredential(engine, key, account || undefined),
      model: engine === seat.engine ? seat.model : "",
      reasoning_effort: engine === seat.engine ? seat.reasoning_effort || "default" : "default",
    });
  }, [accounts, ensureCredential, seats, updateSeat]);

  const changeEngine = useCallback((id: string, engine: Engine) => {
    const matching = accounts.find((account) => accountMatchesEngine(account, engine));
    const key = matching?.account_id || (backend === "local" ? "__system__" : "");
    updateSeat(id, { engine, credential_id: ensureCredential(engine, key), model: "", reasoning_effort: "default" });
  }, [accounts, backend, ensureCredential, updateSeat]);

  const addSeat = useCallback((engine: Engine) => {
    const matching = accounts.find((account) => accountMatchesEngine(account, engine));
    const key = matching?.account_id || (backend === "local" ? "__system__" : "");
    const id = randomId("seat", engine);
    const sameEngineCount = ordinarySeats.filter((seat) => seat.engine === engine).length;
    const next: Seat = { id, label: `${ENGINE_META[engine].label} Worker ${sameEngineCount + 1}`, engine, credential_id: ensureCredential(engine, key), model: "", reasoning_effort: "default", roles: [...ORDINARY_ROLES, "review"], race: true, capacity: { max_running: 1, max_review_running: 0 }, priority: ordinarySeats.length * 10 + 10, enabled: true };
    setSeats((current) => [...current, next]);
    setSelectedId(id);
    setInspector("seat");
    markDirty();
    showFeedback(`已添加 ${ENGINE_META[engine].label} Worker，请在右侧确认模型服务连接`);
  }, [accounts, backend, ensureCredential, markDirty, ordinarySeats, showFeedback]);

  const duplicateSeat = useCallback((id: string) => { const source = seats.find((seat) => seat.id === id); if (!source) return; const next = { ...source, id: randomId("seat", engineOf(source.engine)), label: `${source.label} 副本`, capacity: { ...source.capacity }, roles: [...source.roles], priority: seats.length * 10 + 10 }; setSeats((current) => [...current, next]); setSelectedId(next.id); markDirty(); }, [markDirty, seats]);
  const deleteSeat = useCallback((id: string) => {
    const next = seats.filter((seat) => seat.id !== id);
    setSeats(next);
    setSelectedId(next.find(isOrdinarySeat)?.id || next[0]?.id || null);
    if (review.engine === id) {
      setReview((current) => ({ ...current, engine: next.find((seat) => canServeChannel(seat, "review") && seat.enabled)?.id || "" }));
    }
    if (verifier.engine === id) {
      setVerifier((current) => ({ ...current, engine: next.find((seat) => canServeChannel(seat, "verifier") && seat.enabled)?.id || "" }));
    }
    markDirty();
  }, [markDirty, review.engine, verifier.engine, seats]);
  const reorderSeats = useCallback((sourceId: string, targetId: string | null) => {
    setSeats((current) => {
      const ordinary = current.filter(isOrdinarySeat);
      const dedicated = current.filter((seat) => !isOrdinarySeat(seat));
      const from = ordinary.findIndex((seat) => seat.id === sourceId);
      if (from < 0) return current;
      const next = [...ordinary];
      const [moved] = next.splice(from, 1);
      // targetId === null → dropped on empty roster space: move to the end.
      if (targetId === null) {
        next.push(moved);
      } else {
        // Insert before the target when dragging up, after it when dragging
        // down (the target index shifts by one once the source is removed).
        const originalTo = ordinary.findIndex((seat) => seat.id === targetId);
        const to = next.findIndex((seat) => seat.id === targetId);
        if (to < 0) return current;
        next.splice(from < originalTo ? to + 1 : to, 0, moved);
      }
      return [...next, ...dedicated];
    });
    markDirty();
  }, [markDirty]);
  const toggleSeatEnabled = useCallback((id: string) => { setSeats((current) => current.map((seat) => seat.id === id ? { ...seat, enabled: !seat.enabled } : seat)); markDirty(); }, [markDirty]);
  const toggleSeatRace = useCallback((id: string) => { setSeats((current) => current.map((seat) => seat.id === id ? { ...seat, race: !seat.race } : seat)); markDirty(); }, [markDirty]);
  const setReviewSeat = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    setSeats((current) => current.map((item) => item.id === id && !item.roles.includes("review") ? { ...item, roles: [...item.roles, "review"] } : item));
    setReview((current) => ({ ...current, engine: id, enabled: true, max_concurrent: 1 }));
    markDirty();
    showFeedback(`${seat.label} 已设为 Review Worker`);
  }, [markDirty, seats, showFeedback]);

  const setVerifierSeat = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    setSeats((current) => current.map((item) => item.id === id && !item.roles.includes("verifier") ? { ...item, roles: [...item.roles, "verifier"] } : item));
    setVerifier((current) => ({ ...current, engine: id, enabled: true, max_concurrent: Math.max(0, current.max_concurrent ?? 0) }));
    markDirty();
    showFeedback(`${seat.label} 已设为 Verifier Worker`);
  }, [markDirty, seats, showFeedback]);

  const updateReview = useCallback((patch: Partial<ReviewPolicy>) => { setReview((current) => ({ ...current, ...patch, max_concurrent: 1 })); markDirty(); }, [markDirty]);
  const updateVerifier = useCallback((patch: Partial<VerifierPolicy>) => {
    setVerifier((current) => ({
      ...current,
      ...patch,
      max_concurrent: Math.max(0, patch.max_concurrent ?? current.max_concurrent ?? 0),
    }));
    markDirty();
  }, [markDirty]);
  const createDedicatedReview = useCallback(() => { const source = seats.find((seat) => seat.id === review.engine) || ordinarySeats[0]; if (!source) { showFeedback("请先添加一个 Worker"); return; } const next: Seat = { ...source, id: randomId("seat", engineOf(source.engine)), label: `${ENGINE_META[engineOf(source.engine)].label} Review`, roles: ["review"], race: false, capacity: { max_running: 1, max_review_running: 1 }, priority: seats.length * 10 + 10, enabled: true }; setSeats((current) => [...current, next]); setReview((current) => ({ ...current, engine: next.id, enabled: true, max_concurrent: 1 })); markDirty(); showFeedback("已创建独立 Review 配置"); }, [markDirty, ordinarySeats, review.engine, seats, showFeedback]);
  const createDedicatedVerifier = useCallback(() => {
    const source = seats.find((seat) => seat.id === verifier.engine) || ordinarySeats[0];
    if (!source) { showFeedback("请先添加一个 Worker"); return; }
    const next: Seat = {
      ...source,
      id: randomId("seat", engineOf(source.engine)),
      label: `${ENGINE_META[engineOf(source.engine)].label} Verifier`,
      roles: ["verifier"],
      race: false,
      capacity: { max_running: 1, max_review_running: 0 },
      priority: seats.length * 10 + 10,
      enabled: true,
    };
    setSeats((current) => [...current, next]);
    setVerifier((current) => ({ ...current, engine: next.id, enabled: true, max_concurrent: Math.max(0, current.max_concurrent ?? 0) }));
    markDirty();
    showFeedback("已创建独立 Verifier 配置");
  }, [markDirty, ordinarySeats, seats, showFeedback, verifier.engine]);

  const testSeat = useCallback(async (seat: Seat, quiet = false): Promise<WorkerModelTestResult> => {
    const credential = credentials.find((item) => item.id === seat.credential_id);
    const account = accounts.find((item) => item.account_id === credential?.secret_ref);
    const connection = credential?.kind === "system_inherit"
      ? "system"
      : account
        ? accountConnection(account)
        : credential?.kind === "custom_endpoint" ? "custom_endpoint" : "official";
    setTestingIds((current) => { const next = new Set(current); next.add(seat.id); return next; });
    setTestResults((current) => { const next = { ...current }; delete next[seat.id]; return next; });
    const accountId = credential?.kind === "system_inherit" ? "__system__" : credential?.secret_ref || "";
    try {
      const probed: WorkerModelTestResult = connection === "custom_endpoint" && !seat.model?.trim()
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
            id: seat.id,
            label: seat.label,
            engine: engineOf(seat.engine),
            accountId,
            connection,
            baseUrl: account ? accountBaseUrl(account) : credential?.endpoint?.base_url,
            model: seat.model || "",
            reasoningEffort: seat.reasoning_effort || "default",
          }), seat.model || "", backend);
      const systemLoginPresent = connection === "system"
        && backend === "local"
        && systemLogins[engineOf(seat.engine)] === "present";
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
          status: result.ok ? "ok" : "auth_failed",
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
  }, [accounts, backend, credentials, showFeedback, systemLogins]);

  const testAllSeats = useCallback(async () => {
    const targets = seats.filter((seat) => seat.enabled && (
      isOrdinarySeat(seat)
      || (review.enabled && review.engine === seat.id)
      || (verifier.enabled && verifier.engine === seat.id)
    ));
    if (!targets.length) { showFeedback("当前没有可检查的启用 Worker"); return; }
    setBatchCheck({ running: true, completed: 0, total: targets.length });
    if (backend === "container") {
      const requests = targets.map((seat) => {
        const credential = credentials.find((item) => item.id === seat.credential_id);
        const account = accounts.find((item) => item.account_id === credential?.secret_ref);
        const connection = credential?.kind === "system_inherit"
          ? "system"
          : account
            ? accountConnection(account)
            : credential?.kind === "custom_endpoint" ? "custom_endpoint" : "official";
        const accountId = credential?.kind === "system_inherit" ? "__system__" : credential?.secret_ref || "";
        const profile = buildModelTestProfile({
          id: seat.id,
          label: seat.label,
          engine: engineOf(seat.engine),
          accountId,
          connection,
          baseUrl: account ? accountBaseUrl(account) : credential?.endpoint?.base_url,
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
              status: result.ok ? "ok" : "auth_failed",
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
    const results = await Promise.all(targets.map(async (seat) => {
      const result = await testSeat(seat, true);
      setBatchCheck((current) => ({ ...current, completed: current.completed + 1 }));
      return result;
    }));
    const passed = results.filter((result) => result.ok).length;
    setBatchCheck({ running: false, completed: targets.length, total: targets.length });
    showFeedback(`一键检查完成：${passed}/${targets.length} 个 Worker 通过真实模型请求`);
  }, [accounts, backend, credentials, review.enabled, review.engine, verifier.enabled, verifier.engine, seats, showFeedback, testSeat]);

  const save = async () => {
    if (!config) return;
    const enabledOrdinary = seats.filter((seat) => isOrdinarySeat(seat) && seat.enabled);
    if (!enabledOrdinary.length) { setSaveState("error"); showFeedback("至少需要一个启用的普通 Worker"); return; }
    if (review.enabled && !seats.some((seat) => seat.id === review.engine && seat.enabled && canServeChannel(seat, "review"))) { setSaveState("error"); showFeedback("Review 已启用，但没有指定可用 Worker"); return; }
    if (verifier.enabled && !seats.some((seat) => seat.id === verifier.engine && seat.enabled && canServeChannel(seat, "verifier"))) { setSaveState("error"); showFeedback("Verifier 已启用，但没有指定可用 Worker"); return; }
    const invalidLlm = (["planner", "titler"] as const).find((which) => {
      const profile = llmProfiles[which];
      return !profile.model.trim() || ((profile.connection || (profile.base_url ? "custom_endpoint" : "default")) === "custom_endpoint" && !profile.base_url?.trim());
    });
    if (invalidLlm) { setSaveState("error"); showFeedback(`${invalidLlm === "planner" ? "Reason / Planner" : "Titler"} 缺少模型 ID 或自定义 Base URL`); return; }
    const invalidTemperature = (["planner", "titler"] as const).find((which) => {
      const profile = llmProfiles[which];
      if (llmTemperatureMode(profile) !== "custom") return false;
      const value = Number(profile.temperature);
      return !Number.isFinite(value) || value < 0 || value > 2;
    });
    if (invalidTemperature) { setSaveState("error"); showFeedback(`${invalidTemperature === "planner" ? "Reason / Planner" : "Titler"} 的 Temperature 需在 0 到 2 之间`); return; }
    setSaveState("saving");
    const normalizedSeats = seats.map((seat, index) => ({ ...seat, priority: (index + 1) * 10, roles: [...seat.roles], capacity: { ...seat.capacity } }));
    const refs = enabledOrdinary.map((seat) => seat.id);
    const raceRefs = enabledOrdinary.filter((seat) => seat.race).map((seat) => seat.id);
    const saved = await putWorkerSettings({
      seats: normalizedSeats,
      credentials,
      engines: refs,
      race_engines: raceRefs,
      start_workers: Math.min(Math.max(1, startWorkers), Math.max(1, maxWorkers)),
      max_workers: Math.max(1, maxWorkers),
      worker_backend: backend,
      worker_network: network,
      race_scout: raceScout,
      race_timeout: raceTimeout,
      wall_clock_budget: wallClock,
      max_total_workers: maxTotal,
      cost_budget_usd: costBudget,
      llm_profiles: llmProfiles,
      stage_policy: {
        ...config.stage_policy,
        race: { ...config.stage_policy.race, enabled: raceScout, timeout: raceTimeout, engines: raceRefs },
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
    if (!saved) { setSaveState("error"); showFeedback(backend === "container" ? "保存失败：请检查容器 Worker 是否都绑定了可注入凭据" : "Worker 配置保存失败"); return; }
    setConfig(saved);
    setSeats(normalizedSeats);
    setLlmProfiles(saved.llm_profiles);
    setDirty(false);
    setSaveState("saved");
    showFeedback("Worker 阵容、Review 与 Verifier 配置已保存");
    await refreshHealth();
  };

  const navItems: { id: SettingsSection; icon: IconName; label: string; sub: string }[] = [
    { id: "roster", icon: "grid", label: "出战配置", sub: `${ordinarySeats.filter((seat) => seat.enabled).length}/${ordinarySeats.length} Worker · Review ${review.enabled ? "开" : "关"} · Verifier ${verifier.enabled ? "开" : "关"}` },
    { id: "runtime", icon: "cpu", label: "运行环境", sub: backend === "local" ? "本地" : `容器 · ${network}` },
    { id: "system", icon: "refresh", label: "系统更新", sub: "版本与回滚" },
    { id: "scheduling", icon: "clock", label: "调度与预算", sub: `普通并发 ${maxWorkers}` },
    { id: "models", icon: "crosshair", label: "推理模型", sub: llmProfiles.planner.model },
    { id: "appearance", icon: "droplet", label: "外观配色", sub: "主题与配色引擎" },
  ];
  const titles: Record<SettingsSection, string> = { roster: "出战配置", runtime: "运行环境", scheduling: "调度与预算", models: "推理模型", appearance: "外观配色", system: "系统更新" };

  if (!config) return (
    <div className="wsettings-skeleton" role="status" aria-label="正在读取 Worker 配置">
      <div className="wsettings-skel-rail">
        <i className="skel sk-brand" />
        <i className="skel sk-label" />
        <i className="skel sk-row" /><i className="skel sk-row" /><i className="skel sk-row" /><i className="skel sk-row" />
      </div>
      <div className="wsettings-skel-main">
        <div className="wsettings-skel-top"><i className="skel sk-title" /><i className="skel sk-btn" /></div>
        <div className="wsettings-skel-grid"><i className="skel sk-card" /><i className="skel sk-card" /><i className="skel sk-card" /><i className="skel sk-card" /></div>
        <span className="wsettings-skel-note">正在读取 Worker 配置…</span>
      </div>
    </div>
  );

  return (
    <div className="wsettings-page">
      <aside className="wsettings-nav">
        <a className="wsettings-brand" href={returnTo}><strong>無敵</strong><span>Muteki</span></a>
        <div className="wsettings-nav-title"><span>设置</span><strong>Worker 设置</strong></div>
        <nav aria-label="设置导航">{navItems.map((item) => <button key={item.id} type="button" className={section === item.id ? "on" : ""} onClick={() => setSection(item.id)}><Icon name={item.icon} size={16} /><span><strong>{item.label}</strong><small>{item.sub}</small></span><Icon name="chevronRight" size={13} /></button>)}</nav>
        <div className="wsettings-nav-foot"><span><i />配置中心</span><a href={returnTo}><Icon name="chevronRight" size={13} />返回工作台</a></div>
      </aside>

      <main className="wsettings-main">
        <header className="wsettings-topbar"><div><span>设置 / {titles[section]}</span><h1>{titles[section]}</h1></div>{section === "system" ? <span className="wsettings-draft">稳定通道</span> : section === "appearance" ? <span className="wsettings-draft">即时生效 · 保存在本浏览器</span> : <><span className={`wsettings-draft${dirty ? " dirty" : ""}`}>{dirty ? "有未保存更改" : "当前生效配置"}</span><div className="wsettings-top-actions"><button type="button" className="primary" onClick={save} disabled={!dirty || saveState === "saving"}><Icon name="check" size={14} />{saveState === "saving" ? "保存中…" : "保存配置"}</button></div></>}</header>

        <div className="wsettings-content">
          {section === "roster" ? <div className="wsettings-orchestration"><RosterWorkspace seats={seats} credentials={credentials} backend={backend} testingIds={testingIds} testResults={testResults} batchCheck={batchCheck} selectedId={inspector === "seat" ? selectedId : null} review={review} verifier={verifier} onSelect={(id) => { setSelectedId(id); setInspector("seat"); }} onSelectReview={() => setInspector("review")} onSelectVerifier={() => setInspector("verifier")} onAdd={addSeat} onReorder={reorderSeats} onDuplicate={duplicateSeat} onToggleEnabled={toggleSeatEnabled} onToggleRace={toggleSeatRace} onSetReview={setReviewSeat} onSetVerifier={setVerifierSeat} onTestAll={() => void testAllSeats()} onTest={(seat) => void testSeat(seat)} onDelete={deleteSeat} />{inspector === "review" ? <ReviewInspector seats={seats} credentials={credentials} accounts={accounts} models={models} backend={backend} review={review} onReview={updateReview} onCreateDedicated={createDedicatedReview} onSeatUpdate={updateSeat} onSeatEngine={changeEngine} onSeatAccount={bindAccount} onAccountsChanged={refreshAccounts} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onEditOrdinary={(id) => { setSelectedId(id); setInspector("seat"); }} onTest={() => { const seat = seats.find((item) => item.id === review.engine); if (seat) void testSeat({ ...seat, reasoning_effort: review.reasoning_effort && review.reasoning_effort !== "inherit" ? review.reasoning_effort : seat.reasoning_effort || "default" }); }} testing={testingIds.has(review.engine || "")} testResult={review.engine ? testResults[review.engine] || null : null} /> : inspector === "verifier" ? <VerifierInspector seats={seats} credentials={credentials} accounts={accounts} models={models} backend={backend} verifier={verifier} onVerifier={updateVerifier} onCreateDedicated={createDedicatedVerifier} onSeatUpdate={updateSeat} onSeatEngine={changeEngine} onSeatAccount={bindAccount} onAccountsChanged={refreshAccounts} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onEditOrdinary={(id) => { setSelectedId(id); setInspector("seat"); }} onTest={() => { const seat = seats.find((item) => item.id === verifier.engine); if (seat) void testSeat({ ...seat, reasoning_effort: verifier.reasoning_effort && verifier.reasoning_effort !== "inherit" ? verifier.reasoning_effort : seat.reasoning_effort || "default" }); }} testing={testingIds.has(verifier.engine || "")} testResult={verifier.engine ? testResults[verifier.engine] || null : null} /> : <SeatInspector seat={selectedSeat && isOrdinarySeat(selectedSeat) ? selectedSeat : null} credentials={credentials} accounts={accounts} models={models} backend={backend} health={selectedSeat ? health[selectedSeat.id] : undefined} testing={selectedSeat ? testingIds.has(selectedSeat.id) : false} testResult={selectedSeat ? testResults[selectedSeat.id] || null : null} onUpdate={(patch) => selectedSeat && updateSeat(selectedSeat.id, patch)} onEngine={(engine) => selectedSeat && changeEngine(selectedSeat.id, engine)} onAccount={(key, account) => selectedSeat && bindAccount(selectedSeat.id, key, account)} onAccountsChanged={refreshAccounts} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onDuplicate={() => selectedSeat && duplicateSeat(selectedSeat.id)} onDelete={() => selectedSeat && deleteSeat(selectedSeat.id)} onTest={() => selectedSeat && void testSeat(selectedSeat)} />}</div>
            : section === "runtime" ? <RuntimeWorkspace backend={backend} network={network} seatCount={seats.length} imageStatus={imageStatus} imageLoading={imageLoading} pulling={pullingImage} onBackend={(next) => { setBackend(next); markDirty(); if (next === "container" && !imageStatus) void refreshImage(); }} onNetwork={(next) => { setNetwork(next); markDirty(); }} onRefreshImage={() => void refreshImage()} onPullImage={() => void pullImage()} />
                : section === "scheduling" ? <SchedulingWorkspace config={config} raceScout={raceScout} raceTimeout={raceTimeout} startWorkers={startWorkers} maxTotal={maxTotal} wallClock={wallClock} costBudget={costBudget} maxWorkers={maxWorkers} onChange={(patch) => { if (patch.raceScout !== undefined) setRaceScout(patch.raceScout); if (patch.raceTimeout !== undefined) setRaceTimeout(patch.raceTimeout); if (patch.startWorkers !== undefined) setStartWorkers(patch.startWorkers); if (patch.maxTotal !== undefined) setMaxTotal(patch.maxTotal); if (patch.wallClock !== undefined) setWallClock(patch.wallClock); if (patch.costBudget !== undefined) setCostBudget(patch.costBudget); markDirty(); }} />
                  : section === "models" ? <ModelsWorkspace value={llmProfiles} onChange={(next) => { setLlmProfiles(next); markDirty(); }} />
                    : section === "appearance" ? <AppearanceWorkspace />
                      : <PlatformUpdate />}
        </div>
        {feedback ? <div className="wsettings-feedback"><Icon name={saveState === "error" ? "alert" : "check"} size={14} />{feedback}</div> : null}
      </main>
    </div>
  );
}
