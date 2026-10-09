"use client";

import { useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Tooltip } from "@/components/chat/ui";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";

const STAGGER_MS = 70;
const MAX_STAGGER_MS = 420;

interface SourceView {
  url: string;
  host: string;
  label: string;
}

function readablePath(pathname: string): string {
  try {
    return decodeURIComponent(pathname);
  } catch {
    return pathname;
  }
}

function sourceView(url: string): SourceView | null {
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
    const host = parsed.hostname.replace(/^www\./, "");
    const path = readablePath(parsed.pathname).replace(/\/+$/, "");
    return { url: parsed.toString(), host, label: path && path !== "/" ? `${host}${path}` : host };
  } catch {
    return null;
  }
}

/** First letter of the registrable-ish name: "en.wikipedia.org" → "W", "nextjs.org" → "N". */
function monogram(host: string): string {
  const parts = host.split(".").filter(Boolean);
  const name = parts.length > 2 ? parts[parts.length - 2] : parts[0] || host;
  return (name.match(/[a-z0-9]/i)?.[0] || "?").toUpperCase();
}

/**
 * Compact, link-out chips for web sources. Local preview URLs are excluded
 * because LocalUrlChips owns those.
 */
export function SourceChips({
  urls,
  maxVisible = 4,
  className,
}: {
  urls: string[];
  maxVisible?: number;
  className?: string;
}) {
  const [showAll, setShowAll] = useState(false);
  const sources = useMemo(() => {
    const seen = new Set<string>();
    const out: SourceView[] = [];
    for (const url of urls) {
      if (isLocalPreviewUrl(url)) continue;
      const view = sourceView(url);
      if (!view || seen.has(view.url)) continue;
      seen.add(view.url);
      out.push(view);
    }
    return out;
  }, [urls]);

  if (!sources.length) return null;
  const visible = showAll ? sources : sources.slice(0, maxVisible);
  const hidden = sources.length - visible.length;

  return (
    <div className={cn("flex flex-wrap items-center gap-1.5", className)} data-testid="cx-source-chips">
      {visible.map((source, index) => (
        <Tooltip key={source.url} content={<span className="break-all font-cx-mono text-[11px]">{source.url}</span>}>
          <a
            href={source.url}
            target="_blank"
            rel="noreferrer"
            className="cx-source-chip cx-press inline-flex h-7 min-w-0 max-w-[260px] items-center gap-1.5 rounded-lg bg-cx-hover pl-1.5 pr-2.5 text-[12px] text-cx-fg-2 hover:bg-cx-active hover:text-cx-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cx-focus"
            style={{ animationDelay: `${Math.min(index * STAGGER_MS, MAX_STAGGER_MS)}ms` }}
          >
            <span
              aria-hidden
              className="grid size-[18px] shrink-0 place-items-center rounded-[5px] bg-cx-elevated text-[10px] font-semibold text-cx-fg-3 shadow-cx-xs"
            >
              {monogram(source.host)}
            </span>
            <span className="min-w-0 truncate">{source.label}</span>
          </a>
        </Tooltip>
      ))}
      {hidden > 0 || showAll ? (
        <button
          type="button"
          aria-expanded={showAll}
          onClick={() => setShowAll((value) => !value)}
          className="cx-source-chip cx-press inline-flex h-7 items-center rounded-lg px-2.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cx-focus"
          style={{ animationDelay: `${Math.min(visible.length * STAGGER_MS, MAX_STAGGER_MS)}ms` }}
        >
          {showAll ? "收起" : `+${hidden} 个`}
        </button>
      ) : null}
    </div>
  );
}
