/**
 * Shared number / time formatters. Every panel that renders a timestamp,
 * an elapsed duration, or a compact count should go through these so the
 * same value reads the same way everywhere.
 */

/** Event timestamps arrive as seconds or milliseconds; normalise to epoch ms (0 when unset). */
export function toEpochMs(value?: number | null): number {
  if (!value) return 0;
  return value < 1_000_000_000_000 ? value * 1000 : value;
}

function splitSeconds(ms: number): { hours: number; minutes: number; seconds: number } {
  const total = Number.isFinite(ms) && ms > 0 ? Math.floor(ms / 1000) : 0;
  return {
    hours: Math.floor(total / 3600),
    minutes: Math.floor((total % 3600) / 60),
    seconds: total % 60,
  };
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/** Elapsed time as M:SS under an hour, H:MM:SS beyond; whole seconds, floored. */
export function formatElapsed(ms: number): string {
  const { hours, minutes, seconds } = splitSeconds(ms);
  return hours ? `${hours}:${pad2(minutes)}:${pad2(seconds)}` : `${minutes}:${pad2(seconds)}`;
}

/** ISO 8601 duration (PT1H2M3S) for `<time dateTime>`. */
export function formatIsoDuration(ms: number): string {
  const { hours, minutes, seconds } = splitSeconds(ms);
  return `PT${hours ? `${hours}H` : ""}${minutes ? `${minutes}M` : ""}${seconds}S`;
}

/** Local wall clock HH:MM:SS (24h); `fallback` when the timestamp is unset. */
export function formatClock(value?: number | null, fallback = "--:--:--"): string {
  const ms = toEpochMs(value);
  if (!ms) return fallback;
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/** 999 → "999", 1234 → "1.2k", 15000 → "15k", 1_300_000 → "1.3m". */
export function compactNumber(value: number): string {
  if (value < 1000) return String(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(value < 10_000 ? 1 : 0)}k`;
  return `${(value / 1_000_000).toFixed(1)}m`;
}

type Translate = (key: string, vars?: Record<string, string | number>) => string;

/** Coarse "x ago" stamp (just now / Ns / Nm / Nh / Nd). `now` is injectable so a live ticker can drive it. */
export function formatRelativeTime(value: number | null | undefined, t: Translate, now = Date.now()): string {
  const ms = toEpochMs(value);
  if (!ms) return "";
  const sec = Math.max(0, Math.round((now - ms) / 1000));
  if (sec < 5) return t("time.justNow");
  if (sec < 60) return t("time.secondsAgo", { n: sec });
  const min = Math.floor(sec / 60);
  if (min < 60) return t("time.minutesAgo", { n: min });
  const hr = Math.floor(min / 60);
  if (hr < 24) return t("time.hoursAgo", { n: hr });
  return t("time.daysAgo", { n: Math.floor(hr / 24) });
}

function sameLocalDay(a: Date, b: Date): boolean {
  return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth() && a.getDate() === b.getDate();
}

/** Offset from a run start as `+M:SS`; a local date prefix is added when the event is on another calendar day. */
export function formatRunOffset(value: number | null | undefined, origin: number | null | undefined, lang: "zh" | "en"): string {
  const ms = toEpochMs(value);
  const start = toEpochMs(origin);
  const elapsed = formatElapsed(ms - start);
  const event = new Date(ms);
  if (sameLocalDay(event, new Date(start))) return `+${elapsed}`;
  const date = lang === "zh"
    ? `${event.getMonth() + 1}月${event.getDate()}日`
    : event.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  return `${date} +${elapsed}`;
}
