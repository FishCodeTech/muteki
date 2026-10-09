import {
  descriptorForEngine, adapterDescriptor, engineForAdapter, type ProviderDescriptorCatalog,
} from "@/lib/providerDescriptors";
import type { GlobalCredential, WorkerModelTestResult } from "@/lib/useRun";

/** Engine id of a provider descriptor (``identity.engine``). */
export type Engine = string;
export type Connection = "official" | "custom_endpoint";
export type StatusKind = "ok" | "warn" | "bad" | "disabled";
export type Feedback = { kind: "ok" | "warn" | "bad"; detail: string } | null;
export type ModelRefreshFeedback = { kind: "ok" | "bad"; detail: string } | null;
export type TransportField = { type?: "string" | "boolean" | "number" | "integer"; title?: string; default?: string | number | boolean };
export type VersionCheck = {
  installed_version: string; latest_version: string;
  status: "current" | "update_available" | "unknown";
  update_available: boolean; checked_at?: string; attempted_at?: string;
  detail: string; source?: string; stale?: boolean;
};
export type RuntimeInstance = {
  key: string; adapter_id: string; instance_id: string; engine: string; label: string;
  updated_at?: string; binary_path: string; endpoint: string; transport: Record<string, string | number | boolean>;
  /** `{NAME: "env:HOST_VAR" | "secret://..."}`; the service rejects inline values. */
  env_refs?: Record<string, string>;
  /** Arguments inserted after the executable; structured adapters only. */
  launch_args?: string[];
  config_schema?: { properties?: { transport?: { properties?: Record<string, TransportField> } } };
  enabled: boolean; discovered: boolean; configured: boolean;
  auth: { status: "ok" | "missing" | "unknown" | "unavailable" | "not_applicable"; detail: string; login_command?: string; code?: string };
  health: null | { healthy: boolean; detail: string; runtime_version: string; probed_at?: string; version_check?: VersionCheck; capabilities: Record<string, unknown>; degradations: string[] };
};
export type EngineSummary = {
  engine: Engine; label: string; cliAdapterId: string; credentials: GlobalCredential[]; instances: RuntimeInstance[];
  primaryCredential: GlobalCredential | null; primaryInstance: RuntimeInstance | null;
  enabled: boolean; version: string; readyCredentialCount: number; modelCount: number;
  environmentCount: number; statusKind: StatusKind; statusText: string;
  supportStatus: "supported" | "temporarily_disabled"; disabledReason: string;
};
export type CredentialDraft = {
  accountId: string; connection: Connection; provider: string; baseUrl: string;
  secret: string; defaultModel: string; modelsText: string;
};

/** Host router adapter: Next.js navigation on web, the desktop shell's `go` on desktop. */
export interface AgentsSettingsNavigation {
  pathname: string;
  searchParams: URLSearchParams;
  router: {
    push(href: string, options?: { scroll?: boolean }): void;
    replace(href: string, options?: { scroll?: boolean }): void;
  };
}

export interface AgentsSelection {
  engine: Engine | null;
  instanceId: string;
  credentialId: string;
}

export const CAPABILITY_LABELS: Record<string, string> = {
  streaming: "流式输出", resume: "会话恢复", steer: "过程转向", interrupt: "即时中断", approval: "权限审批",
  user_input: "用户输入", fork: "分支复制", structured_output: "结构化输出", subagents: "子代理派发",
  skills: "技能支持", mcp: "MCP 协议", native_tool_binding: "原生工具调用", tool_events: "工具事件",
  usage_events: "用量事件", session_persistence: "会话持久化", plan: "结构化计划",
  compaction: "上下文压缩", image_input: "图像输入",
};
export const EMPTY_DRAFT: CredentialDraft = { accountId: "", connection: "official", provider: "", baseUrl: "", secret: "", defaultModel: "", modelsText: "" };

type Catalog = ProviderDescriptorCatalog | null;

export function asEngine(catalog: Catalog, value?: string): Engine | null {
  return descriptorForEngine(catalog, value)?.identity.engine ?? null;
}

/** CLI Runtime adapter of an engine; the default instance key and the URL query value use it. */
export function engineCliAdapter(catalog: Catalog, engine: Engine): string {
  return descriptorForEngine(catalog, engine)?.identity.cli_adapter_id ?? "";
}

/** URL query value for an engine: its CLI adapter id, falling back to the engine id. */
export function engineQueryValue(catalog: Catalog, engine: Engine): string {
  return engineCliAdapter(catalog, engine) || engine;
}

export function engineFromQueryValue(catalog: Catalog, value: string): Engine | null {
  return asEngine(catalog, value) || asEngine(catalog, engineForAdapter(catalog, value));
}

/** Transport settings an adapter declares, for instances that carry no config schema. */
export function declaredTransportFields(catalog: Catalog, adapterId: string): Record<string, TransportField> {
  const settings = adapterDescriptor(catalog, adapterId)?.transport_settings ?? [];
  return Object.fromEntries(settings.map((item) => [item.name, {
    type: item.type,
    ...(item.title ? { title: item.title } : {}),
    ...(item.default !== null ? { default: item.default } : {}),
  }]));
}

export function readSelection(searchParams: URLSearchParams, catalog: Catalog): AgentsSelection {
  const engineValue = (searchParams.get("engine") || "").trim();
  return {
    engine: engineValue ? engineFromQueryValue(catalog, engineValue) : null,
    instanceId: (searchParams.get("instance") || "").trim(),
    credentialId: (searchParams.get("credential") || "").trim(),
  };
}

export function selectionHref(pathname: string, searchParams: URLSearchParams, selection: AgentsSelection, catalog: Catalog): string {
  const params = new URLSearchParams(searchParams.toString());
  if (selection.engine) params.set("engine", engineQueryValue(catalog, selection.engine));
  else params.delete("engine");
  if (selection.engine && selection.instanceId) params.set("instance", selection.instanceId);
  else params.delete("instance");
  if (selection.engine && selection.credentialId) params.set("credential", selection.credentialId);
  else params.delete("credential");
  const query = params.toString();
  return query ? `${pathname}?${query}` : pathname;
}

export function splitModels(value: string) {
  return [...new Set(value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean))];
}
export function credentialModels(item: GlobalCredential) {
  return [...new Set([item.default_model || "", ...(item.models || []), ...(item.candidate_models || [])].map(String).map((model) => model.trim()).filter(Boolean))];
}
export function credentialReady(item: GlobalCredential) {
  return item.present && ["ready", "untested"].includes(item.status);
}
export function credentialName(item: GlobalCredential) {
  return item.source === "system" ? "本机宿主登录" : item.label || item.account_id || item.id.replace(/^account:/, "");
}
export function runtimeLabel(instance: RuntimeInstance | null) {
  if (instance && !instance.adapter_id.startsWith("cli.")) {
    const transport = instance.adapter_id.split(".")[1] || "";
    return transport === "app_server" ? "Codex App Server" : `${instance.engine} · ${transport.replaceAll("_", " ")}`;
  }
  const path = (instance?.binary_path || "").trim();
  const parts = path.split(/[\\/]/).filter(Boolean);
  const binary = parts[parts.length - 1];
  if (binary) return binary.includes("cli") ? binary : `${binary} CLI`;
  const adapter = instance?.adapter_id.replace(/^cli\./, "").trim();
  return adapter ? `${adapter} CLI` : "本机 CLI";
}
export function installedVersion(version: string, check?: VersionCheck) {
  if (check?.installed_version) return check.installed_version;
  return version.match(/\d+(?:\.\d+){1,3}(?:-[0-9a-z._-]+)?/i)?.[0] || "未探测";
}
export function versionStatus(check?: VersionCheck) {
  if (!check) return "尚未检查最新版本";
  if (check.status === "update_available" && check.latest_version) {
    return `${check.stale ? "上次结果：" : ""}可更新至 ${check.latest_version}`;
  }
  if (check.status === "current") return check.stale ? "上次检查为最新" : "已是最新";
  return "最新版本未获取";
}
export function versionCheckDetail(check?: VersionCheck) {
  if (!check) return "最新版本尚未检查。";
  const timestamp = check.attempted_at || check.checked_at;
  const date = timestamp ? new Date(timestamp) : null;
  const checkedAt = date && !Number.isNaN(date.getTime())
    ? date.toLocaleString("zh-CN", { hour12: false })
    : "时间未知";
  const source = check.source ? `；来源 ${check.source}` : "";
  return `${check.detail || "最新版本未获取"}${source}；检查时间 ${checkedAt}。只检测版本，不执行更新。`;
}

export function catalogSummary(catalog?: GlobalCredential["model_catalog"]) {
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

export function lastTestAsResult(item: GlobalCredential, engine: string): WorkerModelTestResult | null {
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

export function credentialLiveStatus(
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

export function formatTestTime(value?: number) {
  if (!value) return "";
  const date = new Date(value > 1e12 ? value : value * 1000);
  if (Number.isNaN(date.getTime())) return "";
  return date.toLocaleString("zh-CN", { hour12: false, month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit" });
}

export function statusTone(kind: StatusKind): "success" | "warning" | "danger" | "neutral" {
  if (kind === "ok") return "success";
  if (kind === "warn") return "warning";
  if (kind === "bad") return "danger";
  return "neutral";
}
