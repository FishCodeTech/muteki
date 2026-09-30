import type { ConversationSearchHit } from "./useConversation";

/** Stable message identity prevents duplicate rows when a page overlaps an earlier snapshot. */
export function mergeSidebarSearchHits(previous: ConversationSearchHit[], page: ConversationSearchHit[]): ConversationSearchHit[] {
  const result = [...previous];
  const positions = new Map(result.map((hit, index) => [`${hit.thread_id}:${hit.message_id}`, index]));
  for (const hit of page) {
    const key = `${hit.thread_id}:${hit.message_id}`;
    const position = positions.get(key);
    if (position != null) result[position] = hit;
    else { positions.set(key, result.length); result.push(hit); }
  }
  return result;
}

export function sidebarSearchFailure(error: unknown): string {
  if (error instanceof Error) {
    const code = (error as Error & { code?: string }).code;
    return code && !error.message.includes(code) ? `${code}：${error.message}` : error.message;
  }
  return typeof error === "string" ? error : "conversation.search.failed：正文搜索未完成，请重试";
}
