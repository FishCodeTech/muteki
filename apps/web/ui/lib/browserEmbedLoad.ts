/**
 * Browser workspace iframe load classification (#139).
 *
 * Cross-origin embeds fire onLoad but contentDocument is unreadable — that is
 * normal for a successful preview and must NOT be treated as embed failure.
 */

export type BrowserEmbedAccess =
  | "readable_content"
  | "readable_empty"
  | "unreadable";

export type BrowserEmbedLoadResult = {
  loadState: "loaded" | "loaded_uncertain";
  /** Only true for same-origin empty body (likely silent refusal). Never for cross-origin. */
  autoExpandBlockedHint: boolean;
};

export function classifyBrowserEmbedLoad(access: BrowserEmbedAccess): BrowserEmbedLoadResult {
  if (access === "readable_content") {
    return { loadState: "loaded", autoExpandBlockedHint: false };
  }
  if (access === "readable_empty") {
    return { loadState: "loaded_uncertain", autoExpandBlockedHint: true };
  }
  // Unreadable (cross-origin): preview may still render; keep troubleshooting collapsed.
  return { loadState: "loaded_uncertain", autoExpandBlockedHint: false };
}

/** Inspect iframe document access after onLoad. Safe for cross-origin. */
export function inspectIframeAccess(iframe: HTMLIFrameElement): BrowserEmbedAccess {
  try {
    const doc = iframe.contentDocument;
    if (!doc) return "unreadable";
    if (doc.body && doc.body.innerHTML.trim().length > 0) return "readable_content";
    return "readable_empty";
  } catch {
    return "unreadable";
  }
}
