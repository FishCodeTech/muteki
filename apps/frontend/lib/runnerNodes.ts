import { apiFetch } from "@/lib/serviceAuth";

export type RunnerNodeState = "active" | "draining" | "revoked";
export type RunnerNode = {
  id: string;
  name: string;
  projects: string[];
  state: RunnerNodeState;
  credential_version: number;
  credential_expires_at: number;
  session_generation: number;
  lease_expires_at: number;
  created_at: number;
  connected: boolean;
};
export type RunnerListenerConfig = { enabled: boolean; listen: string; public_url: string };
export type RunnerListenerStatus = {
  running: boolean;
  address?: string;
  public_url?: string;
  ca_fingerprint?: string;
  certificate_names?: string[];
  certificate_expires_at?: number;
  started_at?: number;
  error?: string;
  managed_by_cli?: string | null;
};
export type RunnerListener = { config: RunnerListenerConfig; status: RunnerListenerStatus };
export type RunnerEnrollment = {
  token: string;
  expires_at: number;
  single_use: boolean;
  commands: { enroll: string; start: string } | null;
};
export type RunnerAuditEvent = { id: string; node_id: string; node_name?: string; action: string; at: number };

async function readJson<T>(response: Response, action: string): Promise<T> {
  const body = await response.json().catch(() => null) as (T & { error?: string }) | null;
  if (!response.ok) {
    const detail = body && typeof body.error === "string" ? body.error : `HTTP ${response.status}`;
    throw new RunnerApiError(`${action}失败：${detail}`, response.status, body);
  }
  if (!body) throw new RunnerApiError(`${action}失败：服务没有返回内容`, response.status, null);
  return body;
}

export class RunnerApiError extends Error {
  constructor(message: string, readonly status: number, readonly body: unknown) { super(message); }
}

const json = (method: string, body?: unknown): RequestInit => ({
  method,
  headers: body === undefined ? undefined : { "Content-Type": "application/json" },
  body: body === undefined ? undefined : JSON.stringify(body),
});

export async function listRunnerNodes(signal?: AbortSignal): Promise<RunnerNode[]> {
  const body = await readJson<{ nodes: RunnerNode[] }>(await apiFetch("/api/runner/nodes", { signal, cache: "no-store" }), "读取执行节点");
  return [...body.nodes].sort((a, b) => a.created_at - b.created_at);
}
export async function getRunnerListener(signal?: AbortSignal): Promise<RunnerListener> {
  return readJson(await apiFetch("/api/runner/listener", { signal, cache: "no-store" }), "读取远程接入状态");
}
export async function putRunnerListener(config: RunnerListenerConfig): Promise<RunnerListener> {
  return readJson(await apiFetch("/api/runner/listener", json("PUT", config)), "保存远程接入");
}
export async function issueRunnerEnrollment(name: string, ttlSeconds: number, projects: string[] = ["personal"]): Promise<RunnerEnrollment> {
  return readJson(await apiFetch("/api/runner/enrollments", json("POST", { name, projects, ttl_seconds: ttlSeconds })), "生成注册令牌");
}
export async function setRunnerNodeState(id: string, state: RunnerNodeState): Promise<void> {
  await readJson(await apiFetch(`/api/runner/nodes/${encodeURIComponent(id)}/state`, json("POST", { state })), "更新节点状态");
}
export type RunnerRotation = { node_id: string; version: number; expires_at: number; commands: { install: string } };
export async function rotateRunnerNode(id: string): Promise<RunnerRotation> {
  return readJson(await apiFetch(`/api/runner/nodes/${encodeURIComponent(id)}/rotate`, json("POST")), "轮换节点凭据");
}
export async function deleteRunnerNode(id: string): Promise<void> {
  await readJson(await apiFetch(`/api/runner/nodes/${encodeURIComponent(id)}`, json("DELETE")), "删除节点");
}
export async function listRunnerAudit(signal?: AbortSignal): Promise<RunnerAuditEvent[]> {
  return (await readJson<{ events: RunnerAuditEvent[] }>(await apiFetch("/api/runner/audit", { signal, cache: "no-store" }), "读取节点审计")).events;
}
