import type { GlobalCredential, WorkerModelOptions, WorkerModelTestResult, WorkerSettings } from "@/lib/useRun";
import {
  descriptorForEngine, providerWorkerEngines,
  type ProviderDescriptorCatalog, type ProviderWorkerProfileSpec,
} from "@/lib/providerDescriptors";

export type Seat = NonNullable<WorkerSettings["seats"]>[number];
export type NewWorkerOptions = Pick<Seat, "label" | "model" | "reasoning_effort"> & { max_running: number };
export type Credential = NonNullable<WorkerSettings["credentials"]>[number];
export type ReviewPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["review"]>;
export type VerifierPolicy = NonNullable<WorkerSettings["stage_policy"]["coordinator"]["verifier"]>;
export type Engine = string;
export type Catalog = ProviderDescriptorCatalog | null;
export type ModelOptionList = WorkerModelOptions["models"][string];
export type ModelDiscoveryOutcome = { ok: boolean; detail: string };
export type BatchCheckState = { running: boolean; completed: number; total: number };

export const ORDINARY_ROLES = ["bootstrap", "explore", "respond"];
export const EFFORT_LABELS: Record<string, string> = {
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

export function workerEngines(catalog: Catalog): Engine[] {
  return providerWorkerEngines(catalog);
}

export function workerProfileSpec(catalog: Catalog, engine: Engine): ProviderWorkerProfileSpec | undefined {
  return descriptorForEngine(catalog, engine)?.worker;
}

export function engineProtocolLabel(catalog: Catalog, engine: Engine): string {
  return workerProfileSpec(catalog, engine)?.protocol_label ?? "";
}

export function engineLocalOnly(catalog: Catalog, engine: Engine): boolean {
  return descriptorForEngine(catalog, engine)?.login.system_login_only === true;
}

export function engineEffortLevels(catalog: Catalog, engine: Engine): string[] {
  return descriptorForEngine(catalog, engine)?.cli.reasoning_efforts ?? [];
}

export function engineOf(value: string): Engine {
  return String(value || "").trim().toLocaleLowerCase("en-US") || "claude";
}

export function effortDefinition(
  engineLevels: string[],
  model: string,
  options: ModelOptionList,
): { levels: string[]; defaultLevel: string; supported: boolean } {
  const selected = options.find((item) => item.id === model);
  if (selected?.reasoning) {
    const levels = selected.reasoning.levels.filter((level) => engineLevels.includes(level));
    return {
      levels,
      defaultLevel: selected.reasoning.default || "",
      supported: selected.reasoning.supported && levels.length > 0,
    };
  }
  return { levels: engineLevels, defaultLevel: "", supported: engineLevels.length > 0 };
}

export function normalizeEffortForModel(
  value: string | undefined,
  engineLevels: string[],
  model: string,
  options: ModelOptionList,
): string {
  const effort = String(value || "default").toLocaleLowerCase("en-US");
  const definition = effortDefinition(engineLevels, model, options);
  return effort === "default" || definition.levels.includes(effort) ? effort : "default";
}

export function isOrdinarySeat(seat: Seat): boolean {
  return seat.roles.some((role) => ORDINARY_ROLES.includes(role) || role === "solve" || role === "scout");
}

export function canServeChannel(seat: Seat, role: "review" | "verifier"): boolean {
  return isOrdinarySeat(seat) || seat.roles.includes(role);
}

export function credentialKey(credential?: Credential): string {
  if (!credential) return "";
  return credential.kind === "system_inherit" ? "__system__" : credential.secret_ref;
}

export function globalCredentialKey(credential: GlobalCredential): string {
  return credential.source === "system"
    ? "__system__"
    : credential.account_id || credential.id.replace(/^account:/, "");
}

const UNUSABLE_GLOBAL_CREDENTIAL_STATUSES = new Set([
  "missing", "absent", "failed", "invalid", "error", "unavailable",
]);

export function globalCredentialUsable(
  credential: GlobalCredential | null | undefined,
): credential is GlobalCredential {
  return Boolean(
    credential?.present
    && !UNUSABLE_GLOBAL_CREDENTIAL_STATUSES.has(
      String(credential.status || "").toLocaleLowerCase("en-US"),
    ),
  );
}

export function globalCredentialForLegacy(
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

export function findGlobalCredential(
  availableCredentials: GlobalCredential[],
  engine: Engine,
  accountKey: string,
): GlobalCredential | null {
  return availableCredentials.find((item) => (
    item.engine === engine && (
      (accountKey === "__system__" && item.source === "system")
      || (item.source === "stored" && item.account_id === accountKey)
    )
  )) || null;
}

export function credentialsForEngine(
  availableCredentials: GlobalCredential[],
  engine: Engine,
  backend: WorkerSettings["worker_backend"],
): GlobalCredential[] {
  return availableCredentials.filter((item) => (
    item.engine === engine && (item.source === "stored" || backend === "local")
  ));
}

export function credentialKindLabel(credential: GlobalCredential): string {
  return credential.source === "system" ? "宿主登录" : credential.connection === "custom_endpoint" ? "自定义端点" : "官方账号";
}

export type ModelEntry = { id: string; source: string };

export function modelDirectory(credential: GlobalCredential | null, current: string): ModelEntry[] {
  const byId = new Map<string, ModelEntry>();
  byId.set("", { id: "", source: "Runtime 默认模型" });
  if (credential?.default_model) byId.set(credential.default_model, { id: credential.default_model, source: "凭据默认" });
  for (const id of [...(credential?.candidate_models || []), ...(credential?.models || [])]) {
    if (!byId.has(id)) byId.set(id, { id, source: "端点目录" });
  }
  if (current && !byId.has(current)) byId.set(current, { id: current, source: "当前已保存" });
  return [...byId.values()];
}

export type SeatCheckState = "off" | "checking" | "ok" | "bad" | "idle";

export function seatCheckState(enabled: boolean, testing: boolean, result?: WorkerModelTestResult): SeatCheckState {
  if (!enabled) return "off";
  if (testing) return "checking";
  if (result) return result.ok ? "ok" : "bad";
  return "idle";
}

export const CHECK_LABEL: Record<SeatCheckState, string> = {
  off: "已停用",
  checking: "自检中",
  ok: "自检通过",
  bad: "自检失败",
  idle: "待自检",
};

export function checkDetail(enabled: boolean, testing: boolean, result?: WorkerModelTestResult): string {
  if (testing) return "正在发起真实模型请求";
  if (result) {
    const elapsed = result.elapsed_ms ? `${(result.elapsed_ms / 1000).toFixed(1)}s` : "";
    const testedAt = result.tested_at
      ? new Date(result.tested_at * 1000).toLocaleString("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" })
      : "";
    return [result.detail || (result.ok ? "真实模型请求完成" : "真实模型请求失败"), elapsed, testedAt ? `检查于 ${testedAt}` : ""].filter(Boolean).join(" · ");
  }
  return enabled ? "尚未发起真实模型请求，点击立即检查" : "停用的 Worker 不参与检查";
}
