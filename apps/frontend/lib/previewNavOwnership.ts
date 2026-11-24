
export type PreviewLoadState =
  | "idle"
  | "loading"
  | "loaded"
  | "loaded_uncertain"
  | "timeout";

export function shouldPollSpaLocation(args: {
  active: boolean;
  trackable: boolean;
  hasUrl: boolean;
  loadState: PreviewLoadState;
  navCommitted: boolean;
}): boolean {
  if (!args.active || !args.trackable || !args.hasUrl) return false;
  if (!args.navCommitted) return false;
  return args.loadState === "loaded" || args.loadState === "loaded_uncertain";
}

export function shouldAcceptSpaFrameHref(args: {
  frameHref: string;
  currentUrl: string;
  navCommitted: boolean;
  pendingTarget: string | null;
}): boolean {
  if (!args.navCommitted) return false;
  if (args.pendingTarget) return false;
  return args.frameHref !== args.currentUrl;
}

export function retainFailedTargetOnTimeout(
  pendingTarget: string | null,
  requestedUrl: string,
): string {
  return pendingTarget || requestedUrl;
}

export function resolveErrorRecoveryUrl(
  failedTarget: string | null,
  url: string,
): string {
  return failedTarget || url;
}

export function retryNavAfterTimeout(
  failedTarget: string | null,
  url: string,
): { target: string; kind: "replace" } | null {
  const target = failedTarget || url;
  if (!target) return null;
  return { target, kind: "replace" };
}

export function isStaleDocumentForPendingNav(args: {
  frameHref: string | null;
  pendingTarget: string | null;
  committedDocUrl: string;
  navGeneration: number;
  eventGeneration: number;
}): boolean {
  if (args.eventGeneration !== args.navGeneration) return true;
  if (!args.pendingTarget) return false;
  if (args.frameHref == null) return false;
  return (
    args.frameHref === args.committedDocUrl &&
    args.frameHref !== args.pendingTarget
  );
}

export function beginNavGeneration(
  currentGeneration: number,
  target: string,
): { generation: number; pendingTarget: string; navCommitted: false } {
  return {
    generation: currentGeneration + 1,
    pendingTarget: target,
    navCommitted: false,
  };
}

export function commitNavDocument(args: {
  generation: number;
  eventGeneration: number;
  docUrl: string;
}): { ok: true; committedDocUrl: string; pendingTarget: null; failedTarget: null; navCommitted: true } | { ok: false } {
  if (args.eventGeneration !== args.generation) return { ok: false };
  return {
    ok: true,
    committedDocUrl: args.docUrl,
    pendingTarget: null,
    failedTarget: null,
    navCommitted: true,
  };
}
