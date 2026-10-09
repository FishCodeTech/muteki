"use client";

import { useEffect, useMemo } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { chatPanel } from "@/lib/chatPanelStore";
import { extractLocalUrls, previewUrlLabel } from "@/lib/previewUrlDetect";

/** Survives virtualized remounts so each URL is reported once per session. */
const reported = new Set<string>();

export function openLocalUrl(threadId: string, url: string, event?: { metaKey?: boolean; ctrlKey?: boolean; shiftKey?: boolean }) {
  if (!threadId || event?.metaKey || event?.ctrlKey || event?.shiftKey) {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  chatPanel.openPreview(threadId, url);
}

export function LocalUrlChips({
  threadId,
  text,
  source,
  disabled = false,
  className,
}: {
  threadId?: string;
  text: string;
  source: "tool" | "message" | "terminal";
  /** Skip detection while text is still streaming (partial URLs). */
  disabled?: boolean;
  className?: string;
}) {
  const urls = useMemo(() => (disabled ? [] : extractLocalUrls(text, 4)), [disabled, text]);

  useEffect(() => {
    if (!threadId) return;
    for (const url of urls) {
      const key = `${threadId}|${url}`;
      if (reported.has(key)) continue;
      reported.add(key);
      chatPanel.reportDetectedUrl(threadId, url, source);
    }
  }, [source, threadId, urls]);

  if (!threadId || !urls.length) return null;

  return (
    <div className={cn("flex flex-wrap items-center gap-1.5", className)} data-testid="cx-local-url-chips">
      {urls.map((url) => (
        <button
          key={url}
          type="button"
          onClick={(event) => openLocalUrl(threadId, url, event)}
          title="在预览面板中打开（⌘/Ctrl+点击在新标签页打开）"
          className="cx-press group inline-flex h-7 max-w-full items-center gap-1.5 rounded-full border border-cx-border bg-cx-elevated pl-2 pr-2.5 text-[12px] font-medium text-cx-fg-2 shadow-cx-xs hover:border-cx-border-strong hover:text-cx-fg"
        >
          <Icon name="globe" size={13} className="shrink-0 text-cx-accent" />
          <span className="text-cx-fg-3 group-hover:text-cx-fg-2">在预览中打开</span>
          <span className="min-w-0 truncate font-cx-mono text-[12px]">{previewUrlLabel(url)}</span>
          <Icon name="arrowUpRight" size={12} className="shrink-0 text-cx-fg-4 group-hover:text-cx-fg-3" />
        </button>
      ))}
    </div>
  );
}
