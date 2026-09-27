/**
 * Embedded browser location tracking (#140).
 *
 * Same-origin iframes expose contentWindow.location; cross-origin does not.
 * Never attempt to bypass that boundary — only sync when href is readable.
 */

export type IframeLocationRead =
  | { kind: "known"; href: string }
  | { kind: "unknown" };

/** Minimal iframe shape so Node self-checks can mock without DOM. */
export type IframeLocationSource = {
  contentWindow: { location: { href: string } } | null;
};

/**
 * Read the iframe's current location when same-origin allows it.
 * Returns unknown for cross-origin, missing window, or about:blank.
 */
export function readIframeLocation(iframe: IframeLocationSource): IframeLocationRead {
  try {
    const href = iframe.contentWindow?.location?.href;
    if (!href || href === "about:blank") return { kind: "unknown" };
    return { kind: "known", href };
  } catch {
    return { kind: "unknown" };
  }
}

export type BrowserPreviewActionCopy = {
  refreshAriaLabel: string;
  citeAriaLabel: string;
  citeLine: (url: string, ts: string) => string;
};

/**
 * Cite/refresh copy must not claim "current URL" when the live location
 * cannot be observed (cross-origin). In that case actions apply to the
 * initial / last address-bar URL only.
 */
export function browserPreviewActionCopy(locationTrackable: boolean): BrowserPreviewActionCopy {
  if (locationTrackable) {
    return {
      refreshAriaLabel: "刷新页面",
      citeAriaLabel: "发送当前 URL 和时间戳到对话输入框",
      citeLine: (url, ts) => `[浏览器预览] ${url} · ${ts}`,
    };
  }
  return {
    refreshAriaLabel: "刷新初始地址（页内地址不可跟踪）",
    citeAriaLabel: "发送初始 URL 和时间戳到对话输入框（页内地址不可跟踪）",
    citeLine: (url, ts) => `[浏览器预览·初始地址] ${url} · ${ts}`,
  };
}
