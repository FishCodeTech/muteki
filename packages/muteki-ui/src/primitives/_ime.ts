"use client";

let composing = false;
let lastCompositionEndAt = -Infinity;
let installed = false;

/**
 * Ensures IME composition tracking is active in browsers and WKWebView so
 * pressing Escape to cancel a CJK IME candidate never dismisses an overlay.
 */
export function ensureImeCompositionTracking(): void {
  if (installed || typeof document === "undefined") return;
  installed = true;
  document.addEventListener(
    "compositionstart",
    () => {
      composing = true;
    },
    true,
  );
  document.addEventListener(
    "compositionend",
    () => {
      composing = false;
      lastCompositionEndAt = typeof performance !== "undefined" ? performance.now() : Date.now();
    },
    true,
  );
}

export function isImeEscapeEvent(event: Event | undefined): boolean {
  if (!event) return composing;
  const kb = event as KeyboardEvent;
  if (kb.isComposing || kb.keyCode === 229 || composing) return true;
  const now = typeof performance !== "undefined" ? performance.now() : Date.now();
  return now - lastCompositionEndAt < 45;
}

export function guardOverlayDismiss(
  nextOpen: boolean,
  details: { reason?: string; event?: Event; cancel?: () => void } | undefined,
  options?: { dismissible?: boolean },
): boolean {
  if (nextOpen) return false;
  if (details?.reason === "escape-key") {
    if (isImeEscapeEvent(details.event)) {
      details.cancel?.();
      return true;
    }
    const target = details.event?.target;
    if (target instanceof Element && target.closest("[data-escape-clears]")) {
      details.cancel?.();
      return true;
    }
  }
  if (options?.dismissible === false && (details?.reason === "outside-press" || details?.reason === "escape-key")) {
    details.cancel?.();
    return true;
  }
  return false;
}
