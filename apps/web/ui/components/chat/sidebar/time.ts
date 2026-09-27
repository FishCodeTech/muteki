export type TimeBucket = "today" | "yesterday" | "week" | "month" | "earlier";

export const TIME_BUCKETS: readonly TimeBucket[] = ["today", "yesterday", "week", "month", "earlier"];

export function timeBucketLabel(bucket: TimeBucket): string {
  switch (bucket) {
    case "today":
      return "今天";
    case "yesterday":
      return "昨天";
    case "week":
      return "近 7 天";
    case "month":
      return "近 30 天";
    case "earlier":
      return "更早";
    default: {
      const exhaustive: never = bucket;
      return exhaustive;
    }
  }
}

const DAY = 86_400_000;

export function timeBucketOf(timestamp: number, now: Date): TimeBucket {
  const todayStart = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  if (timestamp >= todayStart) return "today";
  if (timestamp >= todayStart - DAY) return "yesterday";
  if (timestamp >= todayStart - 7 * DAY) return "week";
  if (timestamp >= todayStart - 30 * DAY) return "month";
  return "earlier";
}

export function parseTimestamp(value: string | undefined | null): number {
  return Date.parse(String(value || "")) || 0;
}

export function formatCompactRelative(value: string | undefined, nowMs: number): string {
  if (!value) return "";
  const then = Date.parse(value);
  if (!Number.isFinite(then)) return "";
  const delta = Math.max(0, nowMs - then);
  const minute = 60_000;
  const hour = 60 * minute;
  const month = 30 * DAY;
  const year = 365 * DAY;
  if (delta < minute) return "刚刚";
  if (delta < hour) return `${Math.floor(delta / minute)}m`;
  if (delta < DAY) return `${Math.floor(delta / hour)}h`;
  if (delta < month) return `${Math.floor(delta / DAY)}d`;
  if (delta < year) return `${Math.floor(delta / month)}mo`;
  return `${Math.floor(delta / year)}y`;
}
