"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/cn";
import { Button, IconButton, TokenLine } from "@/components/chat/ui";
import { languageFromPath, normalizeLanguage, useHighlightedCode } from "@/lib/chatHighlighter";
import { prefersReducedMotion } from "@/lib/usePrefersReducedMotion";

const HIGHLIGHT_LINE_LIMIT = 6000;
const LAZY_ROW_THRESHOLD = 1500;

export interface LineRange {
  start: number;
  end: number;
}

export function formatLineCitation(path: string, range: LineRange, excerpt: string): string {
  const span = range.end !== range.start ? `–${range.end}` : "";
  return `\`${path}\` L${range.start}${span}\n\`\`\`\n${excerpt.slice(0, 2000)}\n\`\`\``;
}

/**
 * Line-numbered, highlighted file viewer. Click a line (Shift-click to extend)
 * to select a range that can be cited into the composer.
 */
export function CodeViewer({
  code,
  path,
  language,
  focusLine,
  wrap = false,
  onCiteLines,
  className,
  maxHeight,
}: {
  code: string;
  path: string;
  language?: string | null;
  focusLine?: number | null;
  wrap?: boolean;
  onCiteLines?: (range: LineRange, excerpt: string) => void;
  className?: string;
  maxHeight?: number;
}) {
  const lang = normalizeLanguage(language) ?? languageFromPath(path);
  const text = useMemo(() => code.replace(/\n$/, ""), [code]);
  const lines = useMemo(() => text.split("\n"), [text]);
  const tokens = useHighlightedCode(text, lang, lines.length <= HIGHLIGHT_LINE_LIMIT);
  const [selection, setSelection] = useState<LineRange | null>(null);
  const anchor = useRef<number | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const gutter = String(lines.length).length;
  const lazy = lines.length > LAZY_ROW_THRESHOLD && wrap;

  useEffect(() => {
    setSelection(null);
    anchor.current = null;
  }, [path]);

  useEffect(() => {
    if (!focusLine || focusLine < 1) return;
    const frame = requestAnimationFrame(() => {
      const row = scrollRef.current?.querySelector<HTMLElement>(`[data-preview-line="${focusLine}"]`);
      row?.scrollIntoView({ block: "center", behavior: prefersReducedMotion() ? "instant" : "smooth" });
    });
    return () => cancelAnimationFrame(frame);
  }, [focusLine, text]);

  const selectLine = (line: number, extend: boolean) => {
    if (extend && anchor.current !== null) {
      setSelection({ start: Math.min(anchor.current, line), end: Math.max(anchor.current, line) });
      return;
    }
    if (selection && selection.start === line && selection.end === line) {
      setSelection(null);
      anchor.current = null;
      return;
    }
    anchor.current = line;
    setSelection({ start: line, end: line });
  };

  const cite = () => {
    if (!selection || !onCiteLines) return;
    onCiteLines(selection, lines.slice(selection.start - 1, selection.end).join("\n"));
  };

  const rangeLabel = selection ? `L${selection.start}${selection.end !== selection.start ? `–${selection.end}` : ""}` : "";

  return (
    <div className={cn("relative flex min-h-0 flex-col", className)}>
      <div
        ref={scrollRef}
        className="cx-scroll min-h-0 flex-1 overflow-auto"
        style={maxHeight ? { maxHeight } : undefined}
      >
        <pre
          aria-label="文件内容"
          className={cn(
            "m-0 py-2 font-cx-mono text-[13px] leading-5 text-cx-fg-2",
            wrap ? "whitespace-pre-wrap break-words" : "min-w-max whitespace-pre",
          )}
        >
          {lines.map((line, index) => {
            const number = index + 1;
            const selected = Boolean(selection && number >= selection.start && number <= selection.end);
            const focused = number === focusLine;
            return (
              <div
                key={index}
                data-preview-line={number}
                aria-label={`行 ${number}`}
                onClick={(event) => {
                  const picked = window.getSelection();
                  if (picked && !picked.isCollapsed && !(event.target as HTMLElement).closest("[data-gutter]")) return;
                  selectLine(number, event.shiftKey);
                }}
                className={cn(
                  "group/row flex cursor-default pr-4",
                  lazy && "cx-code-row",
                  selected
                    ? "bg-cx-selected"
                    : focused
                      ? "bg-cx-selected"
                      : "hover:bg-cx-hover",
                )}
              >
                <span
                  data-gutter=""
                  aria-hidden
                  className={cn(
                    "mr-3 inline-block shrink-0 select-none pl-4 pr-1 text-right tabular-nums",
                    selected || focused ? "text-cx-fg-2" : "text-cx-fg-4 group-hover/row:text-cx-fg-3",
                  )}
                  style={{ minWidth: `calc(${gutter}ch + 20px)` }}
                >
                  {number}
                </span>
                <span className={cn("min-w-0 flex-1", wrap && "whitespace-pre-wrap break-words")}>
                  <TokenLine tokens={tokens?.[index]} fallback={line} />
                </span>
              </div>
            );
          })}
        </pre>
      </div>
      {selection && onCiteLines ? (
        <div className="pointer-events-none absolute inset-x-0 bottom-3 flex justify-center px-3">
          <div className="cx-animate-in pointer-events-auto flex items-center gap-1 rounded-full bg-cx-overlay p-1 pl-3 shadow-cx-pop">
            <span className="cx-tabular mr-1 font-cx-mono text-[12px] text-cx-fg-3">{rangeLabel}</span>
            <Button size="xs" variant="primary" icon="quote" className="rounded-full" onClick={cite} aria-label={`引用 ${rangeLabel}`}>
              引用 {rangeLabel}
            </Button>
            <IconButton size="xs" icon="x" label="取消选择" className="rounded-full" onClick={() => { setSelection(null); anchor.current = null; }} />
          </div>
        </div>
      ) : null}
    </div>
  );
}
