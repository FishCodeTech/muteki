export function compactNumber(value: number): string {
  if (value < 1_000) return Math.round(value).toLocaleString("zh-CN");
  if (value < 1_000_000) {
    const scaled = value / 1_000;
    return `${scaled >= 100 ? scaled.toFixed(0) : scaled.toFixed(1)}K`;
  }
  const scaled = value / 1_000_000;
  return `${scaled >= 100 ? scaled.toFixed(0) : scaled.toFixed(2)}M`;
}

export function formatDuration(valueMs: number): string {
  const seconds = valueMs / 1_000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = Math.floor(seconds / 60);
  const remaining = Math.round(seconds % 60);
  return `${minutes}m ${remaining}s`;
}

export function formatBytes(size?: number): string {
  if (!size || size <= 0) return "";
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${Math.round(size / 1024)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

export function relativeTime(input?: string | number): string {
  if (input === undefined || input === "") return "";
  const time = typeof input === "number" ? input : new Date(input).getTime();
  if (!Number.isFinite(time)) return "";
  const diff = (Date.now() - time) / 1_000;
  if (diff < 5) return "刚刚";
  if (diff < 60) return `${Math.round(diff)} 秒前`;
  if (diff < 3_600) return `${Math.floor(diff / 60)} 分钟前`;
  if (diff < 86_400) return `${Math.floor(diff / 3_600)} 小时前`;
  if (diff < 86_400 * 7) return `${Math.floor(diff / 86_400)} 天前`;
  return new Date(time).toLocaleDateString("zh-CN", { month: "short", day: "numeric" });
}

export function pathBasename(path: string): string {
  const trimmed = path.replace(/\/$/, "");
  if (!trimmed) return "";
  return trimmed.split("/").filter(Boolean).at(-1) || trimmed;
}

export function fileExtension(name: string): string {
  const match = /\.([a-z0-9]{1,6})$/i.exec(name);
  return match ? match[1].toUpperCase() : "";
}
