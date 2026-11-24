
export type IframeLocationRead =
  | { kind: "known"; href: string }
  | { kind: "unknown" };

export type IframeLocationSource = {
  contentWindow: { location: { href: string } } | null;
};

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
