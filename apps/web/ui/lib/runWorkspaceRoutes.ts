/**
 * Conversation / runtime / report hrefs for a run, keyed by CTF vs pentest
 * workspace. Collaboration always lives at /run/<id>/collaboration; the other
 * surfaces differ so pentest does not bounce through /run/<id> + redirect.
 */

export type RunWorkspaceMode = "ctf" | "pentest";

export function runIdFromPathname(pathname: string): string | null {
  const match = pathname.match(/^\/run\/([^/]+)/);
  return match ? decodeURIComponent(match[1]) : null;
}

export function conversationHref(runId: string, mode: RunWorkspaceMode): string {
  if (mode === "pentest") return `/pentest?run=${encodeURIComponent(runId)}`;
  return `/run/${encodeURIComponent(runId)}`;
}

export function runtimeHref(
  runId: string,
  mode: RunWorkspaceMode,
  view: string,
  focus?: [string, string | number],
): string {
  const params = new URLSearchParams({ view });
  if (focus) params.set(focus[0], String(focus[1]));
  if (mode === "pentest") {
    params.set("run", runId);
    return `/pentest?${params.toString()}`;
  }
  return `/run/${encodeURIComponent(runId)}?${params.toString()}`;
}

export function reportHref(runId: string): string {
  return `/pentest?run=${encodeURIComponent(runId)}&view=report`;
}

export function asRunWorkspaceMode(value: string | null | undefined): RunWorkspaceMode | null {
  if (value === "pentest" || value === "ctf") return value;
  return null;
}
