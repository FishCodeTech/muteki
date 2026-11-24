
export type LaterErrorCode = "missing_time" | "missing_message" | "invalid_time" | "skipped_time";

export type LaterParse =
  | { ok: true;  at: number; message: string }
  | { ok: false; code: LaterErrorCode; hour?: number; minute?: number };

const WALL_CLOCK = /^(\d{1,2}):(\d{2})$/;
const RELATIVE = /^(\d+)(min|s|h)$/;
const UNIT_SECONDS = { s: 1, min: 60, h: 3600 } as const;

function nextWallClock(now: Date, hour: number, minute: number): LaterParse | { ok: true; at: number } {
  if (hour > 23 || minute > 59) return { ok: false, code: "invalid_time" };
  for (const dayOffset of [0, 1]) {
    const candidate = new Date(now.getFullYear(), now.getMonth(), now.getDate() + dayOffset, hour, minute, 0, 0);
    if (candidate.getHours() !== hour || candidate.getMinutes() !== minute) {
      if (candidate.getTime() > now.getTime()) return { ok: false, code: "skipped_time", hour, minute };
      continue;
    }
    if (candidate.getTime() > now.getTime()) return { ok: true, at: Math.floor(candidate.getTime() / 1000) };
  }
  return { ok: false, code: "invalid_time" };
}

export function parseLaterCommand(text: string, now: Date = new Date()): LaterParse | null {
  const match = /^\/later(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!match) return null;
  const rest = (match[1] ?? "").trim();
  if (!rest) return { ok: false, code: "missing_time" };
  const split = /\s/.exec(rest);
  const spec = split ? rest.slice(0, split.index) : rest;
  const message = split ? rest.slice(split.index).trim() : "";
  if (!message) return { ok: false, code: "missing_message" };

  const wall = WALL_CLOCK.exec(spec);
  if (wall) {
    const resolved = nextWallClock(now, Number(wall[1]), Number(wall[2]));
    return resolved.ok ? { ok: true, at: resolved.at, message } : resolved;
  }
  const relative = RELATIVE.exec(spec);
  if (!relative) return { ok: false, code: "invalid_time" };
  const seconds = Number(relative[1]) * UNIT_SECONDS[relative[2] as keyof typeof UNIT_SECONDS];
  const at = Math.floor(now.getTime() / 1000) + seconds;
  if (!Number.isSafeInteger(seconds) || seconds <= 0 || !Number.isFinite(new Date(at * 1000).getTime())) {
    return { ok: false, code: "invalid_time" };
  }
  return { ok: true, at, message };
}

export function laterErrorMessage(error: Extract<LaterParse, { ok: false }>): string {
  if (error.code === "skipped_time") {
    return `本地时间没有 ${error.hour}:${String(error.minute).padStart(2, "0")}：夏令时调整会跳过这个时刻。请换一个时间。`;
  }
  return "用法：/later <5:10 | 5min> <消息>";
}

export interface LaterPreset {
  id: string;
  label: string;
  at: number;
}

export function laterPresets(now: Date = new Date()): LaterPreset[] {
  const nowSeconds = Math.floor(now.getTime() / 1000);
  const tomorrowMorning = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1, 9, 0, 0, 0);
  return [
    { id: "10m", label: "10 分钟后", at: nowSeconds + 600 },
    { id: "1h", label: "1 小时后", at: nowSeconds + 3600 },
    { id: "tomorrow", label: "明天上午", at: Math.floor(tomorrowMorning.getTime() / 1000) },
  ];
}

const pad = (value: number) => String(value).padStart(2, "0");

export function formatScheduledTime(unixSeconds: number, now: Date = new Date()): string {
  const at = new Date(unixSeconds * 1000);
  const time = `${pad(at.getHours())}:${pad(at.getMinutes())}`;
  const dayDelta = Math.round(
    (new Date(at.getFullYear(), at.getMonth(), at.getDate()).getTime()
      - new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()) / 86_400_000,
  );
  if (dayDelta === 0) return `今天 ${time}`;
  if (dayDelta === 1) return `明天 ${time}`;
  return `${at.getMonth() + 1} 月 ${at.getDate()} 日 ${time}`;
}

export function formatCountdown(secondsLeft: number): string {
  if (secondsLeft <= 0) return "即将发送";
  if (secondsLeft < 3600) return `${pad(Math.floor(secondsLeft / 60))}:${pad(secondsLeft % 60)}`;
  if (secondsLeft < 86_400) return `${Math.floor(secondsLeft / 3600)} 小时 ${pad(Math.floor((secondsLeft % 3600) / 60))} 分`;
  return `${Math.floor(secondsLeft / 86_400)} 天 ${Math.floor((secondsLeft % 86_400) / 3600)} 小时`;
}

export function toDateTimeLocalValue(unixSeconds: number): string {
  const at = new Date(unixSeconds * 1000);
  return `${at.getFullYear()}-${pad(at.getMonth() + 1)}-${pad(at.getDate())}T${pad(at.getHours())}:${pad(at.getMinutes())}`;
}

export function fromDateTimeLocalValue(value: string): number | null {
  if (!value) return null;
  const time = new Date(value).getTime();
  return Number.isFinite(time) ? Math.floor(time / 1000) : null;
}
