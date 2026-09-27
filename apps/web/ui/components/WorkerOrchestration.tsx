"use client";

import { MotionPreferences } from "@/components/MotionPreferences";
import dynamic from "next/dynamic";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { Alert, Card, Chip, ComboBox, Input, Button, Dropdown, Label, ListBox, ListBoxItem, Radio, RadioGroup, Select, Skeleton, Slider, Switch } from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";
import { EngineLogo } from "@/components/EngineLogo";
import { MutekiLogo } from "@/components/MutekiLogo";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { NumberField } from "@/components/NumberField";
import { PlatformUpdate } from "@/components/PlatformUpdate";
import {
  type GlobalCredential,
  type ModelEndpoint,
  type ProfileHealth,
  type WorkerModelTestResult,
  type WorkerModelOptions,
  type WorkerImageStatus,
  type OpenVpnStatus,
  type LlmProfile,
  type LlmTemperatureMode,
  type WorkerSettings,
  checkAuth,
  createModelEndpoint,
  fetchProfilesHealth,
  getGlobalCredentials,
  getModelEndpoints,
  getWorkerModelOptions,
  getWorkerModelTestResults,
  getWorkerImageStatus,
  getOpenVpnStatus,
  getWorkerSettings,
  putWorkerSettings,
  uploadOpenVpnConfig,
  pullWorkerImage,
  refreshCredentialModels,
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
import { useLang, useT } from "@/lib/i18n";
import { ConversationReadingPrefsPanel } from "./conversation/ConversationReadingPrefsPanel";
import { useSolveOnlyMode, setSolveOnlyMode } from "@/lib/workspaceMode";

const TaskCredentialManager = dynamic(
  () => import("@/components/ProviderManager").then((module) => module.ProviderManager),
  { ssr: false },
);

type Seat = NonNullable<WorkerSettings["seats"]>[number];
type Credential = NonNullable<WorkerSettings["credentials"]>[number];
type ReviewPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["review"]>;
type VerifierPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["verifier"]>;
type SettingsSection = "roster" | "credentials" | "runtime" | "scheduling" | "models" | "system";
const WORKER_SECTIONS: SettingsSection[] = ["roster", "credentials", "runtime", "system", "scheduling", "models"];
function isWorkerSection(value: string): value is SettingsSection {
  return WORKER_SECTIONS.includes(value as SettingsSection);
}
type Engine = "claude" | "codex" | "cursor" | "pi" | "omp" | "kimi" | "grok" | "opencode" | "devin";
type SaveState = "idle" | "saving" | "saved" | "error";
type AccountConnection = "official" | "custom_endpoint";
type LlmProfileName = "planner" | "titler";
type WorkerDraftSnapshot = {
  seats: Seat[];
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  backend: WorkerSettings["worker_backend"];
  network: WorkerSettings["worker_network"];
  containerScope: WorkerSettings["worker_container_scope"];
  workerPrivilege: "default" | "elevated";
  vpnEnabled: boolean;
  raceTimeout: number;
  dispatchMode: "fixed" | "auto";
  startWorkers: number;
  maxTotal: number;
  wallClock: number;
  costBudget: number;
  llmProfiles: WorkerSettings["llm_profiles"];
};

const DEFAULT_LLM_PROFILES: WorkerSettings["llm_profiles"] = {
  planner: { provider: "deepseek", model: "deepseek-v4-pro", base_url: "", connection: "default", temperature_mode: "omit" },
  titler: { provider: "deepseek", model: "deepseek-v4-flash", base_url: "", connection: "default", temperature_mode: "omit" },
};
const WORKER_IMAGE_ENV = "MUTEKI_WORKER_IMAGE";

function workerDraftSignature(snapshot: WorkerDraftSnapshot): string {
  return JSON.stringify(snapshot);
}

function llmTemperatureMode(profile: LlmProfile): LlmTemperatureMode {
  const mode = profile.temperature_mode;
  if (mode === "custom" || mode === "omit" || mode === "default") return mode;
  return "default";
}
type ModelDiscoveryOutcome = { ok: boolean; detail: string };
type BatchCheckState = { running: boolean; completed: number; total: number };

const ENGINES: Engine[] = ["pi", "claude", "codex", "cursor", "omp", "opencode", "kimi", "grok", "devin"];
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
  devin: { label: "Devin CLI", wireApi: "", protocol: "Devin CLI", transport: "devin_cli", localOnly: true },
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
  devin: [],
};

/** CTF + auto dispatch hard-caps ordinary concurrency in Swarm.__init__. */
const CTF_AUTO_CONCURRENCY = 3;

const DEFAULT_REVIEW: ReviewPolicy = {
  enabled: true,
  engine: "",
  timeout: 90,
  after_race: false,
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

function globalCredentialKey(credential: GlobalCredential): string {
  return credential.source === "system"
    ? "__system__"
    : credential.account_id || credential.id.replace(/^account:/, "");
}

const UNUSABLE_GLOBAL_CREDENTIAL_STATUSES = new Set([
  "missing", "absent", "failed", "invalid", "error", "unavailable",
]);

function globalCredentialUsable(
  credential: GlobalCredential | null | undefined,
): credential is GlobalCredential {
  return Boolean(
    credential?.present
    && !UNUSABLE_GLOBAL_CREDENTIAL_STATUSES.has(
      String(credential.status || "").toLowerCase(),
    ),
  );
}

function globalCredentialForLegacy(
  credential: Credential | undefined,
  availableCredentials: GlobalCredential[],
): GlobalCredential | null {
  if (!credential) return null;
  const key = credentialKey(credential);
  return availableCredentials.find((item) => (
    globalCredentialKey(item) === key
    && (key !== "__system__" || item.engine === credential.engine)
  )) || null;
}

function syncCredentialFromGlobal(credential: Credential, availableCredentials: GlobalCredential[]): Credential {
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
      ? { base_url: selected.base_url || "", wire_api: ENGINE_META[engine].wireApi }
      : undefined,
  };
}

function syncCredentialsFromGlobal(credentials: Credential[], availableCredentials: GlobalCredential[]): Credential[] {
  return credentials.map((credential) => syncCredentialFromGlobal(credential, availableCredentials));
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
  const testedAt = result?.tested_at
    ? new Date(result.tested_at * 1000).toLocaleString("zh-CN", {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      })
    : "";
  const detail = testing
    ? "正在发起真实模型请求"
    : result
      ? [result.detail || (result.ok ? "真实模型请求完成" : "真实模型请求失败"), elapsed, testedAt ? `检查于 ${testedAt}` : ""].filter(Boolean).join(" · ")
      : enabled ? "尚未发起真实模型请求" : "停用的 Worker 不参与检查";
  return (
    <span className={`wroster-self-check ${state}${compact ? " compact" : ""}`} data-tooltip={detail} aria-live="polite">
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
    auth: custom || ["cursor", "pi", "omp", "opencode"].includes(engine)
      ? "api_key" : "subscription",
    credential_mode: custom || ["cursor", "pi", "omp", "opencode"].includes(engine)
      ? "api_key" : "subscription",
    credential_account: accountId === "__system__" ? "" : accountId,
    credential_id: accountId === "__system__" ? `system:${engine}` : `account:${accountId}`,
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
  return (
    <div className="wset-effort-control">
      <label>
        <span>{inherit ? "覆盖推理强度" : "推理强度"}</span>
        <Select aria-label={inherit ? "覆盖推理强度" : "推理强度"} selectedKey={selected} onSelectionChange={(key) => onChange(String(key))}>
          <Select.Trigger><Select.Value /></Select.Trigger>
          <Select.Popover><ListBox>{inherit ? <ListBoxItem id="inherit">继承 Worker 设置</ListBoxItem> : <ListBoxItem id="default">{defaultLabel}</ListBoxItem>}{definition.levels.map((level) => <ListBoxItem key={level} id={level}>{EFFORT_LABELS[level] || level}</ListBoxItem>)}</ListBox></Select.Popover>
        </Select>
      </label>
      {!definition.supported ? <small>此模型使用默认推理强度</small> : null}
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
      onClick={onSelect}
      onContextMenu={(event) => {
        event.preventDefault();
        onOpenMenu({ x: event.clientX, y: event.clientY });
      }}
      onDragStart={onDragStart}
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDrop={onDrop}
      onDragEnd={onDragEnd}
    >
      <div className="wroster-card-head">
        <span className="wroster-order" title="拖动调整优先级">{String(order).padStart(2, "0")}</span>
        <span className="wroster-engine"><EngineLogo engine={engine} size={18} data-tooltip={ENGINE_META[engine].label} /></span>
        <Button
          type="button"
          className="wroster-title"
          aria-pressed={selected}
          aria-label={`编辑 ${seat.label}`}
          onClick={(event) => {
            event.stopPropagation();
            onSelect();
          }}
          onContextMenu={(event) => {
            event.preventDefault();
            event.stopPropagation();
            onOpenMenu({ x: event.clientX, y: event.clientY });
          }}
        ><strong>{seat.label}</strong><small>{ENGINE_META[engine].label}</small></Button>
        <span
          className="wroster-card-quick-toggle"
          data-tooltip={seat.enabled ? `停用 ${seat.label}` : `启用 ${seat.label}`}
        >
          <Switch
            size="sm"
            isSelected={seat.enabled}
            onChange={onToggleEnabled}
            aria-label={seat.enabled ? `停用 ${seat.label}` : `启用 ${seat.label}`}
            onPointerDown={(event) => event.stopPropagation()}
            onClick={(event) => event.stopPropagation()}
          ><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch>
        </span>
        <Button type="button" isIconOnly variant="ghost" className="wroster-card-menu" aria-label={`${seat.label} 更多操作`} onClick={(event) => {
          event.stopPropagation();
          const rect = event.currentTarget.getBoundingClientRect();
          onOpenMenu({ x: rect.left, y: rect.bottom });
        }}><Icon name="more" size={15} /></Button>
      </div>
      <dl className="wroster-meta">
        <div><dt>连接</dt><dd>{account}</dd></div>
        <div><dt>模型</dt><dd>{seat.model || "Worker 默认模型"} · {effortSummary(seat.reasoning_effort)}</dd></div>
      </dl>
      <div className="wroster-card-foot">
        <SelfCheckStatus enabled={seat.enabled} testing={testing} result={testResult} compact />
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
  selectedInspector,
  review,
  verifier,
  onSelect,
  onSelectReview,
  onSelectVerifier,
  onAdd,
  onReorder,
  onDuplicate,
  onToggleEnabled,
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
  selectedInspector: "seat" | "review" | "verifier";
  review: ReviewPolicy;
  verifier: VerifierPolicy;
  onSelect: (id: string) => void;
  onSelectReview: () => void;
  onSelectVerifier: () => void;
  onAdd: (engine: Engine) => void;
  onReorder: (source: string, target: string | null) => void;
  onDuplicate: (id: string) => void;
  onToggleEnabled: (id: string) => void;
  onSetReview: (id: string) => void;
  onSetVerifier: (id: string) => void;
  onTestAll: () => void;
  onTest: (seat: Seat) => void;
  onDelete: (id: string) => void;
}) {
  const ordinary = seats.filter(isOrdinarySeat);
  const reviewSeat = seats.find((seat) => seat.id === review.engine);
  const verifierSeat = seats.find((seat) => seat.id === verifier.engine);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const [dragOverId, setDragOverId] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<{ seatId: string; x: number; y: number } | null>(null);
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

  const openContextMenu = (seat: Seat, point: { x: number; y: number }) => {
    const width = 232;
    const height = 318;
    const x = Math.max(8, Math.min(point.x, window.innerWidth - width - 8));
    const y = Math.max(8, Math.min(point.y, window.innerHeight - height - 8));
    onSelect(seat.id);
    setContextMenu({ seatId: seat.id, x, y });
  };

  return (
    <section className="wroster-workspace">
      <header className="wroster-head">
        <div className="wsettings-section-head wroster-head-main">
          <div className="wsettings-section-copy"><h2>Worker <Chip size="sm" variant="soft">{ordinary.length}</Chip></h2><p>{enabledOrdinary.length} 个启用 · 并发合计 {maxWorkers}</p></div>
          <Button type="button" className={`wroster-check-all${batchCheck.running ? " running" : ""}`} isDisabled={!enabledOrdinary.length || batchCheck.running || testingIds.size > 0} onClick={onTestAll} data-tooltip="向所有启用的出战 Worker 发起真实模型请求"><Icon name="refresh" size={13} />{batchCheck.running ? `正在检查 ${batchCheck.completed}/${batchCheck.total}` : "一键检查"}</Button>
        </div>
        <div className="wroster-addbar">
          <Dropdown>
            <Dropdown.Trigger className="wroster-add-trigger"><Icon name="plus" size={14} /><span>添加 Worker</span><Icon name="chevronDown" size={12} /></Dropdown.Trigger>
            <Dropdown.Popover placement="bottom start" className="wroster-add-menu">
              <Dropdown.Menu aria-label="添加 Worker 引擎" onAction={(key) => onAdd(key as Engine)}>
                {ENGINES.map((engine) => {
                  const meta = ENGINE_META[engine];
                  const unavailable = Boolean(meta.localOnly && backend !== "local");
                  return <Dropdown.Item key={engine} id={engine} textValue={meta.label} isDisabled={unavailable}><span className="wroster-add-logo"><EngineLogo engine={engine} size={17} data-tooltip={meta.label} /></span><span className="wroster-add-copy"><strong>{meta.label}</strong><small>{unavailable ? "当前仅支持本地运行" : meta.localOnly ? "仅支持本地运行" : meta.protocol}</small></span><Icon name="plus" size={14} /></Dropdown.Item>;
                })}
              </Dropdown.Menu>
            </Dropdown.Popover>
          </Dropdown>
          <span className="wroster-check-summary" aria-live="polite">{checkSummary}</span>
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

      <div className="wroster-channel-label">辅助 Worker <span>Pentest</span></div>
      <section className={`wreview-slot${review.enabled ? "" : " disabled"}${selectedInspector === "review" ? " selected" : ""}`}>
        <Button type="button" variant="ghost" className="wreview-selection" aria-label="配置 Review Worker" aria-pressed={selectedInspector === "review"} onClick={onSelectReview}>
          <span className="wreview-slot-copy"><span className="wreview-symbol"><Icon name="eye" size={17} /></span><span><strong>Review Worker</strong><small>审查建议 · BTW 默认 Worker</small></span><Icon name="chevronRight" size={14} /></span>
          <span className="wreview-assigned">{reviewSeat?.label || "尚未指定 Worker"}</span>
          <span className="wreview-slot-state">{reviewSeat ? <SelfCheckStatus enabled={Boolean(review.enabled && reviewSeat.enabled)} testing={testingIds.has(reviewSeat.id)} result={testResults[reviewSeat.id]} compact /> : <span>待配置</span>}<span>并发 1</span></span>
        </Button>
      </section>

      <section className={`wreview-slot wverifier-slot${verifier.enabled ? "" : " disabled"}${selectedInspector === "verifier" ? " selected" : ""}`}>
        <Button type="button" variant="ghost" className="wreview-selection" aria-label="配置 Verifier Worker" aria-pressed={selectedInspector === "verifier"} onClick={onSelectVerifier}>
          <span className="wreview-slot-copy"><span className="wreview-symbol"><Icon name="check" size={17} /></span><span><strong>Verifier Worker</strong><small>独立复现报告</small></span><Icon name="chevronRight" size={14} /></span>
          <span className="wreview-assigned">{verifierSeat?.label || "尚未指定 Worker"}</span>
          <span className="wreview-slot-state">{verifierSeat ? <SelfCheckStatus enabled={Boolean(verifier.enabled && verifierSeat.enabled)} testing={testingIds.has(verifierSeat.id)} result={testResults[verifierSeat.id]} compact /> : <span>待配置</span>}<span>并发 {verifier.max_concurrent && verifier.max_concurrent > 0 ? verifier.max_concurrent : "按报告"}</span></span>
        </Button>
      </section>

      <footer className="wroster-note"><Icon name="rows" size={13} />拖动 Worker 调整优先级</footer>

      {contextMenu && menuSeat ? (
        <Dropdown isOpen onOpenChange={(open) => { if (!open) setContextMenu(null); }}>
          <Dropdown.Trigger aria-label={`${menuSeat.label} 快捷操作`} className="fixed z-50 h-px w-px opacity-0" style={{ left: contextMenu.x, top: contextMenu.y }} />
          <Dropdown.Popover placement="bottom start" className="wroster-context-menu">
            <Dropdown.Menu aria-label={`${menuSeat.label} 快捷操作`} onAction={(key) => {
              const actions: Record<string, () => void> = {
                edit: () => onSelect(menuSeat.id), duplicate: () => onDuplicate(menuSeat.id), test: () => onTest(menuSeat),
                enabled: () => onToggleEnabled(menuSeat.id), review: () => onSetReview(menuSeat.id),
                verifier: () => onSetVerifier(menuSeat.id), delete: () => onDelete(menuSeat.id),
              };
              setContextMenu(null);
              actions[String(key)]?.();
            }}>
              <Dropdown.Item id="edit" textValue="编辑配置"><Icon name="pencil" size={14} />编辑配置</Dropdown.Item>
              <Dropdown.Item id="duplicate" textValue="复制 Worker"><Icon name="copy" size={14} />复制 Worker</Dropdown.Item>
              <Dropdown.Item id="test" textValue="单独自检" isDisabled={testingIds.has(menuSeat.id)}><Icon name="plug" size={14} />{testingIds.has(menuSeat.id) ? "正在自检" : "单独自检"}</Dropdown.Item>
              <Dropdown.Item id="enabled" textValue={menuSeat.enabled ? "停用 Worker" : "启用 Worker"}><Icon name={menuSeat.enabled ? "pause" : "play"} size={14} />{menuSeat.enabled ? "停用 Worker" : "启用 Worker"}</Dropdown.Item>
              <Dropdown.Item id="review" textValue="设为 Review Worker"><Icon name="eye" size={14} />设为 Review Worker{review.engine === menuSeat.id ? <Icon name="check" size={13} /> : null}</Dropdown.Item>
              <Dropdown.Item id="verifier" textValue="设为 Verifier Worker"><Icon name="check" size={14} />设为 Verifier Worker{verifier.engine === menuSeat.id ? <Icon name="check" size={13} /> : null}</Dropdown.Item>
              <Dropdown.Item id="delete" textValue="移除配置" className="danger"><Icon name="x" size={14} />移除配置</Dropdown.Item>
            </Dropdown.Menu>
          </Dropdown.Popover>
        </Dropdown>
      ) : null}
    </section>
  );
}

function CredentialBindingEditor({
  engine,
  accountKey,
  model,
  availableCredentials,
  backend,
  onBind,
  onModelChange,
  onDiscoverModels,
  discoveringModels,
}: {
  engine: Engine;
  accountKey: string;
  model: string;
  availableCredentials: GlobalCredential[];
  backend: WorkerSettings["worker_backend"];
  onBind: (key: string, credential?: GlobalCredential) => void;
  onModelChange: (model: string) => void;
  onDiscoverModels: (credentialId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
}) {
  const [discoveryResult, setDiscoveryResult] = useState<ModelDiscoveryOutcome | null>(null);
  const matchingCredentials = availableCredentials.filter((item) => (
    item.engine === engine && (item.source === "stored" || backend === "local")
  ));
  const selectedCredential = availableCredentials.find((item) => (
    item.engine === engine && (
      (accountKey === "__system__" && item.source === "system")
      || (item.source === "stored" && item.account_id === accountKey)
    )
  )) || null;
  const selectableCredentials = matchingCredentials.filter(globalCredentialUsable);
  const modelDirectory = useMemo(() => {
    const byId = new Map<string, { id: string; label: string }>();
    byId.set("", { id: "", label: "Runtime 默认模型" });
    if (selectedCredential?.default_model) {
      byId.set(selectedCredential.default_model, { id: selectedCredential.default_model, label: "凭据默认模型" });
    }
    for (const id of [
      ...(selectedCredential?.candidate_models || []),
      ...(selectedCredential?.models || []),
    ]) {
      if (!byId.has(id)) byId.set(id, { id, label: "端点目录模型" });
    }
    if (model && !byId.has(model)) byId.set(model, { id: model, label: "当前已保存模型" });
    return [...byId.values()];
  }, [model, selectedCredential?.candidate_models, selectedCredential?.default_model, selectedCredential?.models]);

  const chooseCredential = (credentialId: string) => {
    const next = availableCredentials.find((item) => item.id === credentialId);
    if (!globalCredentialUsable(next)) {
      onBind("");
      onModelChange("");
      return;
    }
    const nextModels = new Set(["", next.default_model || "", ...(next.models || [])]);
    if (!nextModels.has(model)) onModelChange(next.default_model || "");
    if (next.source === "system") {
      onBind("__system__");
      return;
    }
    const accountId = next.account_id || next.id.replace(/^account:/, "");
    onBind(accountId, next);
  };
  const refreshModels = async () => {
    if (!globalCredentialUsable(selectedCredential)) return;
    setDiscoveryResult(null);
    setDiscoveryResult(await onDiscoverModels(selectedCredential.id, engine));
  };
  const canDiscoverModels = ENGINE_META[engine].modelDiscovery !== false;
  const credentialCenterHref = "/task/workers?section=credentials";

  return (
    <div className="wbinding-editor">
      <label><span>Worker 凭据</span><Select aria-label="Worker 凭据" selectedKey={selectedCredential?.id || ""} onSelectionChange={(key) => chooseCredential(String(key))}>
        <Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="">选择已有凭据</ListBoxItem>{matchingCredentials.map((item) => <ListBoxItem key={item.id} id={item.id} isDisabled={!globalCredentialUsable(item)}>{item.label || item.id} · {item.source === "system" ? "宿主登录" : item.connection === "custom_endpoint" ? "自定义端点" : "官方账号"}{globalCredentialUsable(item) ? "" : " · 不可用"}</ListBoxItem>)}</ListBox></Select.Popover>
      </Select></label>

      {selectedCredential ? <div className={`wbinding-summary${selectedCredential.source === "system" ? " system" : ""}`}>
        <div><span>服务地址</span><strong>{selectedCredential.base_url || "默认服务地址"}</strong><small>{ENGINE_META[engine].protocol}</small></div>
        <div><span>状态</span><strong>{globalCredentialUsable(selectedCredential) ? "可用" : "不可用"}</strong></div>
      </div> : <div className="wbinding-missing">
        <Icon name="lock" size={14} />
        <span><strong>{selectableCredentials.length ? "请选择一个凭据" : `尚无可用于 ${ENGINE_META[engine].label} 的凭据`}</strong><small>可在本页的 Agent 凭据分区维护 Token、Key、Base URL 和宿主登录。</small></span>
        <a href={credentialCenterHref}>设置凭据</a>
      </div>}

      <div className="wbinding-model-section">
        <div className="wbinding-form-group-title model">
          <span>Worker 模型</span>
          <div className="wbinding-model-tools">
            {!selectedCredential ? <small>绑定凭据后选择模型</small> : null}
            <Button size="sm" variant="ghost" className={discoveringModels ? "loading" : !canDiscoverModels ? "unsupported" : ""} isDisabled={discoveringModels || !canDiscoverModels || !globalCredentialUsable(selectedCredential)} onPress={refreshModels} aria-label={canDiscoverModels ? `从 ${ENGINE_META[engine].label} CLI 读取可用模型` : `${ENGINE_META[engine].label} CLI 不提供模型列表命令`}><Icon name="refresh" size={11} />{discoveringModels ? "刷新中…" : canDiscoverModels ? "刷新模型" : "不支持刷新"}</Button>
          </div>
        </div>
        <div className="wbinding-model-field">
          <ComboBox
            key={`${engine}:${accountKey}`}
            aria-label="模型"
            fullWidth
            selectedKey={model || "__runtime_default__"}
            onSelectionChange={(key) => {
              if (key !== null) onModelChange(key === "__runtime_default__" ? "" : String(key));
            }}
            isDisabled={!globalCredentialUsable(selectedCredential)}
            defaultItems={modelDirectory}
            menuTrigger="focus"
          >
            <Label>模型</Label>
            <ComboBox.InputGroup>
              <Input placeholder="搜索或选择模型" title={model || "Runtime 默认模型"} />
              <ComboBox.Trigger aria-label="展开模型列表" />
            </ComboBox.InputGroup>
            <ComboBox.Popover className="wbinding-model-popover">
              <ListBox renderEmptyState={() => "没有匹配的模型"}>
                {(item: { id: string; label: string }) => (
                  <ListBoxItem id={item.id || "__runtime_default__"} textValue={item.id || item.label}>
                    <span className="wbinding-model-option"><strong>{item.id || item.label}</strong>{item.id && <small>{item.label}</small>}</span>
                    <ListBoxItem.Indicator />
                  </ListBoxItem>
                )}
              </ListBox>
            </ComboBox.Popover>
          </ComboBox>
        </div>
        {discoveryResult ? <p className={`wbinding-discovery-result ${discoveryResult.ok ? "ok" : "failed"}`}><Icon name={discoveryResult.ok ? "check" : "alert"} size={11} />{discoveryResult.detail}</p> : null}
      </div>
      {selectedCredential ? <div className="wbinding-center-link"><span>账户与服务地址</span><a href={credentialCenterHref}>管理凭据<Icon name="arrowUpRight" size={12} /></a></div> : null}
    </div>
  );
}

function SeatInspector({
  seat,
  credentials,
  availableCredentials,
  models,
  backend,
  health,
  testing,
  testResult,
  onUpdate,
  onEngine,
  onAccount,
  onDiscoverModels,
  discoveringModels,
  onDuplicate,
  onDelete,
  onTest,
}: {
  seat: Seat | null;
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  health?: ProfileHealth;
  testing: boolean;
  testResult: WorkerModelTestResult | null;
  onUpdate: (patch: Partial<Seat>) => void;
  onEngine: (engine: Engine) => void;
  onAccount: (key: string, credential?: GlobalCredential) => void;
  onDiscoverModels: (profileId: string, engine: Engine) => Promise<ModelDiscoveryOutcome>;
  discoveringModels: boolean;
  onDuplicate: () => void;
  onDelete: () => void;
  onTest: () => void;
}) {
  if (!seat) return <aside className="wset-inspector empty"><Icon name="grid" size={24} /><strong>选择一个 Worker</strong><span>添加或选择 Worker 以编辑配置</span></aside>;
  const engine = engineOf(seat.engine);
  const credential = credentials.find((item) => item.id === seat.credential_id);
  const accountKey = credentialKey(credential);
  const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
  const testBlocker = !globalCredentialUsable(selectedGlobalCredential)
    ? "请先选择可用凭据"
    : selectedGlobalCredential.connection === "custom_endpoint" && !seat.model?.trim()
      ? "请先选择模型"
      : "";
  const modelOptions = models.models[engine] || [];
  return (
    <aside className="wset-inspector seat">
      <header className="wset-inspector-head">
        <span>Worker 配置</span>
        <strong>{seat.label}</strong>
        <p>{ENGINE_META[engine].label} · {accountKey === "__system__" ? "系统登录" : accountKey || "未绑定"} · {seat.model || "默认模型"} · {effortSummary(seat.reasoning_effort, effortDefinition(engine, seat.model || "", modelOptions).defaultLevel)}</p>
      </header>
      <div className="wset-inspector-scroll">
        <section className="wset-form-section">
          <h3>基本信息</h3>
          <label><span>名称</span><Input value={seat.label} onChange={(event) => onUpdate({ label: event.target.value })} /></label>
          <label><span>Worker 程序</span><Select aria-label="Worker 程序" selectedKey={engine} onSelectionChange={(key) => onEngine(engineOf(String(key)))}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox>{ENGINES.map((item) => <ListBoxItem key={item} id={item} isDisabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</ListBoxItem>)}</ListBox></Select.Popover></Select></label>
        </section>

        <section className="wset-form-section">
          <h3>模型与连接</h3>
          <CredentialBindingEditor engine={engine} accountKey={accountKey} model={seat.model || ""} availableCredentials={availableCredentials} backend={backend} onBind={onAccount} onModelChange={(model) => { if (model !== (seat.model || "")) onUpdate({ model, reasoning_effort: normalizeEffortForModel(seat.reasoning_effort, engine, model, modelOptions) }); }} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={engine} model={seat.model || ""} options={modelOptions} value={seat.reasoning_effort || "default"} onChange={(reasoning_effort) => onUpdate({ reasoning_effort })} />
        </section>

        <section className="wset-form-section">
          <h3>运行设置</h3>
          <label><span>并发上限</span><NumberField min={1} max={32} value={seat.capacity.max_running} onChange={(next) => onUpdate({ capacity: { ...seat.capacity, max_running: Math.max(1, Number(next) || 1) } })} /></label>
          <div className="wset-switch-row"><span><b>启用 Worker</b><small>停用后保留连接和模型配置</small></span><Switch size="sm" aria-label="启用 Worker" isSelected={seat.enabled} onChange={(enabled) => onUpdate({ enabled })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
        </section>

        <section className="wset-test-summary">
          <div><span className={`wroster-health ${health?.status === "ok" ? "ok" : health?.status === "auth_failed" || health?.status === "blocked" ? "bad" : "pending"}`}><i />{healthLabel(health)}</span><p>{health?.status === "ok" ? "连接配置可用" : health?.detail || "可进行模型测试"}</p></div>
        </section>
      </div>

      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}

      <footer className="wset-inspector-dock">
        <Button type="button" className="primary" onClick={onTest} isDisabled={testing || Boolean(testBlocker)}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : testBlocker || "真实模型测试"}</Button>
        <div><Button type="button" onClick={onDuplicate}><Icon name="copy" size={13} />复制</Button><Button type="button" onClick={() => onUpdate({ enabled: !seat.enabled })}><Icon name={seat.enabled ? "pause" : "play"} size={13} />{seat.enabled ? "停用" : "启用"}</Button><Button type="button" className="danger" onClick={onDelete}><Icon name="x" size={13} />删除</Button></div>
      </footer>
    </aside>
  );
}

function ReviewInspector({
  seats,
  credentials,
  availableCredentials,
  models,
  backend,
  review,
  onReview,
  onCreateDedicated,
  onSeatUpdate,
  onSeatEngine,
  onSeatAccount,
  onDiscoverModels,
  discoveringModels,
  onEditOrdinary,
  onTest,
  testing,
  testResult,
}: {
  seats: Seat[];
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  review: ReviewPolicy;
  onReview: (patch: Partial<ReviewPolicy>) => void;
  onCreateDedicated: () => void;
  onSeatUpdate: (id: string, patch: Partial<Seat>) => void;
  onSeatEngine: (id: string, engine: Engine) => void;
  onSeatAccount: (id: string, key: string, credential?: GlobalCredential) => void;
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
  const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
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
        <p className="wsettings-inline-note"><Icon name="info" size={13} />仅 Pentest 生效，占用 1 个总并发名额；同时作为 BTW 默认 Worker。</p>
        <div className="wset-switch-row"><span><b>启用 Review</b><small>按触发条件启动独立审查进程</small></span><Switch size="sm" aria-label="启用 Review" isSelected={review.enabled ?? true} onChange={(enabled) => onReview({ enabled })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
        <label><span>使用 Worker</span><Select aria-label="Review Worker" selectedKey={review.engine || ""} onSelectionChange={(key) => onReview({ engine: String(key) })}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="">选择 Worker</ListBoxItem>{options.map((seat) => <ListBoxItem key={seat.id} id={seat.id} isDisabled={!seat.enabled}>{seat.label} · {credentials.find((item) => item.id === seat.credential_id)?.secret_ref || "系统登录"}{seat.enabled ? "" : " · 已停用"}</ListBoxItem>)}</ListBox></Select.Popover></Select></label>
        <div className="wset-binding-help"><span>并发固定为 1</span><Button type="button" onClick={onCreateDedicated}>创建独立配置</Button></div>
        <ReasoningEffortSelect engine={selectedEngine} model={selected?.model || ""} options={modelOptions} value={review.reasoning_effort || "inherit"} inherit onChange={(reasoning_effort) => onReview({ reasoning_effort })} />
        <label><span>超时</span><NumberField min={30} max={90} suffix="秒" value={review.timeout ?? 90} onChange={(next) => onReview({ timeout: Math.max(30, Math.min(90, Number(next) || 90)) })} /></label>
        <div className="wset-switch-row"><span><b>不可用时降级</b><small>改用下一个健康的 Review 配置</small></span><Switch size="sm" aria-label="Review 不可用时降级" isSelected={review.allow_review_fallback ?? false} onChange={(allow_review_fallback) => onReview({ allow_review_fallback })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
      </section>

      {selected ? dedicated ? (
        <section className="wset-form-section">
          <h3>独立 Review 运行绑定</h3>
          <label><span>名称</span><Input value={selected.label} onChange={(event) => onSeatUpdate(selected.id, { label: event.target.value })} /></label>
          <label><span>Worker 程序</span><Select aria-label="Review Worker 程序" selectedKey={selectedEngine} onSelectionChange={(key) => onSeatEngine(selected.id, engineOf(String(key)))}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox>{ENGINES.map((item) => <ListBoxItem key={item} id={item} isDisabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</ListBoxItem>)}</ListBox></Select.Popover></Select></label>
          <CredentialBindingEditor engine={selectedEngine} accountKey={selectedAccount} model={selected.model || ""} availableCredentials={availableCredentials} backend={backend} onBind={(key, globalCredential) => onSeatAccount(selected.id, key, globalCredential)} onModelChange={(model) => { if (model !== (selected.model || "")) onSeatUpdate(selected.id, { model, reasoning_effort: normalizeEffortForModel(selected.reasoning_effort, selectedEngine, model, modelOptions) }); }} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={selectedEngine} model={selected.model || ""} options={modelOptions} value={selected.reasoning_effort || "default"} onChange={(reasoning_effort) => onSeatUpdate(selected.id, { reasoning_effort })} />
          <div className="wset-switch-row"><span><b>启用 Review Worker</b><small>停用后保留连接和模型配置</small></span><Switch size="sm" aria-label="启用 Review Worker" isSelected={selected.enabled} onChange={(enabled) => onSeatUpdate(selected.id, { enabled })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
        </section>
      ) : (
        <section className="wset-form-section wreview-reuse-note">
          <h3>复用普通 Worker</h3>
          <p>共享连接、模型与环境；可单独覆盖推理强度。</p>
          <Button type="button" onClick={() => onEditOrdinary(selected.id)}><Icon name="chevronRight" size={13} />编辑 {selected.label}</Button>
        </section>
      ) : null}

      <details className="wset-review-triggers">
        <summary><span><Icon name="gear" size={13} />触发条件</span><Icon name="chevronDown" size={13} /></summary>
        <div>
          {[
            ["on_candidate_spike", "候选结果突然增加"],
            ["on_evidence_conflict", "证据出现冲突"],
          ].map(([key, label]) => (
            <div className="wset-switch-row" key={key}><span><b>{label}</b></span><Switch size="sm" aria-label={label} isSelected={Boolean(review[key as keyof ReviewPolicy])} onChange={(value) => onReview({ [key]: value })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
          ))}
          <label><span>连续无成果 Worker</span><NumberField min={0} value={review.after_fruitless_workers ?? 0} onChange={(next) => onReview({ after_fruitless_workers: Math.max(0, Number(next) || 0) })} /></label>
          <label><span>重复分支阈值</span><NumberField min={0} value={review.after_duplicate_intents ?? 0} onChange={(next) => onReview({ after_duplicate_intents: Math.max(0, Number(next) || 0) })} /></label>
          <label><span>候选结果阈值</span><NumberField min={1} value={review.candidate_spike_threshold ?? 5} onChange={(next) => onReview({ candidate_spike_threshold: Math.max(1, Number(next) || 1) })} /></label>
        </div>
      </details>

      <section className="wset-test-summary">
        <span className="wreview-independent"><Icon name="eye" size={13} />最多并发 1</span>
        <p>审查结果以控制建议返回</p>
      </section>
      </div>
      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}
      <footer className="wset-inspector-dock review">
        <Button type="button" className="primary" onClick={onTest} isDisabled={!selected || testing || !globalCredentialUsable(selectedGlobalCredential)}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : !globalCredentialUsable(selectedGlobalCredential) ? "请先选择可用凭据" : "测试 Review 模型"}</Button>
      </footer>
    </aside>
  );
}

function VerifierInspector({
  seats,
  credentials,
  availableCredentials,
  models,
  backend,
  verifier,
  onVerifier,
  onCreateDedicated,
  onSeatUpdate,
  onSeatEngine,
  onSeatAccount,
  onDiscoverModels,
  discoveringModels,
  onEditOrdinary,
  onTest,
  testing,
  testResult,
}: {
  seats: Seat[];
  credentials: Credential[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  verifier: VerifierPolicy;
  onVerifier: (patch: Partial<VerifierPolicy>) => void;
  onCreateDedicated: () => void;
  onSeatUpdate: (id: string, patch: Partial<Seat>) => void;
  onSeatEngine: (id: string, engine: Engine) => void;
  onSeatAccount: (id: string, key: string, credential?: GlobalCredential) => void;
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
  const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
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
        <p className="wsettings-inline-note"><Icon name="info" size={13} />仅 Pentest 生效，独立复现报告并占用总并发名额。</p>
        <div className="wset-switch-row"><span><b>启用 Verifier</b><small>独立复现已提交报告</small></span><Switch size="sm" aria-label="启用 Verifier" isSelected={verifier.enabled ?? true} onChange={(enabled) => onVerifier({ enabled })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
        <label><span>使用 Worker</span><Select aria-label="Verifier Worker" selectedKey={verifier.engine || ""} onSelectionChange={(key) => onVerifier({ engine: String(key) })}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="">选择 Worker</ListBoxItem>{options.map((seat) => <ListBoxItem key={seat.id} id={seat.id} isDisabled={!seat.enabled}>{seat.label} · {credentials.find((item) => item.id === seat.credential_id)?.secret_ref || "系统登录"}{seat.enabled ? "" : " · 已停用"}</ListBoxItem>)}</ListBox></Select.Popover></Select></label>
        <div className="wset-binding-help"><span>计入总并发上限</span><Button type="button" onClick={onCreateDedicated}>创建独立配置</Button></div>
        <ReasoningEffortSelect engine={selectedEngine} model={selected?.model || ""} options={modelOptions} value={verifier.reasoning_effort || "inherit"} inherit onChange={(reasoning_effort) => onVerifier({ reasoning_effort })} />
        <label><span>超时</span><NumberField min={60} suffix="秒" value={verifier.timeout ?? 240} onChange={(next) => onVerifier({ timeout: Math.max(60, Number(next) || 240) })} /></label>
        <label><span>子上限</span><NumberField min={0} value={verifier.max_concurrent ?? 0} onChange={(next) => onVerifier({ max_concurrent: Math.max(0, Number(next) || 0) })} /><small>0 = 自动，不超过总并发</small></label>
        <div className="wset-switch-row"><span><b>不可用时降级</b><small>改用下一个健康的 Verifier 配置</small></span><Switch size="sm" aria-label="Verifier 不可用时降级" isSelected={verifier.allow_verifier_fallback ?? false} onChange={(allow_verifier_fallback) => onVerifier({ allow_verifier_fallback })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
      </section>

      {selected ? dedicated ? (
        <section className="wset-form-section">
          <h3>独立 Verifier 运行绑定</h3>
          <label><span>名称</span><Input value={selected.label} onChange={(event) => onSeatUpdate(selected.id, { label: event.target.value })} /></label>
          <label><span>Worker 程序</span><Select aria-label="Verifier Worker 程序" selectedKey={selectedEngine} onSelectionChange={(key) => onSeatEngine(selected.id, engineOf(String(key)))}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox>{ENGINES.map((item) => <ListBoxItem key={item} id={item} isDisabled={Boolean(ENGINE_META[item].localOnly && backend !== "local")}>{ENGINE_META[item].label}{ENGINE_META[item].localOnly ? "（本地）" : ""}</ListBoxItem>)}</ListBox></Select.Popover></Select></label>
          <CredentialBindingEditor engine={selectedEngine} accountKey={selectedAccount} model={selected.model || ""} availableCredentials={availableCredentials} backend={backend} onBind={(key, globalCredential) => onSeatAccount(selected.id, key, globalCredential)} onModelChange={(model) => { if (model !== (selected.model || "")) onSeatUpdate(selected.id, { model, reasoning_effort: normalizeEffortForModel(selected.reasoning_effort, selectedEngine, model, modelOptions) }); }} onDiscoverModels={onDiscoverModels} discoveringModels={discoveringModels} />
          <ReasoningEffortSelect engine={selectedEngine} model={selected.model || ""} options={modelOptions} value={selected.reasoning_effort || "default"} onChange={(reasoning_effort) => onSeatUpdate(selected.id, { reasoning_effort })} />
          <div className="wset-switch-row"><span><b>启用 Verifier Worker</b><small>停用后保留连接和模型配置</small></span><Switch size="sm" aria-label="启用 Verifier Worker" isSelected={selected.enabled} onChange={(enabled) => onSeatUpdate(selected.id, { enabled })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
        </section>
      ) : (
        <section className="wset-form-section wreview-reuse-note">
          <h3>复用普通 Worker</h3>
          <p>共享连接、模型与环境；可单独设置推理强度与并发。</p>
          <Button type="button" onClick={() => onEditOrdinary(selected.id)}><Icon name="chevronRight" size={13} />编辑 {selected.label}</Button>
        </section>
      ) : null}

      <section className="wset-test-summary">
        <span className="wreview-independent"><Icon name="check" size={13} />独立并发 {concurrentLabel}</span>
        <p>Verifier 只复现已提交漏洞报告，占用总 Worker 并发里的子上限。</p>
      </section>
      </div>
      {testing || testResult ? <div className="wset-live-terminal"><ModelTestTerminal testing={testing} result={testResult} /></div> : null}
      <footer className="wset-inspector-dock verifier">
        <Button type="button" className="primary" onClick={onTest} isDisabled={!selected || testing || !globalCredentialUsable(selectedGlobalCredential)}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : !globalCredentialUsable(selectedGlobalCredential) ? "请先选择可用凭据" : "测试 Verifier 模型"}</Button>
      </footer>
    </aside>
  );
}

function RuntimeNote({ icon, status, title, detail }: { icon: IconName; status: "default" | "warning"; title: string; detail: string }) {
  return (
    <Alert status={status} className="wruntime-note">
      <Alert.Indicator className="wruntime-note-icon"><Icon name={icon} size={16} /></Alert.Indicator>
      <Alert.Content className="wruntime-note-copy">
        <Alert.Title className="wruntime-note-title">{title}</Alert.Title>
        <Alert.Description className="wruntime-note-detail">{detail}</Alert.Description>
      </Alert.Content>
    </Alert>
  );
}

function RuntimeWorkspace({ backend, network, effectiveNetwork, networkError, containerScope, workerPrivilege, vpnEnabled, vpnStatus, vpnUploading, seatCount, imageStatus, imageLoading, pulling, inContainer, onBackend, onNetwork, onContainerScope, onWorkerPrivilege, onVpnEnabled, onVpnUpload, onRefreshImage, onPullImage }: {
  backend: WorkerSettings["worker_backend"];
  network: WorkerSettings["worker_network"];
  effectiveNetwork?: string;
  networkError?: string;
  containerScope: WorkerSettings["worker_container_scope"];
  vpnEnabled: boolean;
  vpnStatus: OpenVpnStatus;
  vpnUploading: boolean;
  seatCount: number;
  imageStatus: WorkerImageStatus | null;
  imageLoading: boolean;
  pulling: boolean;
  inContainer: boolean;
  onBackend: (backend: WorkerSettings["worker_backend"]) => void;
  onNetwork: (network: WorkerSettings["worker_network"]) => void;
  onContainerScope: (scope: WorkerSettings["worker_container_scope"]) => void;
  workerPrivilege: "default" | "elevated";
  onWorkerPrivilege: (priv: "default" | "elevated") => void;
  onVpnEnabled: (enabled: boolean) => void;
  onVpnUpload: (file: File) => void;
  onRefreshImage: () => void;
  onPullImage: () => void;
}) {
  const vpnInputRef = useRef<HTMLInputElement>(null);
  const networkOptions: { id: WorkerSettings["worker_network"]; label: string; detail: string }[] = [
    { id: "bridge", label: "Bridge", detail: "Docker bridge；Compose 下可能加入共享网络" },
    { id: "host", label: "Host", detail: "显式使用宿主机网络（非静默默认）" },
    { id: "none", label: "None", detail: "真实 --network none；RCP/远端模型不可达时创建任务会拒绝，不会改成 bridge" },
  ];
  const modeOptions: { id: WorkerSettings["worker_backend"]; icon: IconName; label: string; detail: string; disabled: boolean }[] = [
    { id: "local", icon: "terminal", label: "本地运行", detail: inContainer ? "当前部署不支持宿主机 CLI" : "使用宿主机 CLI，可继承系统登录", disabled: inContainer },
    { id: "container", icon: "layers", label: "容器运行", detail: "隔离运行，需要绑定可注入凭据", disabled: false },
  ];
  const versionStatus = imageStatus?.version.status;
  const imageChecks: { id: string; label: string; value: string; tone: "default" | "success" | "danger" | "warning" }[] = [
    { id: "daemon", label: "Docker 服务", value: imageLoading ? "检查中" : imageStatus?.daemon.ok ? "可用" : "不可用", tone: imageLoading ? "default" : imageStatus?.daemon.ok ? "success" : "danger" },
    { id: "pulled", label: "本地镜像", value: imageLoading ? "检查中" : imageStatus?.pulled.ok ? "已存在" : "未找到", tone: imageLoading ? "default" : imageStatus?.pulled.ok ? "success" : "danger" },
    { id: "version", label: "镜像版本", value: imageLoading ? "检查中" : imageStatus?.version.actual || "未知", tone: imageLoading ? "default" : versionStatus === "match" ? "success" : versionStatus === "mismatch" ? "danger" : "warning" },
  ];
  const container = backend === "container";
  const environmentNote = inContainer
    ? { icon: "layers" as const, status: "warning" as const, title: "当前只能使用容器 Worker", detail: "当前部署无法直接调用宿主机 CLI。" }
    : null;
  return (
    <div className="wruntime-workspace wruntime-layout" data-backend={backend}>
      <div className="wruntime-canvas">
        <section className="wruntime-stage" aria-label="执行环境设置">
          <Card className="wruntime-card">
            <Card.Header className="wruntime-card-head">
              <div className="wsettings-card-heading"><Icon name="terminal" size={17} /><Card.Title>执行环境</Card.Title></div>
              <Chip size="sm" variant="soft">{seatCount} 个 Worker</Chip>
            </Card.Header>
            <Card.Content className="wruntime-card-body">
              <RadioGroup className="wruntime-modes" aria-label="执行环境" orientation="horizontal" value={backend} onChange={(value) => onBackend(value as WorkerSettings["worker_backend"])}>
                {modeOptions.map((option) => (
                  <Radio key={option.id} className="wruntime-mode" value={option.id} isDisabled={option.disabled}>
                    <Radio.Content className="wruntime-mode-face">
                      <Radio.Control><Radio.Indicator /></Radio.Control>
                      <span className="wruntime-mode-icon"><Icon name={option.icon} size={18} /></span>
                      <span className="wruntime-mode-copy"><strong>{option.label}</strong><small>{option.detail}</small></span>
                    </Radio.Content>
                  </Radio>
                ))}
              </RadioGroup>
              {environmentNote ? <RuntimeNote icon={environmentNote.icon} status={environmentNote.status} title={environmentNote.title} detail={environmentNote.detail} /> : null}
            </Card.Content>
          </Card>

          {container ? <Card className="wruntime-card wruntime-container-settings">
            <Card.Header className="wruntime-card-head">
              <div className="wsettings-card-heading"><Icon name="layers" size={17} /><Card.Title>容器设置</Card.Title></div>
              <Chip size="sm" variant="soft" color="accent">Docker</Chip>
            </Card.Header>
            <Card.Content className="wruntime-card-body">
              <div className="wsettings-option-row">
                <div><strong>容器作用域</strong><small>{containerScope === "shared" ? "互信任务可互读文件和凭据，共用容器与资源上限" : "每个任务使用独立容器"}</small></div>
                <Select aria-label="容器作用域" selectedKey={containerScope} onSelectionChange={(key) => onContainerScope(key as WorkerSettings["worker_container_scope"])}>
                  <Select.Trigger><Select.Value /></Select.Trigger>
                  <Select.Popover><ListBox><ListBoxItem id="run">每个任务独立</ListBoxItem><ListBoxItem id="shared">互信任务共享</ListBoxItem></ListBox></Select.Popover>
                </Select>
              </div>
              <div className="wsettings-option-row">
                <div><strong>Worker 权限</strong><small>{workerPrivilege === "elevated" ? "允许 sudo 和安装系统软件" : "禁止通过 sudo 或可执行文件提升权限"}</small></div>
                <Select aria-label="Worker 权限" selectedKey={workerPrivilege} onSelectionChange={(key) => onWorkerPrivilege(key as "default" | "elevated")}>
                  <Select.Trigger><Select.Value /></Select.Trigger>
                  <Select.Popover><ListBox><ListBoxItem id="default">默认降权</ListBoxItem><ListBoxItem id="elevated">扩展权限</ListBoxItem></ListBox></Select.Popover>
                </Select>
              </div>
              <div className="wsettings-option-row">
                <div><strong>容器网络</strong><small>{networkOptions.find((option) => option.id === network)?.detail}{effectiveNetwork && effectiveNetwork !== network ? ` · 实际生效：${effectiveNetwork}` : ""}{networkError ? ` · ${networkError}` : ""}</small></div>
                <Select aria-label="容器网络" selectedKey={network} onSelectionChange={(key) => onNetwork(key as WorkerSettings["worker_network"])}>
                  <Select.Trigger><Select.Value /></Select.Trigger>
                  <Select.Popover><ListBox>{networkOptions.map((option) => <ListBoxItem key={option.id} id={option.id}>{option.label}</ListBoxItem>)}</ListBox></Select.Popover>
                </Select>
              </div>
              <div className="wsettings-option-row wruntime-vpn-toggle">
                <div><strong>OpenVPN</strong><small>上传 .ovpn 后启用，每个任务容器连接一次</small></div>
                <Switch size="sm" aria-label="启用 OpenVPN" isSelected={vpnEnabled} onChange={onVpnEnabled}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch>
              </div>
              <div className="wruntime-vpn-file">
                <Icon name="file" size={19} />
                <div><strong>{vpnStatus.present ? vpnStatus.filename || "client.ovpn" : "尚未上传配置"}</strong><small>{vpnStatus.present ? `${vpnStatus.size || 0} 字节` : vpnEnabled ? "缺少配置" : ".ovpn"}</small></div>
                <input ref={vpnInputRef} type="file" accept=".ovpn,application/x-openvpn-profile" hidden disabled={vpnUploading} onChange={(event) => { const file = event.target.files?.[0]; if (file) onVpnUpload(file); event.currentTarget.value = ""; }} />
                <Button type="button" size="sm" variant="secondary" isDisabled={vpnUploading} onClick={() => vpnInputRef.current?.click()}><Icon name="upload" size={13} />{vpnUploading ? "上传中…" : "上传 .ovpn"}</Button>
              </div>
            </Card.Content>
          </Card> : null}
        </section>

        {container ? <aside className="wruntime-aside" aria-label="Worker 镜像状态">
          <Card className="wruntime-card wruntime-image-card">
            <Card.Header className="wruntime-card-head">
              <div className="wsettings-card-heading"><Icon name="layers" size={17} /><Card.Title>Worker 镜像</Card.Title></div>
              <Button type="button" size="sm" variant="ghost" isIconOnly aria-label="刷新镜像状态" onClick={onRefreshImage} isDisabled={imageLoading || pulling}><Icon name="refresh" size={15} /></Button>
            </Card.Header>
            <Card.Content className="wruntime-card-body">
              <code className="wruntime-image-ref">{imageStatus?.image || "读取中…"}</code>
              <div className="wruntime-image-checks">
                {imageChecks.map((check) => <div key={check.id} className="wruntime-image-check"><span>{check.label}</span><Chip size="sm" variant="soft" color={check.tone}>{check.value}</Chip></div>)}
              </div>
              {versionStatus === "mismatch" && imageStatus?.version.expected ? <RuntimeNote icon="alert" status="warning" title={`期望版本 ${imageStatus.version.expected}`} detail="当前镜像版本与期望版本不一致，建议重新拉取镜像。" /> : null}
            </Card.Content>
            <Card.Footer className="wruntime-card-foot">
              <div className="wruntime-card-foot-actions"><Button type="button" size="sm" variant="primary" onClick={onPullImage} isDisabled={pulling || !imageStatus?.daemon.ok}><Icon name="download" size={14} />{pulling ? "拉取中…" : "拉取镜像"}</Button></div>
              <span className="wruntime-card-foot-hint"><code>{WORKER_IMAGE_ENV}</code> 可指定镜像</span>
            </Card.Footer>
          </Card>
        </aside> : null}
      </div>
    </div>
  );
}

function SchedulingWorkspace({ config, raceTimeout, dispatchMode, startWorkers, maxTotal, wallClock, costBudget, maxWorkers, onChange }: { config: WorkerSettings; raceTimeout: number; dispatchMode: "fixed" | "auto"; startWorkers: number; maxTotal: number; wallClock: number; costBudget: number; maxWorkers: number; onChange: (patch: Partial<{ raceTimeout: number; dispatchMode: "fixed" | "auto"; startWorkers: number; maxTotal: number; wallClock: number; costBudget: number }>) => void }) {
  const auto = dispatchMode === "auto";
  const rosterCap = Math.max(1, maxWorkers);
  return (
    <div className="wsettings-simple-page wscheduling-page">
      <div className="wscheduling-layout">
        <Card className="wscheduling-card">
          <Card.Header className="wsettings-card-heading"><Icon name="gear" size={17} /><Card.Title>调度方式</Card.Title></Card.Header>
          <Card.Content>
            <RadioGroup className="wruntime-modes wscheduling-modes" aria-label="调度方式" orientation="horizontal" value={dispatchMode} onChange={(value) => onChange({ dispatchMode: value === "auto" ? "auto" : "fixed" })}>
              <Radio className="wruntime-mode" value="auto"><Radio.Content className="wruntime-mode-face"><Radio.Control><Radio.Indicator /></Radio.Control><span className="wruntime-mode-copy"><strong>自动调度</strong><small>CTF 由 Pi Decide 规划 Step，按可用槽位派发</small></span></Radio.Content></Radio>
              <Radio className="wruntime-mode" value="fixed"><Radio.Content className="wruntime-mode-face"><Radio.Control><Radio.Indicator /></Radio.Control><span className="wruntime-mode-copy"><strong>固定并跑（Race）</strong><small>入选 Worker 并行开局，后续由 Coordinator 补位</small></span></Radio.Content></Radio>
            </RadioGroup>
            <p className="wsettings-inline-note">固定并跑（Race）为过渡模式；未来只保留自动调度。</p>
          </Card.Content>
          <Card.Footer className="wscheduling-capacity" aria-live="polite">
            <div><span>{auto ? "CTF 自动调度并发" : "固定并跑总并发"}</span><strong>{auto ? CTF_AUTO_CONCURRENCY : rosterCap}</strong><small>{auto ? "普通 Worker 上限；不执行首轮 Race" : "启用席位的并发上限之和"}</small></div>
            <div><span>{auto ? "阵容并发合计" : "CTF 自动调度并发 · 未启用"}</span><strong>{auto ? rosterCap : CTF_AUTO_CONCURRENCY}</strong><small>{auto ? "固定并跑可用容量" : "仅 CTF 自动调度生效"}</small></div>
          </Card.Footer>
        </Card>

        <Card className="wscheduling-card">
          <Card.Header className="wsettings-card-heading"><Icon name="play" size={17} /><Card.Title>{auto ? "自动启动" : "固定并跑首轮"}</Card.Title></Card.Header>
          <Card.Content className="wscheduling-fields">
            {auto ? <p className="wsettings-inline-note">CTF 首轮由 Pi Decide 规划；可执行 Step 按需启动，普通 Worker 同时最多 3 个。</p> : <>
              <div className="wscheduling-field">
                <label><span>Race 超时</span><NumberField ariaLabel="Race 超时" min={60} suffix="秒" value={raceTimeout} onChange={(next) => onChange({ raceTimeout: Math.max(60, Number(next) || 60) })} /></label>
                <small>每个首轮 Worker 的执行时限</small>
              </div>
              <div className="wscheduling-field">
                <label><span>首轮 Worker</span><NumberField ariaLabel="首轮 Worker" min={1} max={rosterCap} value={startWorkers} onChange={(next) => onChange({ startWorkers: Math.max(1, Number(next) || 1) })} /></label>
                <small>从启用阵容中按顺序选择；要五个引擎同时开局，至少设为 5</small>
              </div>
            </>}
          </Card.Content>
        </Card>

        <Card className="wscheduling-card">
          <Card.Header className="wsettings-card-heading"><Icon name="clock" size={17} /><Card.Title>运行预算</Card.Title><Chip size="sm" variant="soft">0 = 不限</Chip></Card.Header>
          <Card.Content className="wscheduling-fields">
            <div className="wscheduling-field"><label><span>累计启动上限</span><NumberField ariaLabel="累计启动上限" min={0} value={maxTotal} onChange={(next) => onChange({ maxTotal: Math.max(0, Number(next) || 0) })} /></label><small>到限后等待已启动 Worker 完成</small></div>
            <div className="wscheduling-field"><label><span>最长运行时间</span><NumberField ariaLabel="最长运行时间" min={0} suffix="秒" value={wallClock} onChange={(next) => onChange({ wallClock: Math.max(0, Number(next) || 0) })} /></label><small>到限后结束剩余任务</small></div>
            <div className="wscheduling-field"><label><span>成本预算</span><NumberField ariaLabel="成本预算" min={0} step={0.1} suffix="USD" value={costBudget} onChange={(next) => onChange({ costBudget: Math.max(0, Number(next) || 0) })} /></label><small>按已记账成本计算，到限后停止 Worker</small></div>
          </Card.Content>
        </Card>
        {Object.keys(config.overrides || {}).length ? <p className="wsettings-inline-note"><Icon name="alert" size={13} />已保留 {Object.keys(config.overrides).length} 个题型专属配置</p> : null}
      </div>
    </div>
  );
}

function LlmProfileCard({
  profile,
  endpoints,
  which,
  onUpdate,
  onEndpointSaved,
}: {
  profile: WorkerSettings["llm_profiles"][LlmProfileName];
  endpoints: ModelEndpoint[];
  which: LlmProfileName;
  onUpdate: (patch: Partial<WorkerSettings["llm_profiles"]["planner"]>) => void;
  onEndpointSaved: (endpoint: ModelEndpoint) => void;
}) {
  const temperatureMode = llmTemperatureMode(profile);
  const customTemperature = temperatureMode !== "default";
  const selectedEndpoint = endpoints.find((endpoint) => endpoint.id === (profile.endpoint_id || profile.credential_id)) || null;
  const testSignature = [profile.endpoint_id || profile.credential_id || "", profile.model || "", temperatureMode, profile.temperature ?? ""].join("\u0000");
  const [testing, setTesting] = useState(false);
  const [customEndpoint, setCustomEndpoint] = useState(false);
  const [editingEndpoint, setEditingEndpoint] = useState(false);
  const [savingCustom, setSavingCustom] = useState(false);
  const [showCustomKey, setShowCustomKey] = useState(false);
  const [customError, setCustomError] = useState("");
  const [customDraft, setCustomDraft] = useState({ id: "", provider: "", base_url: "", api_key: "", model: "" });
  const [testState, setTestState] = useState<{ signature: string; result: WorkerModelTestResult } | null>(null);
  const testResult = testState?.signature === testSignature ? testState.result : null;
  const modelOptions = useMemo(() => {
    const rows = new Map<string, string>();
    if (selectedEndpoint?.default_model) rows.set(selectedEndpoint.default_model, "端点默认模型");
    for (const model of selectedEndpoint?.models || []) {
      if (!rows.has(model)) rows.set(model, "端点目录模型");
    }
    if (profile.model && !rows.has(profile.model)) rows.set(profile.model, "当前已保存模型");
    return [...rows.entries()].map(([id, label]) => ({ id, label }));
  }, [profile.model, selectedEndpoint?.default_model, selectedEndpoint?.models]);
  const resetCustomDraft = () => {
    setCustomDraft({ id: "", provider: "", base_url: "", api_key: "", model: "" });
    setShowCustomKey(false);
    setCustomError("");
  };
  const closeCustomEndpoint = () => {
    setCustomEndpoint(false);
    setEditingEndpoint(false);
    resetCustomDraft();
  };
  const beginEditEndpoint = () => {
    if (!selectedEndpoint) return;
    const accountId = String(selectedEndpoint.label || selectedEndpoint.id.replace(/^endpoint:/, "")).trim();
    setCustomEndpoint(true);
    setEditingEndpoint(true);
    setCustomError("");
    setShowCustomKey(false);
    setCustomDraft({
      id: accountId,
      provider: selectedEndpoint.provider || "",
      base_url: selectedEndpoint.base_url || "",
      api_key: "",
      model: profile.model || selectedEndpoint.default_model || selectedEndpoint.models[0] || "",
    });
  };
  const chooseEndpoint = (endpointId: string) => {
    if (endpointId === "__custom__") {
      setCustomEndpoint(true);
      setEditingEndpoint(false);
      resetCustomDraft();
      onUpdate({ endpoint_id: "", credential_id: undefined, model: "" });
      return;
    }
    closeCustomEndpoint();
    const next = endpoints.find((endpoint) => endpoint.id === endpointId);
    if (!next) {
      onUpdate({ endpoint_id: "", credential_id: undefined, model: "" });
      return;
    }
    const allowedModels = new Set([
      next.default_model || "",
      ...next.models,
    ].filter(Boolean));
    onUpdate({
      endpoint_id: next.id,
      credential_id: undefined,
      model: allowedModels.has(profile.model)
        ? profile.model
        : next.default_model || next.models[0] || "",
    });
  };
  const saveCustomEndpoint = async () => {
    if (savingCustom) return;
    const accountId = customDraft.id.trim();
    const baseUrl = customDraft.base_url.trim();
    const model = customDraft.model.trim();
    const apiKey = customDraft.api_key.trim();
    if (!accountId || !baseUrl || !model) return;
    if (!editingEndpoint && !apiKey) return;
    setSavingCustom(true);
    setCustomError("");
    const result = await createModelEndpoint({
      id: accountId,
      provider: customDraft.provider.trim(),
      base_url: baseUrl,
      ...(apiKey ? { api_key: apiKey } : {}),
      model,
    });
    setSavingCustom(false);
    if (!result.ok || !result.endpoint) {
      setCustomError(result.detail || "模型端点保存失败");
      return;
    }
    onEndpointSaved(result.endpoint);
    onUpdate({
      endpoint_id: result.endpoint.id,
      credential_id: undefined,
      model,
    });
    closeCustomEndpoint();
  };
  const runTest = async () => {
    if (!selectedEndpoint?.present || !profile.model.trim() || testing) return;
    const signature = testSignature;
    const startedAt = performance.now();
    setTesting(true);
    setTestState(null);
    try {
      const response = await testLlmEndpoint(
        which,
        selectedEndpoint.id,
        profile.model.trim(),
        temperatureMode,
        profile.temperature,
      );
      const elapsedMs = Math.max(0, Math.round(performance.now() - startedAt));
      setTestState({
        signature,
        result: {
          ok: response.ok,
          detail: response.detail || (response.ok ? "模型已返回有效响应" : "模型测试未通过"),
          model: response.model || profile.model,
          engine: "HTTP 模型 API",
          elapsed_ms: elapsedMs,
          tested_at: Date.now() / 1000,
          logs: [
            { stream: "command", message: `${which} · ${profile.model.trim()}`, elapsed_ms: 0 },
            { stream: response.ok ? "success" : "error", message: response.detail || (response.ok ? "模型已返回有效响应" : "模型测试未通过"), elapsed_ms: elapsedMs },
          ],
        },
      });
    } finally {
      setTesting(false);
    }
  };
  return (
    <Card className="wsettings-setting-group wmodel-settings-card">
      <Card.Header className="wmodel-detail-heading">
        <span className="wmodel-purpose-icon"><Icon name={which === "planner" ? "sparkles" : "messages"} size={20} /></span>
        <div><Card.Title>{which === "planner" ? "Planner" : "Titler"}</Card.Title><Card.Description>{which === "planner" ? "协调决策；下发后为任务列表生成标题和分类" : "会话标题；翻译与摘要复用此模型名"}</Card.Description></div>
        <Chip size="sm" variant="soft" color={selectedEndpoint?.present ? "default" : "warning"}>{selectedEndpoint?.label || "未选择模型端点"}</Chip>
      </Card.Header>
      <Card.Content className="wmodel-detail-body">
        <section className="wmodel-connection-section" aria-label="模型连接">
          <h3>模型连接</h3>
      <label><span>模型端点</span><Select aria-label="模型端点" selectedKey={customEndpoint && !editingEndpoint ? "__custom__" : selectedEndpoint?.id || ""} onSelectionChange={(key) => chooseEndpoint(String(key))}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="">选择模型端点</ListBoxItem>{endpoints.map((endpoint) => <ListBoxItem key={endpoint.id} id={endpoint.id} isDisabled={!endpoint.present}>{endpoint.label || endpoint.id}{endpoint.provider ? ` · ${endpoint.provider}` : ""}{endpoint.present ? "" : " · 不可用"}</ListBoxItem>)}<ListBoxItem id="__custom__">＋ 自定义模型端点</ListBoxItem></ListBox></Select.Popover></Select></label>
      {customEndpoint ? <div className="wmodel-custom-endpoint" aria-label={editingEndpoint ? "编辑模型端点" : "自定义模型端点"}>
        <p><Icon name="info" size={13} />{editingEndpoint
          ? "端点修改立即保存；API Key 留空则保留原值。"
          : `填写后会保存为独立模型端点，并立即用于当前 ${which === "planner" ? "Reason / Planner" : "Titler"}。`}</p>
        <label><span>端点名称</span><Input autoComplete="off" placeholder="例如 reason-gateway" value={customDraft.id} disabled={editingEndpoint} onChange={(event) => setCustomDraft((value) => ({ ...value, id: event.target.value }))} /></label>
        <label><span>服务名称</span><Input autoComplete="off" placeholder="可选，例如 OpenAI Compatible" value={customDraft.provider} onChange={(event) => setCustomDraft((value) => ({ ...value, provider: event.target.value }))} /></label>
        <label><span>Base URL</span><Input autoComplete="url" placeholder="https://api.example.com/v1" value={customDraft.base_url} onChange={(event) => setCustomDraft((value) => ({ ...value, base_url: event.target.value }))} /></label>
        <label className="wmodel-custom-key"><span>API Key</span><div><Input type={showCustomKey ? "text" : "password"} autoComplete="new-password" placeholder={editingEndpoint ? "留空则保留原 Key" : "输入 API Key"} value={customDraft.api_key} onChange={(event) => setCustomDraft((value) => ({ ...value, api_key: event.target.value }))} /><Button type="button" aria-label={showCustomKey ? "隐藏 API Key" : "显示 API Key"} onClick={() => setShowCustomKey((value) => !value)}><Icon name={showCustomKey ? "eyeOff" : "eye"} size={14} /></Button></div></label>
        <label><span>模型 ID</span><Input autoComplete="off" placeholder="例如 deepseek-v4-flash" value={customDraft.model} onChange={(event) => setCustomDraft((value) => ({ ...value, model: event.target.value }))} /></label>
        {customError ? <p className="wmodel-custom-error" role="alert"><Icon name="alert" size={13} />{customError}</p> : null}
        <div className="wmodel-custom-actions">
          <Button type="button" className="wsettings-test-button" isDisabled={savingCustom || !customDraft.id.trim() || !customDraft.base_url.trim() || !customDraft.model.trim() || (!editingEndpoint && !customDraft.api_key.trim())} onClick={() => void saveCustomEndpoint()}><Icon name="check" size={13} />{savingCustom ? "正在保存端点…" : editingEndpoint ? "保存修改" : "保存并选择此端点"}</Button>
          <Button type="button" className="wsettings-test-button wmodel-custom-cancel" isDisabled={savingCustom} onClick={closeCustomEndpoint}>取消</Button>
        </div>
      </div> : selectedEndpoint ? <div className="wmodel-global-credential">
        <div className="wmodel-global-credential-meta">
          <div><span>调用方式</span><strong>{which === "planner" ? "HTTP 端点" : "后端 HTTP 直连"}</strong></div>
          <div><span>服务地址</span><strong>{selectedEndpoint.base_url}</strong></div>
          <div><span>状态</span><strong>{selectedEndpoint.present ? "已配置" : "端点不可用"}</strong></div>
        </div>
        <Button type="button" className="wmodel-edit-endpoint" onClick={beginEditEndpoint}><Icon name="pencil" size={13} />编辑端点</Button>
      </div> : <p className="wmodel-credential-note missing"><Icon name="alert" size={13} />选择已有端点，或在列表中添加自定义端点。</p>}
      {!customEndpoint ? <label><span>端点模型</span><Select aria-label="端点模型" selectedKey={profile.model || ""} isDisabled={!selectedEndpoint?.present || modelOptions.length === 0} onSelectionChange={(key) => onUpdate({ model: String(key) })}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="">选择该端点已有模型</ListBoxItem>{modelOptions.map((model) => <ListBoxItem key={model.id} id={model.id}>{model.id} · {model.label}</ListBoxItem>)}</ListBox></Select.Popover></Select></label> : null}
        </section>
        <section className="wmodel-parameters-section" aria-label="生成参数">
          <h3>生成参数</h3>
      <div className="wset-switch-row wmodel-temperature-switch"><span><b>自定义 Temperature</b><small>{which === "planner" ? "用于 Reason 与任务列表标题" : "关闭时使用默认值"}</small></span><Switch size="sm" aria-label="自定义 Temperature" isSelected={customTemperature} onChange={(on) => onUpdate({ temperature_mode: on ? (temperatureMode === "omit" ? "omit" : "custom") : "default", temperature: profile.temperature ?? 1 })}><Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control></Switch.Content></Switch></div>
      {customTemperature ? <>
        <label><span>发送策略</span><Select aria-label="Temperature 发送策略" selectedKey={temperatureMode === "omit" ? "omit" : "custom"} onSelectionChange={(key) => onUpdate({ temperature_mode: key === "omit" ? "omit" : "custom" })}><Select.Trigger><Select.Value /></Select.Trigger><Select.Popover><ListBox><ListBoxItem id="custom">发送指定值</ListBoxItem><ListBoxItem id="omit">不发送该参数</ListBoxItem></ListBox></Select.Popover></Select></label>
        {temperatureMode === "omit" ? <p className="wmodel-temperature-note">使用模型默认行为</p> : <label><span>Temperature</span><NumberField min={0} max={2} step={0.1} value={profile.temperature ?? 1} onChange={(next) => onUpdate({ temperature: Number(next) })} /></label>}
      </> : null}
        </section>
      </Card.Content>
      <Card.Footer className="wmodel-detail-footer">
        <p className="wmodel-credential-note"><Icon name="info" size={13} />{which === "planner" ? "HTTP 测试覆盖 Reason 与任务列表标题。" : "标题通过此端点调用；翻译与摘要使用 Planner 端点。"}</p>
      <Button type="button" className="wsettings-test-button" isDisabled={!selectedEndpoint?.present || !profile.model.trim() || testing} onClick={() => void runTest()}><Icon name="plug" size={13} />{testing ? "正在与模型交互…" : !selectedEndpoint?.present ? "请先选择可用端点" : !profile.model.trim() ? "请先选择模型" : "测试模型连接"}</Button>
      </Card.Footer>
      <div className="wmodel-detail-terminal">
      <ModelTestTerminal testing={testing} result={testResult} compact data-tooltip={`${which === "planner" ? "Reason / Planner" : "Titler"} 测试终端`} runningMessage="正在向当前模型发送最小测试请求，请等待返回…" />
      </div>
    </Card>
  );
}

function ModelsWorkspace({ value, endpoints, onChange, onEndpointSaved }: { value: WorkerSettings["llm_profiles"]; endpoints: ModelEndpoint[]; onChange: (next: WorkerSettings["llm_profiles"]) => void; onEndpointSaved: (endpoint: ModelEndpoint) => void }) {
  const [selectedProfile, setSelectedProfile] = useState<LlmProfileName>("planner");
  const update = (which: LlmProfileName, patch: Partial<WorkerSettings["llm_profiles"]["planner"]>) => onChange({ ...value, [which]: { ...value[which], ...patch } });
  return (
    <div className="wsettings-simple-page wmodels-page wmodels-layout">
      <aside className="wmodels-profile-nav" aria-label="模型用途">
        <h2>模型用途</h2>
        <nav aria-label="推理模型">
          {(["planner", "titler"] as const).map((which) => (
            <Button key={which} type="button" variant="ghost" className={selectedProfile === which ? "on" : ""} aria-pressed={selectedProfile === which} aria-controls={`model-profile-${which}`} onPress={() => setSelectedProfile(which)}>
              <Icon name={which === "planner" ? "sparkles" : "messages"} size={18} />
              <span><strong>{which === "planner" ? "Planner" : "Titler"}</strong><small>{which === "planner" ? "决策与任务列表标题" : "标题与辅助生成"}</small></span>
              <Icon name="chevronRight" size={14} />
            </Button>
          ))}
        </nav>
        <a className="wmodels-center-link" href="/settings/agents"><Icon name="lock" size={13} />管理模型端点<Icon name="arrowUpRight" size={13} /></a>
      </aside>
      <div className="wmodels-profile-content">
        {(["planner", "titler"] as const).map((which) => (
          <div key={which} id={`model-profile-${which}`} className="wmodels-profile-panel" hidden={selectedProfile !== which}>
            <LlmProfileCard which={which} profile={value[which]} endpoints={endpoints} onUpdate={(patch) => update(which, patch)} onEndpointSaved={onEndpointSaved} />
          </div>
        ))}
      </div>
    </div>
  );
}

const APPEARANCE_TOKENS = ["--blue", "--green", "--amber", "--cyan", "--pink", "--violet", "--magenta", "--red", "--gold"];

/**
 * 外观配色 — palette-engine 的控制台。预设方案是四个命名主色；自定义滑杆把
 * 任意 OKLCH 色相喂给引擎，语义色保持固定、装饰色自动避让、对比度由引擎
 * 保证 ≥ WCAG AA。所有改动即时应用到当前页面并持久化到本浏览器，
 * 主工作台下次加载（或切换亮暗模式）时沿用。
 */
export function AppearanceWorkspace({ hideIntro = false }: { hideIntro?: boolean }) {
  const { lang, setLang } = useLang();
  const t = useT();
  const solveOnly = useSolveOnlyMode();
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
  const schemeName = (id: string) => t(`settingsHub.appearance.scheme.${id}`);

  return (
    <div className="wsettings-simple-page wappearance-page">
      {hideIntro ? null : (
        <header className="wsettings-section-head"><div className="wsettings-section-copy">
          <h2>外观配色</h2>
          <p>配色引擎以主色色相为输入自动生成全套强调色：绿/琥珀/红/金等语义色固定不变，青/紫/品红/粉等装饰色与主色冲突时自动避让，主色对比度始终不低于 WCAG AA（4.5:1）。改动即时生效并保存在本浏览器。</p>
        </div></header>
      )}

      <section className="wappearance-card wappearance-choice-card" aria-labelledby="wappearance-language">
        <header><h3 id="wappearance-language">{t("settingsHub.language")}</h3><span>{lang === "zh" ? t("settingsHub.languageZh") : t("settingsHub.languageEn")}</span></header>
        <p>{t("settingsHub.languageHint")}</p>
        <RadioGroup orientation="horizontal" value={lang} onChange={(value) => setLang(value as "zh" | "en")} className="wappearance-modes" aria-label={t("settingsHub.language")}>
          <Radio value="zh" className={lang === "zh" ? "on" : ""}><Radio.Content><Radio.Control><Radio.Indicator /></Radio.Control>{t("settingsHub.languageZh")}</Radio.Content></Radio>
          <Radio value="en" className={lang === "en" ? "on" : ""}><Radio.Content><Radio.Control><Radio.Indicator /></Radio.Control>{t("settingsHub.languageEn")}</Radio.Content></Radio>
        </RadioGroup>
      </section>

      <section className="wappearance-card wappearance-choice-card" aria-labelledby="wappearance-mode">
        <header><h3 id="wappearance-mode">{t("settingsHub.appearance.mode")}</h3><span>{mode === "light" ? t("settingsHub.appearance.light") : t("settingsHub.appearance.dark")}</span></header>
        <p>{t("settingsHub.appearance.modeHint")}</p>
        <RadioGroup orientation="horizontal" value={mode} onChange={(value) => setMode(value as ThemeMode)} className="wappearance-modes" aria-label={t("settingsHub.appearance.mode")}>
          <Radio value="light" className={mode === "light" ? "on" : ""}><Radio.Content><Radio.Control><Radio.Indicator /></Radio.Control><Icon name="sun" size={14} />{t("settingsHub.appearance.light")}</Radio.Content></Radio>
          <Radio value="dark" className={mode === "dark" ? "on" : ""}><Radio.Content><Radio.Control><Radio.Indicator /></Radio.Control><Icon name="moon" size={14} />{t("settingsHub.appearance.dark")}</Radio.Content></Radio>
        </RadioGroup>
      </section>

      <section className="wappearance-card wappearance-choice-card" aria-labelledby="wappearance-workspaces">
        <header><h3 id="wappearance-workspaces">{lang === "zh" ? "工作区模式" : "Workspace mode"}</h3><span>{solveOnly ? (lang === "zh" ? "仅做题" : "Solve only") : (lang === "zh" ? "全部工作区" : "All workspaces")}</span></header>
        <p>{lang === "zh" ? "默认只显示做题模式。开启此项后，首页、导航和搜索会加入对话与比赛工作区。" : "Only the solve workspace is shown by default. Turn this on to add chat and competition workspaces to the home page, navigation, and search."}</p>
        <Switch isSelected={!solveOnly} onChange={(enabled) => setSolveOnlyMode(!enabled)} aria-label={lang === "zh" ? "显示对话和比赛模式" : "Show chat and competition modes"}>
          <Switch.Content><Switch.Control><Switch.Thumb /></Switch.Control>{lang === "zh" ? "显示对话和比赛模式" : "Show chat and competition modes"}</Switch.Content>
        </Switch>
      </section>

      <MotionPreferences />

      <section className="wappearance-card" aria-labelledby="wappearance-presets">
        <header><h3 id="wappearance-presets">{t("settingsHub.appearance.presets")}</h3><span>{sel.kind === "preset" ? schemeName(sel.id) : t("settingsHub.appearance.custom")}</span></header>
        <div className="wappearance-presets">
          {SCHEMES.map((s) => {
            const p = buildPalette(s.id, mode);
            const on = sel.kind === "preset" && sel.id === s.id;
            return (
              <Button key={s.id} type="button" className={`wappearance-preset${on ? " on" : ""}`} onClick={() => setSel({ kind: "preset", id: s.id })} aria-pressed={on}>
                <i style={{ background: p["--accent"] }} />
                <strong>{schemeName(s.id)}</strong>
                <code>{p["--accent"]}</code>
              </Button>
            );
          })}
        </div>
      </section>

      <section className="wappearance-card wappearance-custom-card" aria-labelledby="wappearance-custom">
        <header><h3 id="wappearance-custom">{t("settingsHub.appearance.customColor")}</h3><span>{sel.kind === "custom" ? `${t("settingsHub.appearance.hue")} ${hue}° · ${palette["--accent"]}` : t("settingsHub.appearance.dragHint")}</span></header>
        <Slider
          className="wappearance-hue"
          minValue={0}
          maxValue={359}
          value={hue}
          onChange={(value) => setSel({ kind: "custom", hue: Number(value) })}
          aria-label={t("settingsHub.appearance.hue")}
        >
          <Slider.Track className="wappearance-hue-track">
            <Slider.Thumb className="wappearance-hue-thumb" />
          </Slider.Track>
        </Slider>
        <div className="wappearance-family">
          {APPEARANCE_TOKENS.map((token) => (
            <span key={token} className="wappearance-swatch"><i style={{ background: palette[token] }} /><em>{token.slice(2)}</em><code>{palette[token]}</code></span>
          ))}
        </div>
      </section>

      {!solveOnly ? <section className="wappearance-card" aria-labelledby="wappearance-reading" data-testid="c39-appearance-reading">
        <header><h3 id="wappearance-reading">对话阅读</h3><span>字号 · 密度 · 宽度</span></header>
        <p>仅影响会话正文阅读，不依赖浏览器整体缩放；偏好保存在本浏览器。</p>
        <ConversationReadingPrefsPanel />
      </section> : null}

      <section className="wappearance-card" aria-labelledby="wappearance-preview">
        <header><h3 id="wappearance-preview">{t("settingsHub.appearance.preview")}</h3><span>{mode === "light" ? t("settingsHub.appearance.lightMode") : t("settingsHub.appearance.darkMode")}</span></header>
        <div className="wappearance-preview" style={previewVars}>
          <div className="wp-brand"><MutekiLogo size={48} wordmark /><span>{lang === "zh" ? "品牌标识随主色变化" : "Brand follows the accent color"}</span></div>
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
            <span className="wp-primary" aria-hidden="true">＋ New Solve</span>
            <span className="wp-live"><i />LIVE · 3 workers</span>
          </div>
        </div>
      </section>
    </div>
  );
}

export function WorkerOrchestration({ defaultReturnTo = "/" }: { defaultReturnTo?: string }) {
  const solveOnly = useSolveOnlyMode();
  const [config, setConfig] = useState<WorkerSettings | null>(null);
  const [seats, setSeats] = useState<Seat[]>([]);
  const [credentials, setCredentials] = useState<Credential[]>([]);
  const [availableCredentials, setAvailableCredentials] = useState<GlobalCredential[]>([]);
  const [modelEndpoints, setModelEndpoints] = useState<ModelEndpoint[]>([]);
  const [models, setModels] = useState<WorkerModelOptions>({
    allow_custom: false,
    manual_models: {},
    discovered_models: {},
    models_by_profile: {},
    discovery: {},
    models: {},
  });
  const [health, setHealth] = useState<Record<string, ProfileHealth>>({});
  const [section, setSection] = useState<SettingsSection>("roster");
  const [mobileInspectorOpen, setMobileInspectorOpen] = useState(false);
  const [inspector, setInspector] = useState<"seat" | "review" | "verifier">("seat");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [review, setReview] = useState<ReviewPolicy>(DEFAULT_REVIEW);
  const [verifier, setVerifier] = useState<VerifierPolicy>(DEFAULT_VERIFIER);
  const [backend, setBackend] = useState<WorkerSettings["worker_backend"]>("local");
  const [network, setNetwork] = useState<WorkerSettings["worker_network"]>("bridge");
  const [effectiveNetwork, setEffectiveNetwork] = useState<string>("");
  const [networkError, setNetworkError] = useState<string>("");
  const [containerScope, setContainerScope] = useState<WorkerSettings["worker_container_scope"]>("run");
  const [workerPrivilege, setWorkerPrivilege] = useState<"default" | "elevated">("default");
  const [vpnEnabled, setVpnEnabled] = useState(false);
  const [vpnStatus, setVpnStatus] = useState<OpenVpnStatus>({ present: false });
  const [vpnUploading, setVpnUploading] = useState(false);
  const [imageStatus, setImageStatus] = useState<WorkerImageStatus | null>(null);
  const [imageLoading, setImageLoading] = useState(false);
  const [pullingImage, setPullingImage] = useState(false);
  const [raceTimeout, setRaceTimeout] = useState(300);
  const [dispatchMode, setDispatchMode] = useState<"fixed" | "auto">("fixed");
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
  const [feedback, setFeedback] = useState("");
  const [inContainer, setInContainer] = useState(false);
  const [reloadRevision, setReloadRevision] = useState(0);
  const baselineDraftRef = useRef("");

  const currentDraftSignature = useMemo(() => workerDraftSignature({
    seats,
    review,
    verifier,
    backend,
    network,
    containerScope,
    workerPrivilege,
    vpnEnabled,
    raceTimeout,
    dispatchMode,
    startWorkers,
    maxTotal,
    wallClock,
    costBudget,
    llmProfiles,
  }), [backend, containerScope, costBudget, dispatchMode, llmProfiles, maxTotal, network,
    raceTimeout, review, seats, startWorkers, verifier, vpnEnabled, wallClock, workerPrivilege]);
  const currentDraftSignatureRef = useRef(currentDraftSignature);
  currentDraftSignatureRef.current = currentDraftSignature;

  useEffect(() => {
    if (!config || !baselineDraftRef.current) return;
    setDirty(currentDraftSignature !== baselineDraftRef.current);
  }, [config, currentDraftSignature]);

  const returnTo = useMemo(() => {
    if (typeof window === "undefined") return defaultReturnTo;
    const value = new URLSearchParams(window.location.search).get("return") || defaultReturnTo;
    return value.startsWith("/") ? value : defaultReturnTo;
  }, [defaultReturnTo]);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const requested = (params.get("section") || params.get("tab") || window.location.hash.replace(/^#/, "")).trim();
    if (!requested) return;
    if (requested === "appearance") {
      window.location.replace("/settings/appearance");
      return;
    }
    if (isWorkerSection(requested)) setSection(requested);
  }, []);

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
        detail: result.detail || `${ENGINE_META[engine].label} 未返回当前凭据可用的模型。`,
      };
    } finally {
      setDiscoveringModels(false);
    }
  }, [availableCredentials, backend, showFeedback]);

  useEffect(() => {
    let alive = true;
    Promise.all([getWorkerSettings(), getGlobalCredentials(), getModelEndpoints(), getWorkerModelOptions(), fetchProfilesHealth(), getWorkerModelTestResults(), getWorkerImageStatus(), getOpenVpnStatus(), checkAuth()]).then(([cfg, globalRows, endpointRows, modelRows, healthRows, modelTestRows, workerImage, openVpn, authState]) => {
      if (!alive || !cfg) return;
      const identity = legacyIdentity(cfg);
      const ordinary = identity.seats.filter(isOrdinarySeat);
      const loadedReview = { ...DEFAULT_REVIEW, ...(cfg.stage_policy.coordinator.review || {}) };
      const loadedVerifier = { ...DEFAULT_VERIFIER, ...(cfg.stage_policy.coordinator.verifier || {}) };
      const loadedDispatchMode = cfg.stage_policy.coordinator.dispatch_mode === "auto" ? "auto" : "fixed";
      baselineDraftRef.current = workerDraftSignature({
        seats: identity.seats,
        review: loadedReview,
        verifier: loadedVerifier,
        backend: cfg.worker_backend || "local",
        network: cfg.worker_network || "bridge",
        containerScope: cfg.worker_container_scope || "run",
        workerPrivilege: cfg.worker_privilege === "elevated" ? "elevated" : "default",
        vpnEnabled: Boolean(cfg.worker_vpn_enabled),
        raceTimeout: cfg.race_timeout,
        dispatchMode: loadedDispatchMode,
        startWorkers: cfg.start_workers,
        maxTotal: cfg.max_total_workers,
        wallClock: cfg.wall_clock_budget,
        costBudget: cfg.cost_budget_usd,
        llmProfiles: cfg.llm_profiles,
      });
      setConfig(cfg);
      setSeats(identity.seats);
      setCredentials(syncCredentialsFromGlobal(identity.credentials, globalRows));
      setAvailableCredentials(globalRows);
      setModelEndpoints(endpointRows);
      setModels(modelRows);
      setHealth(Object.fromEntries(healthRows.map((item) => [item.profile_id, item])));
      setTestResults(modelTestRows);
      setSelectedId(ordinary[0]?.id || identity.seats[0]?.id || null);
      setReview(loadedReview);
      setVerifier(loadedVerifier);
      setBackend(cfg.worker_backend || "local");
      setNetwork(cfg.worker_network || "bridge");
      setContainerScope(cfg.worker_container_scope || "run");
      setWorkerPrivilege(cfg.worker_privilege === "elevated" ? "elevated" : "default");
      setEffectiveNetwork(cfg.effective_network || cfg.worker_network || "bridge");
      setNetworkError(cfg.network_error || "");
      setVpnEnabled(Boolean(cfg.worker_vpn_enabled));
      setVpnStatus(openVpn);
      setImageStatus(workerImage);
      setInContainer(authState.inContainer);
      setRaceTimeout(cfg.race_timeout);
      setDispatchMode(loadedDispatchMode);
      setStartWorkers(cfg.start_workers);
      setMaxTotal(cfg.max_total_workers);
      setWallClock(cfg.wall_clock_budget);
      setCostBudget(cfg.cost_budget_usd);
      setLlmProfiles(cfg.llm_profiles);
    });
    try { document.documentElement.dataset.theme = window.localStorage.getItem("muteki.theme") === "light" ? "light" : "dark"; } catch { document.documentElement.dataset.theme = "dark"; }
    return () => { alive = false; };
  }, [reloadRevision]);

  const ordinarySeats = seats.filter(isOrdinarySeat);
  const selectedSeat = seats.find((seat) => seat.id === selectedId) || null;
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
        setCredentials((current) => current.map((credential) => credential.id === existing.id ? syncCredentialFromGlobal(credential, [selected]) : credential));
      }
      return existing.id;
    }
    const next: Credential = {
      id: selected.id,
      label: selected?.label || (key === "__system__" ? `${ENGINE_META[engine].label} 系统登录` : key || "未配置模型服务"),
      engine,
      kind: key === "__system__" ? "system_inherit" : connection === "custom_endpoint" ? "custom_endpoint" : "engine_key",
      secret_ref: key === "__system__" ? "" : key,
      target_engine: connection === "custom_endpoint" ? engine : undefined,
      endpoint: connection === "custom_endpoint" ? { base_url: selected?.base_url || "", wire_api: ENGINE_META[engine].wireApi } : undefined,
    };
    setCredentials((current) => [...current, next]);
    return next.id;
  }, [availableCredentials, credentials]);

  const updateSeat = useCallback((id: string, patch: Partial<Seat>) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    if (patch.enabled === false && seat.enabled) {
      const dependencies = [
        review.enabled && review.engine === id ? "Review" : "",
        verifier.enabled && verifier.engine === id ? "Verifier" : "",
      ].filter(Boolean);
      if (dependencies.length) {
        showFeedback(
          `${seat.label} 正由 ${dependencies.join(" 和 ")} 使用，请先更换对应 Worker 或关闭该通道`,
        );
        return;
      }
      const isLastOrdinary = isOrdinarySeat(seat)
        && seats.filter((item) => isOrdinarySeat(item) && item.enabled).length <= 1;
      if (isLastOrdinary) {
        showFeedback("至少保留一个启用的普通 Worker");
        return;
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
    if (typeof patch.enabled === "boolean" && patch.enabled !== seat.enabled) {
      showFeedback(`${seat.label} 已暂存${patch.enabled ? "启用" : "停用"}，点击“保存配置”后生效`);
    }
  }, [markDirty, review.enabled, review.engine, seats, showFeedback, verifier.enabled, verifier.engine]);
  const bindAccount = useCallback((id: string, key: string, resolvedCredential?: GlobalCredential) => {
    const seat = seats.find((item) => item.id === id);
    if (!seat) return;
    const selected = resolvedCredential || availableCredentials.find((item) => globalCredentialKey(item) === key && item.engine === seat.engine);
    if (!globalCredentialUsable(selected)) {
      updateSeat(id, { credential_id: "", model: "" });
      showFeedback(`${ENGINE_META[engineOf(seat.engine)].label} 没有可用凭据，请在 Agent 凭据分区完成配置`);
      return;
    }
    const engine = selected ? engineOf(selected.engine) : engineOf(seat.engine);
    updateSeat(id, {
      engine,
      credential_id: ensureCredential(engine, key, selected || undefined),
      model: engine === seat.engine ? seat.model : "",
      reasoning_effort: engine === seat.engine ? seat.reasoning_effort || "default" : "default",
    });
  }, [availableCredentials, ensureCredential, seats, showFeedback, updateSeat]);

  const changeEngine = useCallback((id: string, engine: Engine) => {
    const matching = availableCredentials.find((credential) => credential.engine === engine && globalCredentialUsable(credential) && (credential.source === "stored" || backend === "local"));
    if (!matching) {
      updateSeat(id, { engine, credential_id: "", model: "", reasoning_effort: "default" });
      showFeedback(`${ENGINE_META[engine].label} 没有可用凭据，请在 Agent 凭据分区完成配置`);
      return;
    }
    updateSeat(id, { engine, credential_id: ensureCredential(engine, globalCredentialKey(matching), matching), model: matching.default_model || "", reasoning_effort: "default" });
  }, [availableCredentials, backend, ensureCredential, showFeedback, updateSeat]);

  const addSeat = useCallback((engine: Engine) => {
    const matching = availableCredentials.find((credential) => credential.engine === engine && globalCredentialUsable(credential) && (credential.source === "stored" || backend === "local"));
    if (!matching) {
      showFeedback(`${ENGINE_META[engine].label} 没有可用凭据；请先在 Agent 凭据分区完成配置，再添加 Worker`);
      return;
    }
    const id = randomId("seat", engine);
    const sameEngineCount = ordinarySeats.filter((seat) => seat.engine === engine).length;
    const next: Seat = { id, label: `${ENGINE_META[engine].label} Worker ${sameEngineCount + 1}`, engine, credential_id: ensureCredential(engine, globalCredentialKey(matching), matching), model: matching.default_model || "", reasoning_effort: "default", roles: [...ORDINARY_ROLES, "review"], race: true, capacity: { max_running: 1, max_review_running: 0 }, priority: ordinarySeats.length * 10 + 10, enabled: true };
    setSeats((current) => [...current, next]);
    setSelectedId(id);
    setInspector("seat");
    setMobileInspectorOpen(true);
    markDirty();
    showFeedback(`已添加 ${ENGINE_META[engine].label} Worker`);
  }, [availableCredentials, backend, ensureCredential, markDirty, ordinarySeats, showFeedback]);

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
  const toggleSeatEnabled = useCallback((id: string) => {
    const seat = seats.find((item) => item.id === id);
    if (seat) updateSeat(id, { enabled: !seat.enabled });
  }, [seats, updateSeat]);
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
    const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
    const connection = credential?.kind === "system_inherit"
      ? "system"
      : selectedGlobalCredential?.connection === "custom_endpoint" || credential?.kind === "custom_endpoint"
        ? "custom_endpoint"
        : "official";
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
  }, [availableCredentials, backend, credentials, showFeedback]);

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
        const selectedGlobalCredential = globalCredentialForLegacy(credential, availableCredentials);
        const connection = credential?.kind === "system_inherit"
          ? "system"
          : selectedGlobalCredential?.connection === "custom_endpoint" || credential?.kind === "custom_endpoint"
            ? "custom_endpoint"
            : "official";
        const accountId = credential?.kind === "system_inherit" ? "__system__" : credential?.secret_ref || "";
        const profile = buildModelTestProfile({
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
  }, [availableCredentials, backend, credentials, review.enabled, review.engine, verifier.enabled, verifier.engine, seats, showFeedback, testSeat]);

  const save = async () => {
    if (!config) return;
    const enabledOrdinary = seats.filter((seat) => isOrdinarySeat(seat) && seat.enabled);
    if (!enabledOrdinary.length) { setSaveState("error"); showFeedback("至少需要一个启用的普通 Worker"); return; }
    const invalidBinding = seats.find((seat) => {
      if (!seat.enabled) return false;
      const legacyCredential = credentials.find((credential) => credential.id === seat.credential_id);
      return !globalCredentialUsable(globalCredentialForLegacy(legacyCredential, availableCredentials));
    });
    if (invalidBinding) { setSaveState("error"); showFeedback(`${invalidBinding.label} 需要选择可用的全局凭据`); return; }
    if (review.enabled && !seats.some((seat) => seat.id === review.engine && seat.enabled && canServeChannel(seat, "review"))) { setSaveState("error"); showFeedback("Review 已启用，但没有指定可用 Worker"); return; }
    if (verifier.enabled && !seats.some((seat) => seat.id === verifier.engine && seat.enabled && canServeChannel(seat, "verifier"))) { setSaveState("error"); showFeedback("Verifier 已启用，但没有指定可用 Worker"); return; }
    const invalidLlm = (["planner", "titler"] as const).find((which) => {
      const profile = llmProfiles[which];
      const selectedEndpoint = modelEndpoints.find((endpoint) => endpoint.id === (profile.endpoint_id || profile.credential_id));
      return !selectedEndpoint?.present || !profile.model.trim();
    });
    if (invalidLlm) { setSaveState("error"); showFeedback(`${invalidLlm === "planner" ? "Reason / Planner" : "Titler"} 需要选择可用的模型端点和端点模型`); return; }
    const invalidTemperature = (["planner", "titler"] as const).find((which) => {
      const profile = llmProfiles[which];
      if (llmTemperatureMode(profile) !== "custom") return false;
      const value = Number(profile.temperature);
      return !Number.isFinite(value) || value < 0 || value > 2;
    });
    if (invalidTemperature) { setSaveState("error"); showFeedback(`${invalidTemperature === "planner" ? "Reason / Planner" : "Titler"} 的 Temperature 需在 0 到 2 之间`); return; }
    setSaveState("saving");
    const normalizedSeats = seats.map((seat, index) => ({
      ...seat,
      priority: (index + 1) * 10,
      race: isOrdinarySeat(seat) ? true : seat.race,
      roles: isOrdinarySeat(seat) ? Array.from(new Set([...seat.roles, "race"])) : [...seat.roles],
      capacity: { ...seat.capacity },
    }));
    const refs = enabledOrdinary.map((seat) => seat.id);
    const raceScout = dispatchMode === "fixed";
    const raceRefs = raceScout ? refs : [];
    let saved: WorkerSettings | null = null;
    try {
      saved = await putWorkerSettings({
      seats: normalizedSeats,
      engines: refs,
      race_engines: raceRefs,
      start_workers: Math.min(Math.max(1, startWorkers), Math.max(1, maxWorkers)),
      max_workers: Math.max(1, maxWorkers),
      worker_backend: backend,
      worker_network: network,
      worker_container_scope: containerScope,
      worker_privilege: workerPrivilege,
      worker_vpn_enabled: vpnEnabled,
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
          dispatch_mode: dispatchMode,
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
    const savedIdentity = legacyIdentity(saved);
    const savedDraft: WorkerDraftSnapshot = {
      seats: savedIdentity.seats,
      review: { ...DEFAULT_REVIEW, ...(saved.stage_policy.coordinator.review || {}) },
      verifier: { ...DEFAULT_VERIFIER, ...(saved.stage_policy.coordinator.verifier || {}) },
      backend: saved.worker_backend || "local",
      network: saved.worker_network || "bridge",
      containerScope: saved.worker_container_scope || "run",
      workerPrivilege: saved.worker_privilege === "elevated" ? "elevated" : "default",
      vpnEnabled: Boolean(saved.worker_vpn_enabled),
      raceTimeout: saved.race_timeout,
      dispatchMode: saved.stage_policy.coordinator.dispatch_mode === "auto" ? "auto" : "fixed",
      startWorkers: saved.start_workers,
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
      setCredentials(syncCredentialsFromGlobal(savedIdentity.credentials, availableCredentials));
      setReview(savedDraft.review);
      setVerifier(savedDraft.verifier);
      setBackend(savedDraft.backend);
      setNetwork(savedDraft.network);
      setEffectiveNetwork(saved.effective_network || savedDraft.network);
      setNetworkError(saved.network_error || "");
      setContainerScope(savedDraft.containerScope);
      setWorkerPrivilege(savedDraft.workerPrivilege);
      setVpnEnabled(savedDraft.vpnEnabled);
      setRaceTimeout(savedDraft.raceTimeout);
      setDispatchMode(savedDraft.dispatchMode);
      setStartWorkers(savedDraft.startWorkers);
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

  const navItems: { id: SettingsSection; label: string; icon: IconName }[] = [
    { id: "roster", label: "出战配置", icon: "grid" },
    { id: "credentials", label: "Agent 凭据", icon: "lock" },
    { id: "runtime", label: "运行环境", icon: "terminal" },
    { id: "scheduling", label: "调度与预算", icon: "gear" },
    { id: "models", label: "推理模型", icon: "sparkles" },
    { id: "system", label: "系统更新", icon: "refresh" },
  ];
  const refreshCredentials = (action?: "save" | "import" | "create" | "delete") => {
    void getGlobalCredentials().then((rows) => {
      setAvailableCredentials(rows);
      setCredentials((current) => syncCredentialsFromGlobal(current, rows));
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
    credentials: "Agent 凭据",
    runtime: "运行环境",
    scheduling: "调度与预算",
    models: "推理模型",
    system: "系统更新",
  };

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
    <div className="wsettings-page" data-section={section} onClickCapture={(event) => {
      if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
      const target = event.target;
      const link = target instanceof Element ? target.closest('a[href="/task/workers?section=credentials"]') : null;
      if (!link) return;
      event.preventDefault();
      selectSection("credentials");
    }}>
      <aside className="wsettings-nav">
        <div className="wsettings-nav-title"><span className="single-task-workers-mark"><Icon name="gear" size={18} /></span><strong>单题设置</strong></div>
        <nav aria-label="Worker 分区">{navItems.map((item) => (
          <Button
            key={item.id}
            type="button"
            className={section === item.id ? "on" : ""}
            aria-current={section === item.id ? "page" : undefined}
            onClick={() => selectSection(item.id)}
          >
            <Icon name={item.icon} size={17} /><span>{item.label}</span>
            {item.id === "roster" ? <span className="wsettings-nav-count">{seats.filter(isOrdinarySeat).length}</span> : null}
          </Button>
        ))}</nav>
        <div className="wsettings-nav-foot"><span>保存后用于下次任务</span>{solveOnly ? <a href="/settings/appearance"><Icon name="gear" size={14} />工作区模式</a> : null}<a href={returnTo}><Icon name="chevronRight" size={14} />返回单题</a></div>
      </aside>

      <main className="wsettings-main">
        <header className="wsettings-topbar"><div><h1>{titles[section]}</h1></div>{section === "system" ? <Chip size="sm" variant="soft" color="default" className="wsettings-draft">稳定通道</Chip> : section === "credentials" ? null : <><Chip size="sm" variant="soft" color={dirty ? "warning" : "default"} className={`wsettings-draft${dirty ? " dirty" : ""}`}>{dirty ? "未保存" : "已保存"}</Chip><div className="wsettings-top-actions"><Button type="button" size="sm" variant="primary" onClick={save} isDisabled={!dirty || saveState === "saving"}><Icon name="check" size={14} />{saveState === "saving" ? "保存中…" : "保存配置"}</Button></div></>}</header>

        <div className="wsettings-content">
          {section === "roster" ? <div className="wsettings-orchestration" data-inspector-open={mobileInspectorOpen}>
            <RosterWorkspace seats={seats} credentials={credentials} backend={backend} testingIds={testingIds} testResults={testResults} batchCheck={batchCheck} selectedId={inspector === "seat" ? selectedId : null} selectedInspector={inspector} review={review} verifier={verifier} onSelect={(id) => { setSelectedId(id); setInspector("seat"); setMobileInspectorOpen(true); }} onSelectReview={() => { setInspector("review"); setMobileInspectorOpen(true); }} onSelectVerifier={() => { setInspector("verifier"); setMobileInspectorOpen(true); }} onAdd={addSeat} onReorder={reorderSeats} onDuplicate={duplicateSeat} onToggleEnabled={toggleSeatEnabled} onSetReview={setReviewSeat} onSetVerifier={setVerifierSeat} onTestAll={() => void testAllSeats()} onTest={(seat) => void testSeat(seat)} onDelete={deleteSeat} />
            <div className="wsettings-detail">
              <Button type="button" variant="ghost" className="wsettings-detail-back" onPress={() => setMobileInspectorOpen(false)}><Icon name="chevronLeft" size={15} />Worker 列表</Button>
            {inspector === "review" ? <ReviewInspector seats={seats} credentials={credentials} availableCredentials={availableCredentials} models={models} backend={backend} review={review} onReview={updateReview} onCreateDedicated={createDedicatedReview} onSeatUpdate={updateSeat} onSeatEngine={changeEngine} onSeatAccount={bindAccount} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onEditOrdinary={(id) => { setSelectedId(id); setInspector("seat"); }} onTest={() => { const seat = seats.find((item) => item.id === review.engine); if (seat) void testSeat({ ...seat, reasoning_effort: review.reasoning_effort && review.reasoning_effort !== "inherit" ? review.reasoning_effort : seat.reasoning_effort || "default" }); }} testing={testingIds.has(review.engine || "")} testResult={review.engine ? testResults[review.engine] || null : null} />
              : inspector === "verifier" ? <VerifierInspector seats={seats} credentials={credentials} availableCredentials={availableCredentials} models={models} backend={backend} verifier={verifier} onVerifier={updateVerifier} onCreateDedicated={createDedicatedVerifier} onSeatUpdate={updateSeat} onSeatEngine={changeEngine} onSeatAccount={bindAccount} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onEditOrdinary={(id) => { setSelectedId(id); setInspector("seat"); }} onTest={() => { const seat = seats.find((item) => item.id === verifier.engine); if (seat) void testSeat({ ...seat, reasoning_effort: verifier.reasoning_effort && verifier.reasoning_effort !== "inherit" ? verifier.reasoning_effort : seat.reasoning_effort || "default" }); }} testing={testingIds.has(verifier.engine || "")} testResult={verifier.engine ? testResults[verifier.engine] || null : null} />
                : <SeatInspector seat={selectedSeat && isOrdinarySeat(selectedSeat) ? selectedSeat : null} credentials={credentials} availableCredentials={availableCredentials} models={models} backend={backend} health={selectedSeat ? health[selectedSeat.id] : undefined} testing={selectedSeat ? testingIds.has(selectedSeat.id) : false} testResult={selectedSeat ? testResults[selectedSeat.id] || null : null} onUpdate={(patch) => selectedSeat && updateSeat(selectedSeat.id, patch)} onEngine={(engine) => selectedSeat && changeEngine(selectedSeat.id, engine)} onAccount={(key, globalCredential) => selectedSeat && bindAccount(selectedSeat.id, key, globalCredential)} onDiscoverModels={refreshWorkerModels} discoveringModels={discoveringModels} onDuplicate={() => selectedSeat && duplicateSeat(selectedSeat.id)} onDelete={() => selectedSeat && deleteSeat(selectedSeat.id)} onTest={() => selectedSeat && void testSeat(selectedSeat)} />}
            </div>
          </div>
            : section === "credentials" ? <div className="wsettings-credentials"><TaskCredentialManager taskSettings onCredentialsChanged={refreshCredentials} /></div>
            : section === "runtime" ? <RuntimeWorkspace backend={backend} network={network} effectiveNetwork={effectiveNetwork} networkError={networkError} containerScope={containerScope} workerPrivilege={workerPrivilege} vpnEnabled={vpnEnabled} vpnStatus={vpnStatus} vpnUploading={vpnUploading} seatCount={seats.length} imageStatus={imageStatus} imageLoading={imageLoading} pulling={pullingImage} inContainer={inContainer} onBackend={(next) => { setBackend(next); setTestResults({}); markDirty(); if (next === "container" && !imageStatus) void refreshImage(); }} onNetwork={(next) => { setNetwork(next); setTestResults({}); markDirty(); }} onContainerScope={(next) => { setContainerScope(next); markDirty(); }} onWorkerPrivilege={(next) => { setWorkerPrivilege(next); markDirty(); }} onVpnEnabled={(next) => { setVpnEnabled(next); markDirty(); }} onVpnUpload={(file) => { setVpnUploading(true); void uploadOpenVpnConfig(file).then((status) => { setVpnStatus(status); setVpnEnabled(true); markDirty(); showFeedback("OpenVPN 配置已上传"); }).catch((error) => showFeedback(`上传失败：${error instanceof Error ? error.message : String(error)}`)).finally(() => setVpnUploading(false)); }} onRefreshImage={() => void refreshImage()} onPullImage={() => void pullImage()} />
                : section === "scheduling" ? <SchedulingWorkspace config={config} raceTimeout={raceTimeout} dispatchMode={dispatchMode} startWorkers={startWorkers} maxTotal={maxTotal} wallClock={wallClock} costBudget={costBudget} maxWorkers={maxWorkers} onChange={(patch) => { if (patch.raceTimeout !== undefined) setRaceTimeout(patch.raceTimeout); if (patch.dispatchMode !== undefined) setDispatchMode(patch.dispatchMode); if (patch.startWorkers !== undefined) setStartWorkers(patch.startWorkers); if (patch.maxTotal !== undefined) setMaxTotal(patch.maxTotal); if (patch.wallClock !== undefined) setWallClock(patch.wallClock); if (patch.costBudget !== undefined) setCostBudget(patch.costBudget); markDirty(); }} />
                  : section === "models" ? <ModelsWorkspace value={llmProfiles} endpoints={modelEndpoints} onChange={(next) => { setLlmProfiles(next); markDirty(); }} onEndpointSaved={(endpoint) => setModelEndpoints((current) => [...current.filter((item) => item.id !== endpoint.id), endpoint])} />
                    : <PlatformUpdate />}
        </div>
        {feedback ? <div className="wsettings-feedback"><Icon name={saveState === "error" ? "alert" : "check"} size={14} />{feedback}</div> : null}
      </main>
    </div>
  );
}
