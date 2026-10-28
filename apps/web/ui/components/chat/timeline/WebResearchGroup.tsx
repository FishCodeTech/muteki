"use client";

import { useMemo, useState, type CSSProperties } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Collapse, ShimmerText } from "@/components/chat/ui";
import { SourceChips, SourceFavicon, SourceStack, toSourceViews } from "./SourceChips";
import { CountUp, SwapText, useEver, useFreshKeys } from "./turnMotion";
import {
  isWebSearchTool,
  parseToolArgs,
  toolActivityRows,
  toolActivityText,
  toolWebSources,
  type PresentableTool,
  type ToolActivityRow,
} from "./toolPresentation";

/** The Sources row lands a beat after the search it belongs to. */
const SOURCES_BEAT_MS = 160;

function isActive(tool: PresentableTool): boolean {
  return tool.status === "running" || tool.status === "pending";
}

function Chevron({ open }: { open: boolean }) {
  return (
    <span aria-hidden className="cx-chevron inline-flex shrink-0" data-open={open || undefined}>
      <Icon name="chevronRight" size={12} />
    </span>
  );
}

/** A step that settles in front of the reader lands tinted in the accent hues. */
function RowLabel({ row, active, tint }: { row: ToolActivityRow; active: boolean; tint: boolean }) {
  const text = toolActivityText(row);
  return (
    <SwapText value={`${active ? "active" : "done"}:${text}`} className="min-w-0">
      {active ? <ShimmerText className="inline">{text}</ShimmerText> : (
        <>
          {row.parts.map((part, index) => (
            <span key={index} className={cn(tint && "cx-hue-in", part.tone === "verb" && "text-cx-fg-2", part.tone === "mono" && "font-cx-mono text-[12px]")}>
              {part.text}
            </span>
          ))}
        </>
      )}
    </SwapText>
  );
}

function SourcesRow({ tool, animate }: { tool: PresentableTool; animate: boolean }) {
  const [open, setOpen] = useState(false);
  const sources = useMemo(() => toSourceViews(toolWebSources(tool)), [tool]);
  if (!sources.length) return null;
  const style: CSSProperties | undefined = animate ? { animationDelay: `${SOURCES_BEAT_MS}ms` } : undefined;
  return (
    <ul className="cx-tree" data-live={animate || undefined}>
      <li className={cn("cx-tree-item", animate && "cx-reveal")} data-fresh={animate || undefined} style={style}>
        <div className="min-w-0">
        <button
          type="button"
          onClick={() => setOpen((value) => !value)}
          aria-expanded={open}
          aria-label={`来源，共 ${sources.length} 个`}
          className="group/sources flex h-7 max-w-full items-center gap-2 rounded-md text-[13px] text-cx-fg-3 outline-none transition-colors hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus"
        >
          <span>来源</span>
          <SourceStack sources={sources} animate={animate} />
          <Chevron open={open} />
        </button>
        <Collapse open={open} enterDuration={0.26} exitDuration={0.18}>
          <SourceChips sources={sources.map((source) => ({ url: source.url, title: source.title }))} label="host" maxVisible={12} className="pb-1.5 pt-0.5" />
        </Collapse>
        </div>
      </li>
    </ul>
  );
}

function StepIcon({ tool }: { tool: PresentableTool }) {
  if (tool.status === "failed") return <Icon name="x" size={13} className="shrink-0 text-cx-danger" title="执行失败" />;
  if (isWebSearchTool(tool)) return <Icon name="globe" size={13} className="shrink-0 text-cx-fg-3" />;
  const url = String(parseToolArgs(tool.argsSummary).url || "");
  const [view] = url ? toSourceViews([{ url }]) : [];
  return view ? <SourceFavicon source={view} size={14} /> : <Icon name="globe" size={13} className="shrink-0 text-cx-fg-3" />;
}

function ResearchStep({
  tool,
  fresh,
  onSelect,
}: {
  tool: PresentableTool;
  fresh: boolean;
  onSelect?: () => void;
}) {
  const active = isActive(tool);
  const sawActive = useEver(active);
  const row = toolActivityRows(tool)[0];
  const search = isWebSearchTool(tool);
  const results = search && !active ? toolWebSources(tool).length : 0;
  const animate = fresh || sawActive;
  return (
    <li className={cn("cx-tree-item min-w-0", fresh && "cx-reveal")} data-fresh={fresh || undefined} data-testid="cx-research-step" data-status={tool.status}>
      <div className="min-w-0">
      <div className="flex h-7 min-w-0 items-center gap-2 text-[13px] text-cx-fg-3">
        <StepIcon tool={tool} />
        <button
          type="button"
          onClick={onSelect}
          disabled={!onSelect}
          title={row.title}
          className="min-w-0 truncate rounded-md text-left outline-none transition-colors hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus disabled:cursor-default"
        >
          <RowLabel row={row} active={active} tint={sawActive} />
        </button>
        <span className="flex-1" />
        {results ? (
          <span className={cn("cx-tabular shrink-0 text-[12px] text-cx-fg-4", sawActive && "cx-enter-soft")}>
            <CountUp value={results} from={animate ? 0 : undefined} /> 个结果
          </span>
        ) : null}
      </div>
      {tool.status === "failed" && tool.error ? (
        <p className={cn("mb-1 line-clamp-2 break-words font-cx-mono text-[12px] leading-5 text-cx-danger", sawActive && "cx-enter")}>{String(tool.error)}</p>
      ) : null}
      {search ? <SourcesRow tool={tool} animate={animate} /> : null}
      </div>
    </li>
  );
}

function researchSummary(tools: PresentableTool[]): string {
  const searches = tools.filter((tool) => isWebSearchTool(tool)).length;
  const pages = tools.length - searches;
  return [searches ? `搜索了 ${searches} 次网页` : "", pages ? `浏览了 ${pages} 个网页` : ""].filter(Boolean).join("，");
}

/**
 * Web research as a trail: a heading, then each search and page read on a
 * curved guide, the newest landing at the bottom. A search that found pages
 * branches into a Sources row whose site marks open into link chips.
 */
export function WebResearchGroup({
  tools,
  live = false,
  liveFallback = "正在思考",
  onSelectTool,
}: {
  tools: PresentableTool[];
  live?: boolean;
  liveFallback?: string;
  onSelectTool?: (tool: PresentableTool) => void;
}) {
  const [open, setOpen] = useState(true);
  const anyActive = tools.some(isActive);
  const isFresh = useFreshKeys(tools.map((tool) => tool.id), live || anyActive);
  const summary = researchSummary(tools);
  const label = live ? "正在查资料" : summary;
  // Between calls the trail ends on what the agent is thinking about next.
  const thinking = live && !anyActive;
  return (
    <div className="min-w-0" data-testid="cx-web-research" data-live={live || undefined}>
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        title={summary}
        className="flex h-7 min-w-0 max-w-full items-center gap-2 rounded-md text-left text-[13px] text-cx-fg-3 outline-none transition-colors hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus"
      >
        <Icon name="search" size={13} className="shrink-0" />
        <SwapText value={`${live ? "live" : "done"}:${label}`} className="min-w-0">
          {live ? <ShimmerText className="inline">{label}</ShimmerText> : label}
        </SwapText>
        <Chevron open={open} />
      </button>
      <Collapse open={open} enterDuration={0.28} exitDuration={0.22}>
        <ul className="cx-tree" data-live={live || anyActive || undefined}>
          {tools.map((tool) => (
            <ResearchStep
              key={tool.id}
              tool={tool}
              fresh={isFresh(tool.id)}
              onSelect={onSelectTool ? () => onSelectTool(tool) : undefined}
            />
          ))}
        </ul>
        {thinking ? (
          <div className="cx-enter flex h-7 items-center pl-[26px] text-[13px] text-cx-fg-3" data-testid="cx-web-research-live">
            <SwapText value={liveFallback}><ShimmerText className="inline">{liveFallback}</ShimmerText></SwapText>
          </div>
        ) : null}
      </Collapse>
    </div>
  );
}
