
export type BrowserEmbedAccess =
  | "readable_content"
  | "readable_empty"
  | "unreadable";

export type BrowserEmbedLoadResult = {
  loadState: "loaded" | "loaded_uncertain";
  autoExpandBlockedHint: boolean;
};

export function classifyBrowserEmbedLoad(access: BrowserEmbedAccess): BrowserEmbedLoadResult {
  if (access === "readable_content") {
    return { loadState: "loaded", autoExpandBlockedHint: false };
  }
  if (access === "readable_empty") {
    return { loadState: "loaded_uncertain", autoExpandBlockedHint: true };
  }
  return { loadState: "loaded_uncertain", autoExpandBlockedHint: false };
}

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
