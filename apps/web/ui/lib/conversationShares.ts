import { apiFetch } from "./useRun";

export type ShareSnapshot = {
  version: number; title: string; captured_at: number; watermark: number;
  branch: "current"; redact_paths: boolean; tools_exclude_parameters: boolean;
  messages: Array<{ id: string; role: string; text: string }>;
  tool_summaries: Array<{ name: string; status: string }>;
  attachments: Array<{ id: string; name: string; size: number; text: string }>;
};
export type SharePreview = {
  preview_id: string; snapshot: ShareSnapshot;
  choices: { messages: Array<{ id: string; role: string; preview: string }>; attachments: Array<{ id: string; name: string; size: number; unavailable_reason: string }> };
};
export type ShareRecord = {
  share_id: string; access_mode: "authenticated" | "link"; created_at: number;
  expires_at: number; status?: "active" | "expired" | "revoked";
  message_count?: number; watermark?: number;
};
export type ShareView = ShareRecord & { snapshot: ShareSnapshot };

export function shareDate(seconds: number): string {
  return new Intl.DateTimeFormat("zh-CN", { dateStyle: "medium", timeStyle: "long" }).format(new Date(seconds * 1000));
}
export async function shareJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await apiFetch(path, { cache: "no-store", ...init });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw Object.assign(new Error(typeof body.detail === "string" ? body.detail : body.error?.message || `请求失败（HTTP ${response.status}）`), { status: response.status });
  return body as T;
}
export function sharePost(body: unknown): RequestInit {
  return { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
}
