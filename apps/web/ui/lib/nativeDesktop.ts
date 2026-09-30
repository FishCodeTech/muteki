"use client";

import { useEffect, useState, useSyncExternalStore } from "react";
import { currentAuthScope } from "./useRun";
import { desktopChatBridge, type DesktopNativeState, type DesktopWorkspaceGrant, type NativeCapabilityId, type NativeCapabilityManifest } from "./desktopChatBridge";

export const NATIVE_CAPABILITIES: NativeCapabilityId[] = ["pathSelection", "workspaceFileActions", "preview", "attachmentCache", "terminal", "microphone", "notifications", "deepLinks"];
export class NativeDesktopError extends Error {
  constructor(public code: string, message: string) { super(message); this.name = "NativeDesktopError"; }
}

const NATIVE_ENGLISH: Record<string, string> = {
  "desktop.capabilities_missing": "The desktop has not provided its native capability manifest.",
  "desktop.capabilities_version_unsupported": "This native capability manifest version is unsupported. Update the desktop app.",
  "desktop.capabilities_invalid": "The desktop capability manifest has an invalid status or host scope.",
  "desktop.capability_host_mismatch": "This capability belongs to a different host from the requested action.",
  "desktop.capability_unavailable": "This desktop environment does not provide this capability.",
  "desktop.mapping_manual": "Select the corresponding client directory. File synchronization has not been verified.",
  "desktop.packaged_only": "Development builds do not register the system chat protocol.",
  "desktop.microphone_os_settings": "Manage microphone permissions in system settings.",
  "desktop.identity_unverified": "Verify the current service identity before connecting its client directory.",
  "desktop.workspace_actions_unavailable": "This desktop environment does not provide client file actions.",
  "desktop.workspace_relative_path_invalid": "Select a relative file path inside the current service workspace.",
  "desktop.workspace_grant_expired": "The client directory mapping has expired. Select it again.",
  "desktop.workspace_grant_invalid": "The desktop returned an invalid workspace mapping.",
  "desktop.connection_changed": "The service connection changed. Select the corresponding client directory again.",
};
export function nativeDisplayMessage(value: { code?: string; reason?: string; message?: string }, english: boolean): string {
  return (english && value.code && NATIVE_ENGLISH[value.code]) || value.reason || value.message || (english ? "The native operation failed." : "原生操作失败。");
}

export function nativeManifest(value: unknown): NativeCapabilityManifest {
  if (!value || typeof value !== "object") throw new NativeDesktopError("desktop.capabilities_missing", "桌面尚未提供原生能力清单。");
  const raw = value as Record<string, unknown>;
  if (raw.version !== 1 || raw.host !== "desktop-client" || !raw.entries || typeof raw.entries !== "object") {
    throw new NativeDesktopError("desktop.capabilities_version_unsupported", "桌面原生能力清单版本不受支持，请更新桌面端。");
  }
  const entries = raw.entries as Record<string, unknown>;
  for (const id of NATIVE_CAPABILITIES) {
    const row = entries[id] as Record<string, unknown> | undefined;
    if (!row || typeof row.supported !== "boolean" || !["desktop-client", "service"].includes(String(row.host)) || (row.code !== undefined && typeof row.code !== "string") || (row.reason !== undefined && typeof row.reason !== "string")) {
      throw new NativeDesktopError("desktop.capabilities_invalid", `桌面能力 ${id} 缺少有效状态或宿主归属。`);
    }
  }
  return raw as unknown as NativeCapabilityManifest;
}

export function nativeCapability(state: DesktopNativeState | null, id: NativeCapabilityId, expectedHost: "desktop-client" | "service" = "desktop-client"): { available: boolean; reason: string; code: string } {
  try {
    const entry = nativeManifest(state?.capabilities).entries[id];
    if (entry.host !== expectedHost) return { available: false, reason: entry.reason || "该能力的宿主范围与当前操作不一致。", code: entry.code || "desktop.capability_host_mismatch" };
    return { available: entry.supported, reason: entry.reason || (entry.supported ? "" : "当前桌面环境未提供此能力。"), code: entry.code || (entry.supported ? "" : "desktop.capability_unavailable") };
  } catch (error) {
    return { available: false, reason: error instanceof Error ? error.message : String(error), code: error instanceof NativeDesktopError ? error.code : "desktop.capabilities_failed" };
  }
}

const grants = new Map<string, DesktopWorkspaceGrant>();
const grantListeners = new Set<() => void>();
let nativeScope = "";
const emitGrants = () => { for (const listener of grantListeners) listener(); };
function acceptScope(state: DesktopNativeState): void {
  const next = `${state.connectionVersion || 0}:${state.serviceId || ""}:${state.identityId || ""}`;
  if (next !== nativeScope) { nativeScope = next; grants.clear(); emitGrants(); }
}

export function useNativeDesktopState(): { desktop: boolean; state: DesktopNativeState | null; error: string } {
  const bridge = desktopChatBridge();
  const [state, setState] = useState<DesktopNativeState | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!bridge) return;
    let active = true;
    let updates = 0;
    const accept = (next: DesktopNativeState) => { if (active) { updates++; acceptScope(next); setState(next); setError(""); } };
    const off = bridge.onState?.(accept);
    const initialUpdate = updates;
    if (bridge.getState) void bridge.getState().then(next => { if (updates === initialUpdate) accept(next); }).catch(cause => { if (active) setError(cause instanceof Error ? cause.message : String(cause)); });
    else setError("桌面尚未提供原生能力状态接口。");
    return () => { active = false; off?.(); };
  }, [bridge]);
  return { desktop: Boolean(bridge), state, error };
}

export interface NativeWorkspaceContext { threadId: string; workspaceId: string; serviceRoot: string }
function workspaceKey(state: DesktopNativeState | null, context: NativeWorkspaceContext): string {
  return JSON.stringify([state?.connectionVersion || 0, state?.serviceId || "", state?.identityId || "", context.threadId, context.workspaceId, context.serviceRoot]);
}
function workspaceIdentity(state: DesktopNativeState | null): { serviceId: string; identityId: string } {
  if (!state?.serviceId || !state.identityId || currentAuthScope() !== `${state.serviceId}:${state.identityId}`) {
    throw new NativeDesktopError("desktop.identity_unverified", "请先确认当前服务身份，再连接本机对应目录。");
  }
  return { serviceId: state.serviceId, identityId: state.identityId };
}
export function workspaceRelativePath(value: string): string {
  // Service file previews return workspace-relative POSIX paths. Do not infer
  // a client absolute path from a server path or a loopback service address.
  if (!value || value.startsWith("/") || value.includes("\\") || value.includes("\0") || /^[A-Za-z]:/.test(value) || value.split("/").some(part => !part || part === "." || part === "..")) {
    throw new NativeDesktopError("desktop.workspace_relative_path_invalid", "文件必须使用当前服务工作区内的相对路径。");
  }
  return value;
}

export function useNativeWorkspaceGrant(state: DesktopNativeState | null, context: NativeWorkspaceContext): DesktopWorkspaceGrant | null {
  const key = workspaceKey(state, context);
  return useSyncExternalStore(callback => { grantListeners.add(callback); return () => { grantListeners.delete(callback); }; }, () => grants.get(key) || null, () => null);
}
export async function selectNativeWorkspaceRoot(state: DesktopNativeState | null, context: NativeWorkspaceContext): Promise<DesktopWorkspaceGrant | null> {
  const bridge = desktopChatBridge();
  if (!nativeCapability(state, "workspaceFileActions").available || !bridge?.selectWorkspaceRoot) throw new NativeDesktopError("desktop.workspace_actions_unavailable", "当前桌面环境未提供本机文件操作。");
  const identity = workspaceIdentity(state);
  const key = workspaceKey(state, context);
  const selected = await bridge.selectWorkspaceRoot({ ...context, ...identity });
  if (!selected) return null;
  if (currentAuthScope() !== `${identity.serviceId}:${identity.identityId}`) throw new NativeDesktopError("desktop.connection_changed", "服务身份已改变，请重新选择本机对应目录。");
  if (bridge.getState && workspaceKey(await bridge.getState(), context) !== key) throw new NativeDesktopError("desktop.connection_changed", "服务连接已改变，请重新选择本机对应目录。");
  if (!selected.grantId || selected.threadId !== context.threadId || selected.workspaceId !== context.workspaceId || selected.serviceRoot !== context.serviceRoot || selected.host !== "desktop-client" || selected.mapping !== "user-selected" || !selected.clientRoot) throw new NativeDesktopError("desktop.workspace_grant_invalid", "桌面返回了无效的工作区映射。");
  grants.set(key, selected); emitGrants(); return selected;
}
export async function openNativeWorkspaceFile(state: DesktopNativeState | null, context: NativeWorkspaceContext, grant: DesktopWorkspaceGrant, relativePath: string, action: "reveal" | "open"): Promise<void> {
  const bridge = desktopChatBridge();
  if (!nativeCapability(state, "workspaceFileActions").available || !bridge?.openWorkspaceFile) throw new NativeDesktopError("desktop.workspace_actions_unavailable", "当前桌面环境未提供本机文件操作。");
  const identity = workspaceIdentity(state);
  if (grants.get(workspaceKey(state, context)) !== grant) throw new NativeDesktopError("desktop.workspace_grant_expired", "本机目录映射已过期，请重新选择。");
  await bridge.openWorkspaceFile({ grantId: grant.grantId, threadId: context.threadId, workspaceId: context.workspaceId, ...identity, relativePath: workspaceRelativePath(relativePath), action });
}
