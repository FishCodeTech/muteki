import { apiFetch } from "./serviceAuth";
import type { ComposerCapabilityItem } from "./composerCapabilities";

export const MAC_APP_DRAG_MIME = "application/x-muteki-mac-app";
export const MAC_APP_REF_PREFIX = "mcp:muteki-computer:app:";

export interface MacAppItem extends ComposerCapabilityItem {
  icon?: string;
  in_dock?: boolean;
  running?: boolean;
}

export type MacAppInventory =
  | { available: true; apps: MacAppItem[] }
  | { available: false; code: string; reason: string };

export async function fetchMacApps(signal?: AbortSignal): Promise<MacAppInventory> {
  const response = await apiFetch("/api/conversation/mac-apps", { signal });
  const body = await response.json().catch(() => null) as {
    available?: boolean;
    code?: string;
    reason?: string;
    apps?: MacAppItem[];
    error?: { message?: string; code?: string };
  } | null;
  if (!response.ok || !body) {
    throw new Error(body?.error?.message || `本机应用列表加载失败（HTTP ${response.status}）`);
  }
  if (body.available !== true) {
    return { available: false, code: body.code || "", reason: body.reason || "当前无法指定本机应用" };
  }
  if (!Array.isArray(body.apps)) throw new Error("本机应用列表响应缺少 apps");
  return { available: true, apps: body.apps };
}

export function macAppCapability(app: MacAppItem): ComposerCapabilityItem {
  return {
    id: app.id,
    kind: app.kind,
    name: app.name,
    description: app.description,
    source: app.source,
    scope: app.scope,
    ...(app.target ? { target: app.target } : {}),
  };
}

export function macAppFromDataTransfer(data: DataTransfer): ComposerCapabilityItem | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(data.getData(MAC_APP_DRAG_MIME));
  } catch {
    return null;
  }
  const item = parsed as Partial<ComposerCapabilityItem> | null;
  if (!item || typeof item.id !== "string" || !item.id.startsWith(MAC_APP_REF_PREFIX)) return null;
  if (item.kind !== "mcp" || typeof item.name !== "string") return null;
  return {
    id: item.id,
    kind: item.kind,
    name: item.name,
    description: String(item.description || ""),
    source: String(item.source || ""),
    scope: String(item.scope || "turn"),
    ...(item.target ? { target: item.target } : {}),
  };
}
