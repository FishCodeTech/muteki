
import {
  adapterDescriptor, descriptorForEngine, isKnownAdapter, isKnownEngine, loginGuidance as descriptorLoginGuidance, runtimeScopesModelCatalog,
  type ProviderDescriptorCatalog,
} from "./providerDescriptors";

export const GUIDE_DISMISSED_STORAGE_KEY = "muteki:conversation-readiness:guide-dismissed";
export const RECENT_PATHS_STORAGE_KEY = "muteki:conversation-readiness:recent-paths";

export type SourcePhase = "loading" | "ok" | "error";

export interface SourceState {
  phase: SourcePhase;
  httpStatus?: number;
  message?: string;
}

export type BlockerKind =
  | "connecting"
  | "config_read_failed"
  | "not_installed"
  | "version_unsupported"
  | "discovery_unavailable"
  | "not_logged_in"
  | "model_unavailable"
  | "directory_inaccessible"
  | "runtime_unhealthy";

export type RecoveryAction =
  | "retry"
  | "open_agents"
  | "probe"
  | "pick_directory"
  | "enter_path";

export type ReadinessGuide = "hidden" | "compact" | "first_run";

export type ReadinessSource = "credentials" | "runtimes" | "projects" | "descriptors" | "directory" | "selection";

export interface ReadinessBlocker {
  kind: BlockerKind;
  message: string;
  recovery: RecoveryAction;
  source?: ReadinessSource;
  loginCommand?: string;
  loginNote?: string;
  engine?: string;
}

export interface ConversationReadinessSelected {
  credentialId: string;
  runtimeKey: string;
  projectId: string;
  modelId: string;
}

export interface ConversationReadiness {
  sources: {
    credentials: SourceState;
    runtimes: SourceState;
    projects: SourceState;
    descriptors: SourceState;
  };
  selected: ConversationReadinessSelected;
  blockers: ReadinessBlocker[];
  canSend: boolean;
  guide: ReadinessGuide;
  envReady: boolean;
  agentVerified: boolean;
  directorySelected: boolean;
}

export interface ReadinessCredential {
  id: string;
  engine: string;
  source?: string;
  present?: boolean | null;
  detection_status?: "present" | "absent" | "unknown" | "disabled";
  status: string;
  status_detail?: string;
  discovery_code?: string;
  models?: Array<{ id: string }>;
  candidate_models?: Array<{ id: string }>;
  default_model?: string;
}

export interface ReadinessRuntimeAuth {
  code?: string;
  status?: string;
  detail?: string;
  login_command?: string;
  note?: string;
}

export interface ReadinessRuntimeHealth {
  healthy?: boolean;
  capabilities?: Record<string, unknown>;
  detail?: string;
  auth?: ReadinessRuntimeAuth;
  binary_path?: string;
  degradations?: string[];
  source?: string;
  probed_at?: string;
  version_check?: {
    installed_version?: string;
    baseline_version?: string;
    min_supported_version?: string;
    meets_min_version?: boolean;
    below_minimum?: boolean;
    policy_enforced?: boolean;
    policy_modes?: { ctf?: boolean; pentest?: boolean; src?: boolean };
    upgrade_command?: string;
    upgrade_note?: string;
  };
}

export interface ReadinessRuntime {
  key: string;
  origin?: string;
  engine?: string;
  enabled?: boolean;
  adapter_id?: string;
  default_for_engine?: boolean;
  health?: ReadinessRuntimeHealth;
  auth?: ReadinessRuntimeAuth;
  binary_path?: string;
}

export interface ReadinessProject {
  project_id: string;
  name?: string;
  root_path?: string;
}

const UNAVAILABLE_STATUSES = new Set([
  "missing", "absent", "failed", "invalid", "error", "unavailable",
]);

export function parseHttpStatus(message: string): number | undefined {
  const match = String(message || "").match(/HTTP\s+(\d{3}|network)/i);
  if (!match) return undefined;
  if (match[1].toLowerCase() === "network") return 0;
  const status = Number(match[1]);
  return Number.isFinite(status) ? status : undefined;
}


export function isConnectionSourceFailure(source: SourceState): boolean {
  if (source.phase !== "error") return false;
  const status = source.httpStatus;
  if (status === 0 || status === 502) return true;
  const msg = String(source.message || "").toLowerCase();
  return (
    msg.includes("failed to fetch")
    || msg.includes("networkerror")
    || msg.includes("network request failed")
    || msg.includes("api proxy failed")
    || msg.includes("err_connection")
    || msg.includes("econnrefused")
    || /http\s*network/i.test(msg)
    || /http\s*502\b/.test(msg)
  );
}

export function sourceError(message: string, httpStatus?: number): SourceState {
  return {
    phase: "error",
    message,
    httpStatus: httpStatus ?? parseHttpStatus(message),
  };
}

export function sourceOk(): SourceState {
  return { phase: "ok" };
}

export function sourceLoading(): SourceState {
  return { phase: "loading" };
}

export function credentialAvailable(credential: ReadinessCredential | undefined): boolean {
  if (!credential) return false;
  if (credential.present === false) return false;
  return !UNAVAILABLE_STATUSES.has(String(credential.status || "").toLowerCase());
}

export function credentialLoginMissing(credential: ReadinessCredential | undefined): boolean {
  if (!credential || credential.discovery_code === "host_discovery_disabled") return false;
  if (credential.detection_status === "unknown" || credential.detection_status === "disabled") return false;
  const status = String(credential.status || "").toLowerCase();
  return credential.present === false || status === "missing" || status === "absent";
}

export function allModelIds(credential: ReadinessCredential | undefined): string[] {
  if (!credential) return [];
  const ids: string[] = [];
  const seen = new Set<string>();
  for (const row of [...(credential.models || []), ...(credential.candidate_models || [])]) {
    const id = String(row?.id || "").trim();
    if (!id || seen.has(id)) continue;
    seen.add(id);
    ids.push(id);
  }
  return ids;
}

export function isRuntimeUnprobed(runtime: ReadinessRuntime | undefined): boolean {
  if (!runtime || runtime.enabled === false) return false;
  const health = runtime.health;
  if (!health) return true;
  if (health.probed_at) return false;
  if (typeof health.healthy === "boolean") return false;
  if (health.detail || health.binary_path || health.source) return false;
  return true;
}

export function runtimeCliMissing(runtime: ReadinessRuntime | undefined): boolean {
  if (!runtime) return false;
  const health = runtime.health;
  if (!health) return false;
  const source = String(health.source || "");
  if (source === "probe_timeout" || source === "probe_error") return false;
  const blob = `${health.detail || ""} ${(health.degradations || []).join(" ")}`.toLowerCase();
  if (/cli_missing|binary 解析失败|not found|no such file|不可运行/.test(blob)) return true;
  const binary = String(health.binary_path || runtime.binary_path || "").trim();
  if (health.healthy === false && !binary && (source === "cli_driver" || source === "registry")) {
    return true;
  }
  return false;
}

export function runtimeAuthMissing(runtime: ReadinessRuntime | undefined): boolean {
  const auth = runtime?.health?.auth || runtime?.auth;
  const status = String(auth?.status || "").toLowerCase();
  return status === "missing" || status === "absent";
}

export function runtimeHealthy(runtime: ReadinessRuntime | undefined): boolean {
  return Boolean(
    runtime
    && runtime.enabled !== false
    && runtime.health?.healthy === true,
  );
}

export function runtimeAvailableForSelection(
  runtime: ReadinessRuntime,
  descriptors?: ProviderDescriptorCatalog | null,
): boolean {
  if (runtime.enabled === false || runtime.health?.healthy === false) return false;
  const defaultAdapter = descriptorForEngine(descriptors ?? null, runtime.engine)?.identity.default_adapter_id;
  const defaultTransport = adapterDescriptor(descriptors ?? null, defaultAdapter)?.transport_kind;
  const adapter = adapterDescriptor(descriptors ?? null, runtime.adapter_id);
  return !defaultTransport || defaultTransport === "cli" || Boolean(adapter && adapter.role !== "cli");
}

export function pickRuntimeForEngine(
  engine: string,
  runtimes: ReadinessRuntime[],
): ReadinessRuntime | undefined {
  const rows = runtimes.filter((runtime) => (
    runtime.engine === engine && runtime.enabled !== false
  ));
  const healthy = rows.filter((runtime) => runtime.health?.healthy === true);
  const pool = healthy.length ? healthy : rows;
  return [...pool].sort((left, right) => {
    const score = (runtime: ReadinessRuntime) => (
      Number(runtimeHealthy(runtime)) * 8
      + Number(Boolean(runtime.default_for_engine)) * 4
      + Number(Boolean(runtime.health?.capabilities?.approval)) * 2
      + Number(runtime.adapter_id && !String(runtime.adapter_id).startsWith("cli."))
    );
    return score(right) - score(left);
  })[0];
}

export function isDirectoryPickerUnavailable(error: unknown): boolean {
  const record = error && typeof error === "object" ? error as {
    code?: string;
    httpStatus?: number;
    message?: string;
  } : {};
  const message = error instanceof Error ? error.message : String(record.message || error || "");
  const code = String(record.code || "");
  return record.httpStatus === 503
    || code.includes("directory_picker")
    || message.includes("directory_picker.unavailable")
    || message.includes("没有可用的目录选择器");
}

export function isDirectoryInaccessibleMessage(message: string): boolean {
  const text = String(message || "");
  return /目录不存在|directory\.inaccessible|directory_inaccessible|不是目录|已经不存在/.test(text);
}

function loginGuidance(
  engine: string,
  runtime: ReadinessRuntime | undefined,
  descriptors: ProviderDescriptorCatalog | null,
): { command: string; note: string } {
  const auth = runtime?.health?.auth || runtime?.auth;
  const mapped = descriptorLoginGuidance(descriptors, engine);
  return {
    command: String(auth?.login_command || mapped.command || "").trim(),
    note: String(auth?.note || mapped.note || "").trim(),
  };
}

export function blockerBlocksSend(blocker: ReadinessBlocker): boolean {
  if (blocker.kind === "directory_inaccessible") return false;
  if (blocker.kind === "connecting") return false;
  if (blocker.kind === "config_read_failed" && blocker.source === "projects") return false;
  return true;
}

export function primarySendBlock(readiness: ConversationReadiness): ReadinessBlocker | null {
  return readiness.blockers.find(blockerBlocksSend) ?? null;
}

export interface ClassifyConversationReadinessInput {
  sources: ConversationReadiness["sources"];
  credentials: ReadinessCredential[];
  runtimes: ReadinessRuntime[];
  projects?: ReadinessProject[];
  descriptors: ProviderDescriptorCatalog | null;
  selected: ConversationReadinessSelected;
  probing?: boolean;
  directoryIssue?: { message: string } | null;
  guideDismissed?: boolean;
  hasThread?: boolean;
  policyMode?: "ctf" | "pentest" | "src";
}

export function classifyConversationReadiness(
  input: ClassifyConversationReadinessInput,
): ConversationReadiness {
  const sources = input.sources;
  const descriptors = input.descriptors;
  const credentials = input.credentials.filter((row) => isKnownEngine(descriptors, row.engine));
  const runtimes = input.runtimes.filter((row) => (
    row.enabled !== false && (row.origin === "extension" || isKnownAdapter(descriptors, row.adapter_id))
  ));
  const selected = { ...input.selected };
  const blockers: ReadinessBlocker[] = [];

  const connectionSources = (
    ["credentials", "runtimes", "projects", "descriptors"] as const
  ).filter((key) => isConnectionSourceFailure(sources[key]));
  const erroredSources = (
    ["credentials", "runtimes", "projects", "descriptors"] as const
  ).filter((key) => sources[key].phase === "error");
  const siteDown = connectionSources.length >= 2
    && connectionSources.length === erroredSources.length;
  if (siteDown) {
    blockers.push({
      kind: "config_read_failed",
      source: connectionSources[0],
      recovery: "retry",
      message: "连接已断开，草稿已保留，可重试",
    });
  } else {
    if (sources.credentials.phase === "error") {
      blockers.push({
        kind: "config_read_failed",
        source: "credentials",
        recovery: "retry",
        message: `凭据列表加载失败：${sources.credentials.message || "未知错误"}`,
      });
    }
    if (sources.runtimes.phase === "error") {
      blockers.push({
        kind: "config_read_failed",
        source: "runtimes",
        recovery: "retry",
        message: `Runtime 列表加载失败：${sources.runtimes.message || "未知错误"}`,
      });
    }
    if (sources.projects.phase === "error") {
      blockers.push({
        kind: "config_read_failed",
        source: "projects",
        recovery: "retry",
        message: `项目列表加载失败：${sources.projects.message || "未知错误"}`,
      });
    }
    if (sources.descriptors.phase === "error") {
      blockers.push({
        kind: "config_read_failed",
        source: "descriptors",
        recovery: "retry",
        message: `引擎描述加载失败：${sources.descriptors.message || "未知错误"}`,
      });
    }
  }

  if (input.probing) {
    blockers.push({
      kind: "connecting",
      source: "selection",
      recovery: "probe",
      message: "正在验证 Agent…",
    });
  } else if (
    sources.credentials.phase === "loading"
    || sources.runtimes.phase === "loading"
    || sources.descriptors.phase === "loading"
  ) {
    blockers.push({
      kind: "connecting",
      source: "credentials",
      recovery: "retry",
      message: "正在连接…",
    });
  }

  const selectedExtension = runtimes.find((row) => row.key === selected.runtimeKey && row.origin === "extension");
  const credential = selectedExtension ? undefined : (
    credentials.find((row) => row.id === selected.credentialId)
    || credentials.find(credentialAvailable)
    || credentials[0]
  );
  const runtime = selectedExtension || runtimes.find((row) => row.key === selected.runtimeKey)
    || (credential ? pickRuntimeForEngine(credential.engine, runtimes) : undefined);
  const engine = credential?.engine || runtime?.engine || "";
  const modelIds = allModelIds(credential);
  const engineRuntime = runtime || (credential
    ? pickRuntimeForEngine(credential.engine, runtimes)
    : undefined);

  if (selectedExtension && sources.runtimes.phase === "ok") {
    if (runtime && runtime.health?.healthy === false) {
      blockers.push({ kind: "runtime_unhealthy", source: "selection", recovery: "probe", engine,
        message: `${runtime.adapter_id || "Agent"} 接入异常：${runtime.health?.detail || "健康检查未通过"}` });
    } else if (runtime && isRuntimeUnprobed(runtime) && !input.probing) {
      blockers.push({ kind: "connecting", source: "selection", recovery: "probe", engine,
        message: "Agent 尚未验证" });
    }
  } else if (sources.credentials.phase === "ok" && sources.runtimes.phase === "ok" && descriptors) {
    const missingRuntime = Boolean(credential && !engineRuntime);
    if (runtimeCliMissing(engineRuntime) || missingRuntime) {
      blockers.push({
        kind: "not_installed",
        source: "selection",
        recovery: "open_agents",
        engine,
        message: engine
          ? `${engine} CLI 未安装或无法运行`
          : "未检测到可用的 Agent CLI",
      });
    }

    const discoveryDisabled = credential?.discovery_code === "host_discovery_disabled"
      || ((runtime?.health?.auth || runtime?.auth)?.code === "host_discovery_disabled"
        && credential?.source !== "stored");
    if (discoveryDisabled) {
      blockers.push({ kind: "discovery_unavailable", source: "selection", recovery: "open_agents", engine,
        message: "此服务已关闭宿主登录发现，尚未检测当前接入点的登录状态。请在 Agents 中手动配置并选择已登记凭据；桌面客户端的登录文件不会自动同步至服务。" });
    }
    const alreadyInstalled = blockers.some((row) => row.kind === "not_installed");
    const verCheck = engineRuntime?.health?.version_check;
    if (
      !alreadyInstalled
      && input.policyMode
      && verCheck?.below_minimum
      && verCheck.policy_enforced !== false
      && verCheck.policy_modes?.[input.policyMode] !== false
    ) {
      const modeLabel = input.policyMode === "src" ? "SRC 模式" : input.policyMode === "pentest" ? "Pentest 模式" : "CTF 模式";
      blockers.push({
        kind: "version_unsupported",
        source: "selection",
        recovery: "open_agents",
        engine,
        loginCommand: verCheck.upgrade_command,
        loginNote: verCheck.upgrade_note,
        message: `${engine || "Agent"} 当前版本 ${verCheck.installed_version || "未知"} 低于最低支持版本 ${verCheck.min_supported_version || "基准"}，${modeLabel}禁止使用或设为出战 Worker。建议先升级 CLI 后重试。`,
      });
    }
    if (!alreadyInstalled && !discoveryDisabled && (credentialLoginMissing(credential) || runtimeAuthMissing(runtime))) {
      const guidance = loginGuidance(engine, runtime, descriptors);
      const commandLine = guidance.command
        ? `在宿主终端执行 ${guidance.command}，完成后返回此页将自动检查`
        : "请在宿主终端完成 CLI 登录，完成后返回此页将自动检查";
      blockers.push({
        kind: "not_logged_in",
        source: "selection",
        recovery: "open_agents",
        engine,
        loginCommand: guidance.command,
        loginNote: guidance.note,
        message: engine ? `${engine} 未登录。${commandLine}` : `未登录。${commandLine}`,
      });
    }

    if (
      !alreadyInstalled
      && !discoveryDisabled
      && !blockers.some((row) => row.kind === "not_logged_in")
    ) {
      if (!credentials.length) {
        blockers.push({
          kind: "model_unavailable",
          source: "selection",
          recovery: "open_agents",
          message: "未发现可用 Agent，请前往设置 → Agents 配置",
        });
      } else if (credential && credentialAvailable(credential) && !modelIds.length) {
        const scopedCatalog = runtimeScopesModelCatalog(descriptors, runtime?.key);
        blockers.push({
          kind: "model_unavailable",
          source: "selection",
          recovery: scopedCatalog ? "probe" : "open_agents",
          engine,
          message: scopedCatalog
            ? "尚未读取当前凭据的原生模型目录，请验证 Agent 后选择模型"
            : "当前接入点没有可用模型，请前往 Agents 补充模型或测连通",
        });
      } else if (runtime && !isRuntimeUnprobed(runtime) && runtime.health?.healthy === false) {
        blockers.push({
          kind: "runtime_unhealthy",
          source: "selection",
          recovery: "probe",
          engine,
          message: `${engine || "Agent"} 接入异常：${runtime.health?.detail || "健康检查未通过"}`,
        });
      } else if (runtime && isRuntimeUnprobed(runtime) && !input.probing) {
        blockers.push({
          kind: "connecting",
          source: "selection",
          recovery: "probe",
          engine,
          message: "Agent 尚未验证",
        });
      }
    }
  }

  if (input.directoryIssue?.message) {
    blockers.push({
      kind: "directory_inaccessible",
      source: "directory",
      recovery: "enter_path",
      message: input.directoryIssue.message,
    });
  }

  const envReady = selectedExtension ? Boolean(sources.runtimes.phase === "ok" && runtimeHealthy(runtime)) : Boolean(
    sources.credentials.phase === "ok"
    && descriptors
    && credential
    && credentialAvailable(credential)
    && modelIds.length,
  );
  const agentVerified = Boolean(runtimeHealthy(runtime));
  const directorySelected = Boolean(selected.projectId);
  const canSend = envReady
    && !blockers.some(blockerBlocksSend)
    && Boolean(runtime)
    && (agentVerified || isRuntimeUnprobed(runtime));

  let guide: ReadinessGuide = "hidden";
  if (input.guideDismissed) {
    guide = blockers.length ? "compact" : "hidden";
  } else if (input.hasThread) {
    guide = blockers.some(blockerBlocksSend) || blockers.some((row) => row.kind === "config_read_failed")
      ? "compact"
      : "hidden";
  } else if (canSend && directorySelected && !blockers.length) {
    guide = "hidden";
  } else {
    guide = "first_run";
  }

  return {
    sources,
    selected,
    blockers,
    canSend,
    guide,
    envReady,
    agentVerified,
    directorySelected,
  };
}
