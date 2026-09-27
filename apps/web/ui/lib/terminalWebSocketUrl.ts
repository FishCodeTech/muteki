/** Use the same API base as HTTP requests, including reverse-proxy prefixes. */
export function terminalWebSocketUrl(
  threadId: string,
  sessionId: string,
  ticket: string,
  pageUrl: string,
  apiBase = "",
): string {
  const path = `/api/threads/${encodeURIComponent(threadId)}/workspace/terminal/sessions/${encodeURIComponent(sessionId)}/stream`;
  const url = new URL(`${apiBase.replace(/\/+$/, "")}${path}`, pageUrl);
  if (url.protocol === "https:") url.protocol = "wss:";
  else if (url.protocol === "http:") url.protocol = "ws:";
  else if (url.protocol !== "ws:" && url.protocol !== "wss:") {
    throw new Error("终端 API 地址协议无效");
  }
  if (ticket) url.searchParams.set("ticket", ticket);
  return url.toString();
}
