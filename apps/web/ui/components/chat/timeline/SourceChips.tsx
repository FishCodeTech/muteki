"use client";

import { useMemo, useState, type CSSProperties } from "react";
import { cn } from "@/lib/cn";
import { Tooltip } from "@/components/chat/ui";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";
import type { WebSource } from "./toolPresentation";

const STAGGER_MS = 70;
const MAX_STAGGER_MS = 420;
const STACK_SIZE = 3;

export interface SourceView {
  url: string;
  origin: string;
  host: string;
  /** Hostname as the page shows it, "www." kept. */
  fullHost: string;
  label: string;
  title?: string;
}

function readablePath(pathname: string): string {
  try {
    return decodeURIComponent(pathname);
  } catch {
    return pathname;
  }
}

function sourceView(source: WebSource): SourceView | null {
  try {
    const parsed = new URL(source.url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return null;
    const host = parsed.hostname.replace(/^www\./, "");
    const path = readablePath(parsed.pathname).replace(/\/+$/, "");
    return {
      url: parsed.toString(),
      origin: parsed.origin,
      host,
      fullHost: parsed.hostname,
      label: path && path !== "/" ? `${host}${path}` : host,
      title: source.title?.trim() || undefined,
    };
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

function hostHue(host: string): number {
  let hash = 0;
  for (let index = 0; index < host.length; index += 1) hash = (hash * 31 + host.charCodeAt(index)) >>> 0;
  return hash % 360;
}

/**
 * The site's own favicon over a tinted monogram; the monogram stays when the
 * icon is missing or blocked, so a chip never shows a broken image.
 */
export function SourceFavicon({ source, size = 16, className }: { source: SourceView; size?: number; className?: string }) {
  const [state, setState] = useState<"loading" | "loaded" | "failed">("loading");
  return (
    <span
      aria-hidden
      className={cn("cx-favicon relative grid shrink-0 place-items-center overflow-hidden rounded-full font-semibold", className)}
      style={{ "--cx-fav-h": hostHue(source.host), width: size, height: size, fontSize: Math.round(size * 0.56) } as CSSProperties}
    >
      <span className={cn("transition-opacity duration-200", state === "loaded" && "opacity-0")}>{monogram(source.host)}</span>
      {state !== "failed" ? (
        // eslint-disable-next-line @next/next/no-img-element
        <img
          src={`${source.origin}/favicon.ico`}
          alt=""
          width={size}
          height={size}
          loading="lazy"
          referrerPolicy="no-referrer"
          className={cn("absolute inset-0 size-full bg-cx-elevated object-contain opacity-0 transition-opacity duration-200", state === "loaded" && "opacity-100")}
          onLoad={(event) => setState(event.currentTarget.naturalWidth > 1 ? "loaded" : "failed")}
          onError={() => setState("failed")}
        />
      ) : null}
    </span>
  );
}

/** Web sources as chip views: deduplicated, local preview URLs left to LocalUrlChips. */
export function toSourceViews(sources: readonly WebSource[]): SourceView[] {
  const seen = new Set<string>();
  const out: SourceView[] = [];
  for (const source of sources) {
    if (isLocalPreviewUrl(source.url)) continue;
    const view = sourceView(source);
    if (!view || seen.has(view.url)) continue;
    seen.add(view.url);
    out.push(view);
  }
  return out;
}

/**
 * Where the agent looked, at a glance: site marks overlapping in a row, the
 * rest folded into a count. Each mark names its page on hover and lifts clear
 * of its neighbours.
 */
export function SourceStack({ sources, max = 6, animate = true }: { sources: SourceView[]; max?: number; animate?: boolean }) {
  const shown = sources.slice(0, max);
  const rest = sources.length - shown.length;
  return (
    <span className="inline-flex items-center" data-testid="cx-source-stack">
      <span className="flex -space-x-1">
        {shown.map((source, index) => (
          <span
            key={source.url}
            title={source.title ? `${source.title}\n${source.fullHost}` : source.fullHost}
            className={cn("cx-stack-mark relative rounded-full ring-2 ring-cx-bg", animate && "cx-stack-in")}
            style={animate ? { animationDelay: `${120 + index * 60}ms` } : undefined}
          >
            <SourceFavicon source={source} size={18} />
          </span>
        ))}
      </span>
      {rest > 0 ? (
        <span
          className={cn("cx-tabular ml-1.5 text-[12px] text-cx-fg-4", animate && "cx-stack-in")}
          style={animate ? { animationDelay: `${120 + shown.length * 60}ms` } : undefined}
        >
          +{rest}
        </span>
      ) : null}
    </span>
  );
}

/**
 * Compact, link-out chips for web sources. Local preview URLs are excluded
 * because LocalUrlChips owns those.
 */
export function SourceChips({
  urls,
  sources: given,
  label = "path",
  maxVisible = 4,
  animate = true,
  className,
}: {
  urls?: string[];
  sources?: WebSource[];
  /** "path": host and path; "host": the hostname alone, the way search results list sites. */
  label?: "path" | "host";
  maxVisible?: number;
  /** Chips cascade in; off for history that was already on screen. */
  animate?: boolean;
  className?: string;
}) {
  const [showAll, setShowAll] = useState(false);
  const sources = useMemo(() => toSourceViews(given ?? (urls ?? []).map((url) => ({ url }))), [given, urls]);

  if (!sources.length) return null;
  const visible = showAll ? sources : sources.slice(0, maxVisible);
  const hidden = sources.slice(visible.length);
  const delay = (index: number): CSSProperties | undefined => (
    animate ? { animationDelay: `${Math.min(index * STAGGER_MS, MAX_STAGGER_MS)}ms` } : undefined
  );

  return (
    <div className={cn("flex flex-wrap items-center gap-1.5", className)} data-testid="cx-source-chips">
      {visible.map((source, index) => (
        <Tooltip
          key={source.url}
          content={(
            <span className="flex max-w-[320px] flex-col gap-0.5">
              {source.title ? <span className="text-[12px] font-medium">{source.title}</span> : null}
              <span className="break-all font-cx-mono text-[11px] opacity-80">{source.url}</span>
            </span>
          )}
        >
          <a
            href={source.url}
            target="_blank"
            rel="noreferrer"
            className={cn(
              "cx-press inline-flex h-7 min-w-0 max-w-[260px] items-center gap-1.5 rounded-full bg-cx-hover pl-1.5 pr-2.5 text-[12px] text-cx-fg-2 hover:bg-cx-active hover:text-cx-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cx-focus",
              animate && "cx-source-chip",
            )}
            style={delay(index)}
          >
            <SourceFavicon source={source} />
            <span className="min-w-0 truncate">{label === "host" ? source.fullHost : source.label}</span>
          </a>
        </Tooltip>
      ))}
      {hidden.length > 0 || showAll ? (
        <button
          type="button"
          aria-expanded={showAll}
          onClick={() => setShowAll((value) => !value)}
          className={cn(
            "cx-press inline-flex h-7 items-center gap-1.5 rounded-full bg-cx-hover px-2.5 text-[12px] text-cx-fg-3 hover:bg-cx-active hover:text-cx-fg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-cx-focus",
            hidden.length > 0 && "pl-1.5",
            animate && "cx-source-chip",
          )}
          style={delay(visible.length)}
        >
          {hidden.length > 0 ? (
            <span aria-hidden className="flex -space-x-1.5">
              {hidden.slice(0, STACK_SIZE).map((source) => (
                <SourceFavicon key={source.url} source={source} className="ring-2 ring-cx-hover" />
              ))}
            </span>
          ) : null}
          {showAll ? "收起" : `还有 ${hidden.length} 个`}
        </button>
      ) : null}
    </div>
  );
}
