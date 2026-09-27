/**
 * #208 — preview navigation ownership.
 *
 * Distinguish requested / in-flight / last-committed / failed targets so a
 * readable old document cannot overwrite address, history, or retry after
 * timeout. Pure helpers for PreviewSurface + Node self-checks.
 */

export type PreviewLoadState =
  | "idle"
  | "loading"
  | "loaded"
  | "loaded_uncertain"
  | "timeout";

/** SPA location poll runs only after the current nav has committed a matching doc. */
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

/**
 * Whether a readable frame href may overwrite address/history as an in-page SPA nav.
 * Blocked while a navigation is still uncommitted (loading / timeout / stopped mid-nav).
 */
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

/** Keep the in-flight request as the failed target when the load timer fires. */
export function retainFailedTargetOnTimeout(
  pendingTarget: string | null,
  requestedUrl: string,
): string {
  return pendingTarget || requestedUrl;
}

/** Error UI / open-external / retry must point at the failed nav target when present. */
export function resolveErrorRecoveryUrl(
  failedTarget: string | null,
  url: string,
): string {
  return failedTarget || url;
}

/**
 * Retry after timeout must re-navigate to the failed target (replace),
 * never location.reload() of an old readable document that is still showing.
 */
export function retryNavAfterTimeout(
  failedTarget: string | null,
  url: string,
): { target: string; kind: "replace" } | null {
  const target = failedTarget || url;
  if (!target) return null;
  return { target, kind: "replace" };
}

/**
 * A load event from an old readable document must not commit a different pending navigation.
 * Cross-origin / unknown frameHref is not treated as stale (fallback path may still run).
 */
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

/** Bump generation and mark the next in-flight target when a intentional nav starts. */
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

/** Mark the current generation as committed to a document URL. */
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
