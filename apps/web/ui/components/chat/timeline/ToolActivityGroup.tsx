"use client";

import { memo, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, Collapse, ScrollArea, ShimmerText } from "@/components/chat/ui";
import { ToolResultOutput } from "@/components/agentui/agents/tool-result";
import { LocalUrlChips } from "./LocalUrlChips";
import { WebResearchGroup } from "./WebResearchGroup";
import { CountUp, SwapText, useEver, useFreshKeys, useStickToBottom } from "./turnMotion";
import {
  formatToolDuration,
  summarizeToolActivity,
  toolActivityRows,
  toolActivityText,
  toolEditLines,
  toolEditStat,
  toolOutputText,
  isWebResearchTool,
  type PresentableTool,
  type ToolActivityRow,
} from "./toolPresentation";

const MAX_OUTPUT_CHARS = 20_000;
/** Codex caps an expanded work group at 14rem and scrolls the rest. */
const GROUP_MAX_HEIGHT = "max-h-56";
/** The live tail shows about four rows; older ones glide up under a fade. */
const LIVE_TAIL_HEIGHT = "max-h-[8.75rem]";
const LIVE_TAIL_ROWS = 24;
const PEEK_LINES = 40;
/** A command's output peek opens only once it has run this long, so a burst of quick calls never pumps the list. */
const PEEK_DELAY_MS = 700;
/** Rows of a group opened by the reader cascade in, up to this many. */
const STAGGER_ROWS = 12;

function isActive(tool: PresentableTool): boolean {
  return tool.status === "running" || tool.status === "pending";
}

// Tool payloads are never mutated once folded, so each nested value is
// serialized at most once however many frames compare it.
const toolFieldJson = new WeakMap<object, string>();

function jsonOf(value: object): string {
  let text = toolFieldJson.get(value);
  if (text === undefined) {
    text = JSON.stringify(value);
    toolFieldJson.set(value, text);
  }
  return text;
}

/**
 * The live event window slides, and a refold rebuilds every tool record even
 * when nothing about the call changed; compare by content instead.
 */
export function sameToolRecord(a: PresentableTool, b: PresentableTool): boolean {
  if (a === b) return true;
  const left = a as unknown as Record<string, unknown>;
  const right = b as unknown as Record<string, unknown>;
  const leftKeys = Object.keys(left);
  if (leftKeys.length !== Object.keys(right).length) return false;
  for (const key of leftKeys) {
    const l = left[key];
    const r = right[key];
    if (!(key in right)) return false;
    if (Object.is(l, r)) continue;
    if (typeof l !== "object" || typeof r !== "object" || !l || !r) return false;
    if (jsonOf(l) !== jsonOf(r)) return false;
  }
  return true;
}

function sameTools(a: readonly PresentableTool[], b: readonly PresentableTool[]): boolean {
  return a === b || (a.length === b.length && a.every((tool, index) => sameToolRecord(tool, b[index])));
}

/** Holds each live label on screen for at least `minMs` so fast calls do not flicker. */
function useDwellLabel(label: string, minMs: number): string {
  const [shown, setShown] = useState(label);
  const shownAt = useRef(0);
  useEffect(() => {
    if (label === shown) return;
    const apply = () => {
      shownAt.current = Date.now();
      setShown(label);
    };
    const wait = minMs - (Date.now() - shownAt.current);
    if (!shown || wait <= 0) {
      apply();
      return;
    }
    const timer = window.setTimeout(apply, wait);
    return () => window.clearTimeout(timer);
  }, [label, shown, minMs]);
  return label ? shown || label : "";
}

/** True once `value` has held for `ms`; false again the moment it drops. */
function useHeld(value: boolean, ms: number): boolean {
  const [held, setHeld] = useState(false);
  useEffect(() => {
    if (!value) {
      setHeld(false);
      return;
    }
    const timer = window.setTimeout(() => setHeld(true), ms);
    return () => window.clearTimeout(timer);
  }, [value, ms]);
  return value && held;
}

function RowParts({ row, tint = false }: { row: ToolActivityRow; tint?: boolean }) {
  return (
    <>
      {row.parts.map((part, index) => (
        <span
          key={index}
          className={cn(
            tint && "cx-hue-in",
            part.tone === "verb" && "text-cx-fg-2",
            part.tone === "mono" && "font-cx-mono text-[12px]",
          )}
        >
          {part.text}
        </span>
      ))}
    </>
  );
}

function StatusMark({ status, pop }: { status: PresentableTool["status"]; pop: boolean }) {
  const mark = status === "failed" ? <Icon name="x" size={12} className="text-cx-danger" title="执行失败" />
    : status === "declined" ? <Icon name="minus" size={12} className="text-cx-warning" title="已拒绝" />
      : status === "cancelled" ? <Icon name="minus" size={12} className="text-cx-fg-4" title="已取消" />
        : null;
  if (!mark) return null;
  return <span className={cn("inline-flex shrink-0", pop && "cx-pop")}>{mark}</span>;
}

function Chevron({ open, className }: { open: boolean; className?: string }) {
  return (
    <span aria-hidden className={cn("cx-chevron inline-flex shrink-0", className)} data-open={open || undefined}>
      <Icon name="chevronRight" size={12} />
    </span>
  );
}

function LiveLabel({ text }: { text: string }) {
  return <ShimmerText className="inline">{text}</ShimmerText>;
}

function RowBody({ children, footer, follow = false }: { children: ReactNode; footer?: ReactNode; follow?: boolean }) {
  const stick = useStickToBottom(follow);
  return (
    <div className="cx-term mb-1 mt-0.5 overflow-hidden rounded-lg">
      <ScrollArea ref={stick} className={cn(GROUP_MAX_HEIGHT, "px-3 py-2")}>{children}</ScrollArea>
      {footer ? <div className="flex items-center gap-2 px-2 pb-1 text-[12px] text-cx-fg-4">{footer}</div> : null}
    </div>
  );
}

/** The newest lines of a running command, gliding up under a fade. */
function LiveOutputPeek({ tool }: { tool: PresentableTool }) {
  const stick = useStickToBottom(true);
  const raw = toolOutputText(tool);
  const lines = raw ? raw.split("\n") : [];
  if (!lines.length) return null;
  const tail = lines.length > PEEK_LINES ? lines.slice(-PEEK_LINES) : lines;
  return (
    <div className="cx-term mb-1 mt-0.5 overflow-hidden rounded-lg" data-testid="cx-tool-peek">
      <div
        ref={stick}
        className={cn(
          "max-h-[5.25rem] overflow-hidden whitespace-pre-wrap break-all px-3 py-1.5 font-cx-mono text-[11.5px] leading-[1.6] text-cx-fg-3",
          lines.length > 3 && "cx-peek-mask",
        )}
      >
        {tail.join("\n")}
      </div>
    </div>
  );
}

function CommandOutput({ tool, threadId, onSelect }: { tool: PresentableTool; threadId?: string; onSelect?: () => void }) {
  const raw = toolOutputText(tool)
    || (tool.status !== "completed" && tool.error ? String(tool.error) : "");
  const output = raw.length > MAX_OUTPUT_CHARS ? `${raw.slice(0, MAX_OUTPUT_CHARS)}\n…` : raw;
  const active = isActive(tool);
  const duration = formatToolDuration(tool.durationMs);
  const state = active ? "正在运行" : tool.status === "failed" ? "执行失败"
    : tool.status === "declined" ? "未执行" : tool.status === "cancelled" ? "已停止" : "已完成";
  const stateLabel = [state, duration].filter(Boolean).join(" · ");
  return (
    <>
      <RowBody
        follow={active}
        footer={(
          <>
            <span className={cn("px-1", tool.status === "failed" && "text-cx-danger")}>
              <SwapText value={stateLabel}>{stateLabel}</SwapText>
            </span>
            <span className="flex-1" />
            {onSelect ? <Button size="xs" variant="ghost" onClick={onSelect} className="text-cx-fg-3">查看详情</Button> : null}
          </>
        )}
      >
        {output ? (
          <ToolResultOutput language="text" className="text-[12px] leading-[1.6]">{output}</ToolResultOutput>
        ) : (
          <span className="font-cx-mono text-[12px] text-cx-fg-4">
            {active ? <ShimmerText>等待输出…</ShimmerText> : tool.status === "declined" ? "未执行" : "没有输出"}
          </span>
        )}
      </RowBody>
      <LocalUrlChips threadId={threadId} text={tool.outputSummary || ""} source="tool" disabled={active} className="mb-1" />
    </>
  );
}

function EditDiff({ tool, onSelect, onOpenDiff }: { tool: PresentableTool; onSelect?: () => void; onOpenDiff?: (filePath?: string) => void }) {
  const lines = useMemo(() => toolEditLines(tool) ?? [], [tool]);
  const path = toolEditStat(tool)?.paths[0];
  return (
    <RowBody
      footer={(
        <>
          <span className="flex-1" />
          {onOpenDiff && !isActive(tool) ? <Button size="xs" variant="ghost" onClick={() => onOpenDiff(path)} className="text-cx-fg-3">在变更中查看</Button> : null}
          {onSelect ? <Button size="xs" variant="ghost" onClick={onSelect} className="text-cx-fg-3">查看详情</Button> : null}
        </>
      )}
    >
      <div className="font-cx-mono text-[12px] leading-5">
        {lines.map((line, index) => (
          <div
            key={line.id}
            className={cn(
              "-mx-3 whitespace-pre-wrap break-all px-3",
              index < 24 && "cx-diff-in",
              line.type === "added" && "bg-cx-success/[0.08] text-cx-success",
              line.type === "removed" && "bg-cx-danger/[0.08] text-cx-danger",
              line.type === "context" && "text-cx-fg-3",
            )}
            style={index < 24 ? { animationDelay: `${index * 14}ms` } : undefined}
          >
            <span className="inline-block w-4 select-none">{line.type === "added" ? "+" : line.type === "removed" ? "−" : " "}</span>
            {line.content}
          </div>
        ))}
      </div>
    </RowBody>
  );
}

function ToolActivityRowView({
  row,
  tool,
  threadId,
  onSelect,
  onOpenDiff,
  fresh = false,
  enterDelay,
  peek = false,
  tree = false,
}: {
  row: ToolActivityRow;
  tool: PresentableTool;
  threadId?: string;
  onSelect?: () => void;
  onOpenDiff?: (filePath?: string) => void;
  /** Arrived while the reader watched: rises into place. */
  fresh?: boolean;
  /** Revealed by opening its group: cascades in after this delay (ms). */
  enterDelay?: number;
  /** Live: show the newest output lines of a running command under the row. */
  peek?: boolean;
  /** Hangs off the group's guide line. */
  tree?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const active = isActive(tool);
  const sawActive = useEver(active);
  const expandable = Boolean(row.body) && !(row.body === "diff" && tool.status === "failed");
  const text = toolActivityText(row);
  const showPeek = useHeld(peek && active && !open && row.body === "output", PEEK_DELAY_MS);
  const enterStyle: CSSProperties | undefined = enterDelay != null ? { animationDelay: `${enterDelay}ms` } : undefined;
  return (
    <div
      className={cn("min-w-0", tree && "cx-tree-item", fresh ? (tree ? "cx-reveal" : "cx-enter") : enterDelay != null && "cx-enter-soft")}
      style={enterStyle}
      data-testid="cx-tool-row"
      data-status={tool.status}
      data-category={row.category}
      data-fresh={(tree && fresh) || undefined}
    >
      <div className="min-w-0">
      <button
        type="button"
        onClick={expandable ? () => setOpen((value) => !value) : onSelect}
        disabled={!expandable && !onSelect}
        aria-expanded={expandable ? open : undefined}
        aria-label={expandable ? undefined : `${text}，查看详情`}
        title={row.title}
        className="group/row flex h-7 min-w-0 max-w-full items-center gap-1.5 rounded-md text-left text-[13px] text-cx-fg-3 outline-none transition-colors hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus disabled:cursor-default"
      >
        <SwapText value={`${active ? "active" : tool.status}:${text}`} className="min-w-0">
          {active ? <LiveLabel text={text} /> : <RowParts row={row} tint={sawActive} />}
        </SwapText>
        {row.additions ? (
          <CountUp value={row.additions} from={sawActive || fresh ? 0 : undefined} prefix="+" className="shrink-0 font-cx-mono text-[12px] text-cx-success" />
        ) : null}
        {row.deletions ? (
          <CountUp value={row.deletions} from={sawActive || fresh ? 0 : undefined} prefix="−" className="shrink-0 font-cx-mono text-[12px] text-cx-danger" />
        ) : null}
        <StatusMark status={tool.status} pop={sawActive} />
        {expandable ? (
          <Chevron open={open} className={cn(!open && "opacity-0 group-hover/row:opacity-100 group-focus-visible/row:opacity-100 [@media(hover:none)]:opacity-100")} />
        ) : null}
      </button>
      {expandable ? (
        <Collapse open={open} enterDuration={0.26} exitDuration={0.18}>
          {row.body === "output" ? <CommandOutput tool={tool} threadId={threadId} onSelect={onSelect} /> : null}
          {row.body === "diff" ? <EditDiff tool={tool} onSelect={onSelect} onOpenDiff={onOpenDiff} /> : null}
        </Collapse>
      ) : null}
      {row.body === "output" ? (
        // Opens and closes its height so the rows around it slide instead of jumping.
        <Collapse open={showPeek} enterDuration={0.26} exitDuration={0.34}>
          <LiveOutputPeek tool={tool} />
        </Collapse>
      ) : null}
      {!expandable && tool.status === "failed" && tool.error ? (
        <p className={cn("mb-1 line-clamp-2 break-words font-cx-mono text-[12px] leading-5 text-cx-danger", sawActive && "cx-enter")}>{String(tool.error)}</p>
      ) : null}
      </div>
    </div>
  );
}

type Entry = { row: ToolActivityRow; tool: PresentableTool };

/** Newest rows of the live group, following the latest call as it lands. */
function LiveTail({ entries, render }: { entries: Entry[]; render: (entry: Entry) => ReactNode }) {
  const stick = useStickToBottom(true);
  const shown = entries.length > LIVE_TAIL_ROWS ? entries.slice(-LIVE_TAIL_ROWS) : entries;
  return (
    <ScrollArea ref={stick} className={cn(LIVE_TAIL_HEIGHT, "cx-live-tail cx-no-scrollbar")} data-testid="cx-tool-live-tail">
      <div className="cx-tree" data-live>{shown.map(render)}</div>
    </ScrollArea>
  );
}

export interface ToolActivityGroupProps {
  tools: PresentableTool[];
  /** This group is the turn's newest work: its header narrates what runs now. */
  live?: boolean;
  /** Live header when no call of the group is running (the reasoning title). */
  liveFallback?: string;
  threadId?: string;
  onSelectTool?: (tool: PresentableTool) => void;
  onOpenDiff?: (filePath?: string) => void;
}

/**
 * Consecutive calls of one turn folded under a single line, the way Codex
 * shows work: "已读取文件运行了命令", or the running call's own words while it
 * is the newest work. While live the newest rows stay in view and glide up as
 * calls land; once the turn moves on the group folds to its summary. A lone
 * call is shown as its plain row.
 */
export const ToolActivityGroup = memo(function ToolActivityGroup(props: ToolActivityGroupProps) {
  if (props.tools.length && props.tools.every((tool) => isWebResearchTool(tool))) {
    return <WebResearchGroup tools={props.tools} live={props.live} liveFallback={props.liveFallback} onSelectTool={props.onSelectTool} />;
  }
  return <WorkGroup {...props} />;
}, (prev, next) => (
  prev.live === next.live
  && prev.liveFallback === next.liveFallback
  && prev.threadId === next.threadId
  && prev.onSelectTool === next.onSelectTool
  && prev.onOpenDiff === next.onOpenDiff
  && sameTools(prev.tools, next.tools)
));

function WorkGroup({
  tools,
  live = false,
  liveFallback = "正在思考",
  threadId,
  onSelectTool,
  onOpenDiff,
}: ToolActivityGroupProps) {
  const [open, setOpen] = useState(false);
  const [liveHidden, setLiveHidden] = useState(false);
  const entries = useMemo(
    () => tools.flatMap((tool) => toolActivityRows(tool).map((row) => ({ row, tool }))),
    [tools],
  );
  let activeEntry: Entry | undefined;
  if (live) {
    for (let index = entries.length - 1; index >= 0 && !activeEntry; index -= 1) {
      if (isActive(entries[index].tool)) activeEntry = entries[index];
    }
  }
  const liveLabel = useDwellLabel(
    live ? (activeEntry ? toolActivityText(activeEntry.row) : liveFallback) : "",
    1000,
  );
  const anyActive = entries.some((entry) => isActive(entry.tool));
  const failed = entries.some((entry) => entry.tool.status === "failed");
  const isFresh = useFreshKeys(entries.map((entry) => entry.row.key), live || anyActive);
  const single = entries.length === 1;
  // A group that was a single live row grows its header in when a second call lands.
  const grewLive = useEver(live && single);

  const wasLive = useRef(live);
  useEffect(() => {
    if (wasLive.current && !live) {
      setLiveHidden(false);
      setOpen(false);
    }
    wasLive.current = live;
  }, [live]);

  const renderRow = (entry: Entry, options: { enterDelay?: number; peek?: boolean; tree?: boolean } = {}) => (
    <ToolActivityRowView
      key={entry.row.key}
      row={entry.row}
      tool={entry.tool}
      threadId={threadId}
      onSelect={onSelectTool ? () => onSelectTool(entry.tool) : undefined}
      onOpenDiff={onOpenDiff}
      fresh={isFresh(entry.row.key)}
      enterDelay={options.enterDelay}
      peek={options.peek}
      tree={options.tree}
    />
  );

  if (!entries.length) return null;
  if (single) {
    const entry = entries[0];
    const thinking = live && !activeEntry;
    return (
      <div className="min-w-0" data-testid="cx-tool-group" data-single data-live={live || undefined}>
        {renderRow(entry, { peek: live })}
        {thinking ? (
          <div className="cx-enter flex h-7 items-center text-[13px] text-cx-fg-3" data-testid="cx-tool-group-live">
            <SwapText value={liveLabel}><LiveLabel text={liveLabel} /></SwapText>
          </div>
        ) : null}
      </div>
    );
  }

  const summary = summarizeToolActivity(entries.map((entry) => entry.row));
  const showTail = live && !liveHidden;
  const expanded = live ? showTail : open;
  const label = live ? liveLabel : summary;
  const toggle = () => {
    if (live) {
      setLiveHidden((value) => !value);
      return;
    }
    setOpen(!open);
  };
  return (
    <div className="min-w-0" data-testid="cx-tool-group" data-live={live || undefined}>
      <button
        type="button"
        onClick={toggle}
        aria-expanded={expanded}
        title={label}
        className={cn(
          "group/group flex h-7 min-w-0 max-w-full items-center gap-1.5 rounded-md text-left text-[13px] text-cx-fg-3 outline-none transition-colors hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus",
          grewLive && "cx-enter",
        )}
      >
        <SwapText value={`${live ? "live" : "done"}:${label}`} className="min-w-0">
          {live ? <LiveLabel text={label} /> : label}
        </SwapText>
        {!live && failed ? <span className="cx-pop inline-flex shrink-0"><Icon name="x" size={12} className="text-cx-danger" title="包含失败的调用" /></span> : null}
        <Chevron open={expanded} />
      </button>
      <Collapse open={expanded} initial={grewLive} enterDuration={0.28} exitDuration={0.22}>
        {live ? (
          <LiveTail entries={entries} render={(entry) => renderRow(entry, { peek: entry === activeEntry, tree: true })} />
        ) : (
          <ScrollArea className={GROUP_MAX_HEIGHT}>
            <div className="cx-tree">
              {entries.map((entry, index) => renderRow(entry, {
                enterDelay: index < STAGGER_ROWS ? 40 + index * 22 : undefined,
                tree: true,
              }))}
            </div>
          </ScrollArea>
        )}
      </Collapse>
    </div>
  );
}
