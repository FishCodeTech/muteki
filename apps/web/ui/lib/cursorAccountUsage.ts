import { apiFetch } from "@/lib/useRun";

export const CURSOR_USAGE_DAYS = [1, 7, 30, 90] as const;
export type CursorUsageDays = (typeof CURSOR_USAGE_DAYS)[number];

export interface CursorUsageError {
  code: string;
  message: string;
  action?: "enable_cursor_keychain";
}

export interface CursorUsageTotals {
  uncached_input: number;
  cache_read: number;
  cache_write: number;
  output: number;
  total_tokens: number;
  /** Cursor's own charge; null when no event in the group reported one. */
  reported_cost: number | null;
  events: number;
  unpriced_events: number;
  sessions: number;
}

export interface CursorModelUsage extends CursorUsageTotals {
  model: string;
}

export interface CursorUsageSlot {
  /** `YYYY-MM-DD`, or `YYYY-MM-DDTHH:00` for the past-24h window, in the requested time zone. */
  slot: string;
  reported_cost: number;
  total_tokens: number;
}

export type CursorLimitWindowId = "totalPercentUsed" | "autoPercentUsed" | "apiPercentUsed";

export interface CursorLimitWindow {
  id: CursorLimitWindowId;
  kind: "monthly";
  label: [string, string];
  description: [string, string];
  used_percent: number;
  resets_at: number | null;
}

export interface CursorAccountUsage {
  read_at: number;
  enabled: boolean;
  source: "env" | "keychain" | "auth_file" | null;
  account: string | null;
  window: { days: CursorUsageDays; time_zone: string; since_ms: number; until_ms: number; resolution: "hour" | "day" };
  error: CursorUsageError | null;
  limits: { windows: CursorLimitWindow[]; resets_at: number | null } | null;
  limits_error: CursorUsageError | null;
  history: { totals: CursorUsageTotals; models: CursorModelUsage[]; series: CursorUsageSlot[] } | null;
  history_error: CursorUsageError | null;
}

export interface UsageSettings {
  cursor_account_usage_enabled: boolean;
  keychain: { applies: boolean; login_present: boolean | null; error: CursorUsageError | null };
  auth_file: string | null;
}

async function failure(response: Response): Promise<Error> {
  try {
    const body = await response.json() as { detail?: unknown; error?: { message?: string } };
    if (body.error?.message) return new Error(body.error.message);
    if (typeof body.detail === "string") return new Error(body.detail);
  } catch { /* fall through to the status line */ }
  return new Error(`HTTP ${response.status}`);
}

export function browserTimeZone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";
}

export async function fetchCursorAccountUsage(days: CursorUsageDays, { refresh = false, signal }: { refresh?: boolean; signal?: AbortSignal } = {}): Promise<CursorAccountUsage> {
  const query = new URLSearchParams({ days: String(days), tz: browserTimeZone() });
  if (refresh) query.set("refresh", "true");
  const response = await apiFetch(`/api/usage/cursor-account?${query}`, { signal });
  if (!response.ok) throw await failure(response);
  return await response.json() as CursorAccountUsage;
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

/** Overall is the sum of the two pools, so it is shown only when a pool is missing. */
export function displayLimitWindows(windows: CursorLimitWindow[]): CursorLimitWindow[] {
  const hasBothPools = windows.some(w => w.id === "autoPercentUsed") && windows.some(w => w.id === "apiPercentUsed");
  return hasBothPools ? windows.filter(w => w.id !== "totalPercentUsed") : windows;
}
