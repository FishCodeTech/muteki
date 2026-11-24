export interface SrcDebugLogsMetadata {
  schema: string;
  task_id: string;
  src_collection_count: number;
  src_document_count: number;
  thread_count: number;
  chat_event_count: number;
  related_document_count: number;
  included_log_count: number;
}

const METADATA_INT_KEYS: Array<keyof Omit<SrcDebugLogsMetadata, "schema" | "task_id">> = [
  "src_collection_count",
  "src_document_count",
  "thread_count",
  "chat_event_count",
  "related_document_count",
  "included_log_count",
];

export function parseSrcDebugLogsMetadata(header: string | null): SrcDebugLogsMetadata | null {
  if (!header) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(header);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return null;
  const row = parsed as Record<string, unknown>;
  if (typeof row.schema !== "string" || !row.schema.trim()) return null;
  if (typeof row.task_id !== "string" || !row.task_id.trim()) return null;
  for (const key of METADATA_INT_KEYS) {
    const val = row[key];
    if (!Number.isSafeInteger(val) || Number(val) < 0) return null;
  }
  return {
    schema: row.schema,
    task_id: row.task_id,
    src_collection_count: row.src_collection_count as number,
    src_document_count: row.src_document_count as number,
    thread_count: row.thread_count as number,
    chat_event_count: row.chat_event_count as number,
    related_document_count: row.related_document_count as number,
    included_log_count: row.included_log_count as number,
  };
}

function pad2(n: number): string {
  return String(n).padStart(2, "0");
}

function localTimestamp(date: Date): string {
  const valid = Number.isFinite(date.getTime()) ? date : new Date(0);
  const y = valid.getFullYear();
  const m = pad2(valid.getMonth() + 1);
  const d = pad2(valid.getDate());
  const hh = pad2(valid.getHours());
  const mm = pad2(valid.getMinutes());
  const ss = pad2(valid.getSeconds());
  return `${y}${m}${d}-${hh}${mm}${ss}`;
}

function sanitizeFilename(raw: string): string {
  const stripped = raw
    .replace(/[<>:"/\\|?*\x00-\x1f]/g, "-")
    .replace(/\.\.+/g, ".")
    .replace(/\s+/g, "-")
    .replace(/^[-.]+|[-.]+$/g, "");
  return stripped.slice(0, 120);
}

export function srcDebugLogsFilename(input: {
  contentDisposition?: string | null;
  taskId: string;
  date: Date;
}): string {
  const cd = input.contentDisposition || "";
  let serverName = "";
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(cd)?.[1];
  try {
    if (encoded) {
      serverName = decodeURIComponent(encoded.trim().replace(/^["']|["']$/g, ""));
    } else {
      serverName = /filename="([^"]+)"/i.exec(cd)?.[1] || /filename=([^;\s]+)/i.exec(cd)?.[1] || "";
    }
  } catch {
    serverName = "";
  }
  const safeServer = sanitizeFilename(serverName)
    .replace(/\.tar\.gz$/i, "")
    .replace(/^[-.]+|[-.]+$/g, "");
  if (safeServer) {
    return `${safeServer.slice(0, 113)}.tar.gz`;
  }
  const safeTask = sanitizeFilename(input.taskId).slice(0, 48) || "task";
  return `muteki-src-debug-${safeTask}-${localTimestamp(input.date)}.tar.gz`;
}

export function formatBytesCompact(sizeBytes: number): string {
  if (!Number.isFinite(sizeBytes) || sizeBytes <= 0) return "0 B";
  if (sizeBytes < 1024) return `${Math.round(sizeBytes)} B`;
  if (sizeBytes < 1024 * 1024) return `${(sizeBytes / 1024).toFixed(1)} KB`;
  return `${(sizeBytes / (1024 * 1024)).toFixed(2)} MB`;
}

export function formatSrcDebugLogsSummary(input: {
  filename: string;
  sizeBytes: number;
  metadata: SrcDebugLogsMetadata | null;
}): string {
  const sizeLabel = formatBytesCompact(input.sizeBytes);
  if (!input.metadata) {
    return `${input.filename}（${sizeLabel}）`;
  }
  return `${input.filename}（${sizeLabel}）· ${input.metadata.thread_count} 条对话线程 · ${input.metadata.chat_event_count} 条事件 · ${input.metadata.src_collection_count} 个 SRC 集合`;
}
