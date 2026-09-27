/** Conversation deep-link helpers shared by C07 search jump and C08 reading position. */

export function buildThreadMessageHref(threadId: string, messageId: string): string {
  const id = encodeURIComponent(threadId);
  const params = new URLSearchParams({ message: messageId });
  return `/chat/${id}?${params.toString()}`;
}

export function readMessageDeepLink(
  searchParams: URLSearchParams | { get(name: string): string | null },
): string {
  return String(searchParams.get("message") || "").trim();
}

export function replaceMessageDeepLink(messageId: string | null): void {
  if (typeof window === "undefined") return;
  const url = new URL(window.location.href);
  if (messageId) url.searchParams.set("message", messageId);
  else url.searchParams.delete("message");
  window.history.replaceState(window.history.state, "", `${url.pathname}${url.search}${url.hash}`);
}
