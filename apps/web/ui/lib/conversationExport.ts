export interface ConversationExportMetadata {
  complete: true; schema_version: 2; scope: "current_branch"; binary_content_included: false;
  message_count: number; turn_count: number; artifact_count: number; artifact_count_kind: "associations";
  tool_count: number; watermark: number; path_redaction: "lexical_best_effort" | null;
}

/** Verify the service's metadata for the same transactional snapshot as bytes. */
export function conversationExportMetadata(headers: Pick<Headers, "get">): ConversationExportMetadata {
  const header = headers.get("X-Muteki-Export-Metadata");
  if (!header) throw new Error("导出缺少完整性统计，请更新服务后重试。");
  let value: unknown;
  try { value = JSON.parse(header); } catch { throw new Error("导出完整性统计格式无效。"); }
  const row = value as Partial<ConversationExportMetadata> | null;
  if (!row || row.complete !== true || row.schema_version !== 2 || row.scope !== "current_branch" || row.binary_content_included !== false || row.artifact_count_kind !== "associations"
      || !["message_count", "turn_count", "artifact_count", "tool_count", "watermark"].every(key => Number.isSafeInteger((row as Record<string, unknown>)[key]) && Number((row as Record<string, unknown>)[key]) >= 0)
      || (row.path_redaction !== null && row.path_redaction !== "lexical_best_effort")) throw new Error("服务未返回受支持的完整会话快照，导出未保存。");
  return row as ConversationExportMetadata;
}

export function localExportDate(date: Date): string {
  return `${date.getFullYear()}${String(date.getMonth() + 1).padStart(2, "0")}${String(date.getDate()).padStart(2, "0")}`;
}

export function conversationExportFilename(input: { contentDisposition?: string | null; title: string; format: "markdown" | "jsonl"; excludePaths: boolean; date: Date }): string {
  const ext = input.format === "jsonl" ? "jsonl" : "md";
  let serverName = "";
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(input.contentDisposition || "")?.[1];
  try { serverName = encoded ? decodeURIComponent(encoded) : /filename="([^"]+)"/i.exec(input.contentDisposition || "")?.[1] || ""; } catch { /* optional presentation metadata, never permission */ }
  const title = serverName ? serverName.replace(/-\d{8}\.(?:md|jsonl)$/i, "") : input.excludePaths ? "conversation" : input.title;
  // This is a filename label, never the message or evidence body.
  const safe = title.replace(/[<>:"/\\|?*\x00-\x1f]/g, "").trim().slice(0, 80) || "export";
  return `${safe.replace(/\s+/g, "-")}-${localExportDate(input.date)}.${ext}`;
}
