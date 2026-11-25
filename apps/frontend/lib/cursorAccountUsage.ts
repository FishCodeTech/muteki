import { apiFetch } from "@/lib/useRun";

export interface CursorUsageError {
  code: string;
  message: string;
  action?: "enable_cursor_keychain";
}

export interface UsageSettings {
  host_account_discovery_supported?: boolean;
  cursor_account_usage_enabled: boolean;
  keychain: { applies: boolean; login_present: boolean | null; error: CursorUsageError | null };
  auth_file: string | null;
}

async function failure(response: Response): Promise<Error> {
  try {
    const body = await response.json() as { detail?: unknown; error?: { message?: string } };
    if (body.error?.message) return new Error(body.error.message);
    if (typeof body.detail === "string") return new Error(body.detail);
  } catch {  }
  return new Error(`HTTP ${response.status}`);
}

export async function readUsageSettings(signal?: AbortSignal): Promise<UsageSettings> {
  const response = await apiFetch("/api/usage/settings", { signal });
  if (!response.ok) throw await failure(response);
  return await response.json() as UsageSettings;
}

export async function writeUsageSettings(patch: { cursor_account_usage_enabled: boolean }): Promise<UsageSettings> {
  const response = await apiFetch("/api/usage/settings", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
  if (!response.ok) throw await failure(response);
  return await response.json() as UsageSettings;
}
