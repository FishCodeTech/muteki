"use client";

import { setConversationStorageScope } from "./conversationStorageScope";
import { desktopChatBridge } from "./desktopChatBridge";

const WEB_API = process.env.NEXT_PUBLIC_MUTEKI_API || "";
export let API = WEB_API;
let serviceOrigin = API || (typeof window !== "undefined" ? window.location.origin : "");
let generation = 0;
let scope = "";
let persistenceWarning = "";
let notice = "";
export type AuthState = { authenticated: boolean; authRequired: boolean; inContainer: boolean; expiresAt: number | null };
type AuthReason = "expired" | "service_changed" | "peer_changed";
const listeners = new Set<(reason?: AuthReason) => void>();
let pendingCheck: { generation: number; promise: Promise<AuthState> } | null = null;
const channel = typeof window !== "undefined" && typeof BroadcastChannel !== "undefined"
  ? new BroadcastChannel("muteki:service-auth") : null;

// Remove the legacy JS-readable session for this client. Credentials now belong
// to the cookie jar (Web) or the native main process (desktop).
if (typeof window !== "undefined") {
  try {
    for (const key of Object.keys(window.localStorage)) {
      if (key === "muteki_auth_token" || key.startsWith("muteki_auth_token:")) window.localStorage.removeItem(key);
    }
  } catch { /* no new session depends on localStorage */ }
}

function setScope(next: string): void {
  scope = next;
  setConversationStorageScope(next);
  if (typeof window !== "undefined") window.dispatchEvent(new CustomEvent("muteki:auth-scope", { detail: next }));
}
export function currentAuthGeneration(): number { return generation; }
export function currentAuthScope(): string { return scope; }
export function currentAuthPersistenceWarning(): string { return persistenceWarning; }
export function currentAuthNotice(): string { return notice; }
export function currentServiceOrigin(): string { return serviceOrigin; }
export function isNativeAuth(): boolean { return Boolean(desktopChatBridge()); }
export function apiRequestUrl(path: string): string {
  // Keep Web requests on the UI origin so long-lived backend SSE connections
  // cannot occupy every browser connection needed for control replies.
  if (typeof window !== "undefined" && !isNativeAuth() && API
      && new URL(API, window.location.origin).origin !== window.location.origin) {
    return path;
  }
  return `${API}${path}`;
}
export function onAuthRequired(fn: (reason?: AuthReason) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
export function invalidateAuth(reason: AuthReason = "expired", message = ""): void {
  generation += 1;
  pendingCheck = null;
  notice = message;
  setScope("");
  listeners.forEach(fn => fn(reason));
}
function publishChange(): void { channel?.postMessage({ origin: serviceOrigin }); }
if (channel) channel.onmessage = event => {
  if (event.data?.origin === serviceOrigin && !isNativeAuth()) invalidateAuth("peer_changed");
};

export function resetAuthForServiceChange(origin: string, transportOrigin?: string): void {
  const nextApi = transportOrigin ?? WEB_API;
  if (serviceOrigin === origin && API === nextApi) return;
  API = nextApi;
  serviceOrigin = origin;
  persistenceWarning = "";
  invalidateAuth("service_changed");
}

export async function apiFetch(path: string, init?: RequestInit): Promise<Response> {
  const headers = new Headers(init?.headers);
  const requestGeneration = generation;
  if (!["GET", "HEAD"].includes((init?.method || "GET").toUpperCase())) headers.set("X-Muteki-CSRF", "1");
  const response = await fetch(apiRequestUrl(path), { ...init, headers,
    credentials: isNativeAuth() ? "omit" : "include" });
  // A late response from the previous login/service cannot lock a new session.
  if (response.status === 401 && requestGeneration === generation && path !== "/api/auth/me" && path !== "/api/auth/login") {
    invalidateAuth("expired", "登录已失效，请重新登录。");
  }
  return response;
}

function identity(data: Record<string, unknown>): string {
  if (data.session_protocol !== 2 || typeof data.service_id !== "string" || !data.service_id
      || typeof data.identity_id !== "string" || !data.identity_id || typeof data.auth_required !== "boolean") {
    throw new Error("服务的登录协议不兼容，请更新服务和桌面客户端后重试。");
  }
  return `${data.service_id}:${data.identity_id}`;
}
async function responseError(response: Response, fallback: string): Promise<Error> {
  const data = await response.json().catch(() => ({}));
  return new Error(typeof data.error?.message === "string" ? data.error.message
    : typeof data.detail === "string" ? data.detail : `${fallback}（HTTP ${response.status}）`);
}

export async function login(password: string, remember = true): Promise<{ ok: boolean; authRequired: boolean }> {
  // Invalidate prior in-flight checks before changing the shared session.
  const attempt = ++generation;
  pendingCheck = null;
  const response = await apiFetch("/api/auth/login", { method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password, remember, client: isNativeAuth() ? "desktop" : "web" }),
    signal: AbortSignal.timeout(15_000) });
  if (response.status === 401) return { ok: false, authRequired: true };
  if (!response.ok) throw await responseError(response, "登录服务不可用");
  const data = await response.json() as Record<string, unknown>;
  identity(data);
  if (data.ok !== true || data.session_owner !== (isNativeAuth() ? "desktop" : "cookie") || "token" in data) {
    throw new Error("客户端无法安全保管登录会话，请更新客户端后重试。");
  }
  if (attempt !== generation) throw new Error("连接或登录状态已改变，请在当前服务重试。");
  notice = "";
  persistenceWarning = typeof data.persistence_warning === "string" ? data.persistence_warning : "";
  publishChange();
  return { ok: true, authRequired: data.auth_required as boolean };
}

export function checkAuth(): Promise<AuthState> {
  if (pendingCheck?.generation === generation) return pendingCheck.promise;
  const attempt = generation;
  const promise = (async () => {
    const response = await apiFetch("/api/auth/me", { signal: AbortSignal.timeout(15_000), cache: "no-store" });
    if (attempt !== generation) throw new Error("连接或登录状态已改变，请重新校验。");
    if (response.status === 401) {
      setScope("");
      return { authenticated: false, authRequired: true, inContainer: false, expiresAt: null };
    }
    if (!response.ok) throw await responseError(response, "认证服务不可用");
    const data = await response.json() as Record<string, unknown>;
    const nextScope = identity(data);
    if (data.authenticated !== true) throw new Error("认证服务返回了无效响应。");
    if (attempt !== generation) throw new Error("连接或登录状态已改变，请重新校验。");
    setScope(nextScope);
    return { authenticated: true, authRequired: data.auth_required as boolean,
      inContainer: data.in_container === true, expiresAt: typeof data.expires_at === "number" ? data.expires_at : null };
  })();
  pendingCheck = { generation: attempt, promise };
  void promise.finally(() => { if (pendingCheck?.promise === promise) pendingCheck = null; }).catch(() => {});
  return promise;
}

export async function logout(): Promise<void> {
  const response = await apiFetch("/api/auth/logout", { method: "POST", signal: AbortSignal.timeout(15_000) });
  if (!response.ok && response.status !== 401) throw await responseError(response, "退出登录失败");
  invalidateAuth("expired", "已退出登录。");
  publishChange();
}

export function accessSettingsChanged(authRequired: boolean): void {
  invalidateAuth(authRequired ? "expired" : "peer_changed", "访问设置已保存，旧会话已退出。请使用当前密码登录。");
  publishChange();
}

export async function authTicket(): Promise<string> {
  const response = await apiFetch("/api/auth/ticket", { method: "POST", signal: AbortSignal.timeout(15_000) });
  if (!response.ok) throw await responseError(response, "连接授权失败");
  const data = await response.json() as Record<string, unknown>;
  if (typeof data.ticket !== "string" || !data.ticket) throw new Error("连接授权返回了无效响应。");
  return data.ticket;
}
