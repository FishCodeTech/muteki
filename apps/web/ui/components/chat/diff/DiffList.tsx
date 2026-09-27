"use client";

import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type Ref,
} from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { cn } from "@/lib/cn";
import { highlightCode, languageFromPath } from "@/lib/chatHighlighter";
import type { DiffLineAnnotation } from "@/lib/conversationDiff";
import { DiffLineCommentPopover } from "@/components/conversation/DiffLineCommentPopover";
import {
  CommentRow,
  FileHeader,
  FoldRow,
  HunkRow,
  LineRow,
  NoticeRow,
  PairRow,
  ROW_HEIGHT,
  type FileTokens,
  type RowContext,
} from "./DiffRows";
import {
  annotationEnd,
  buildRows,
  LARGE_FILE_LINES,
  MAX_SIZER_COLS,
  sideLines,
  type DiffListFile,
  type DiffListRow,
  type DiffSide,
  type DiffViewMode,
  type ParsedPatch,
} from "./model";

export interface DiffListHandle {
  scrollToFile: (key: string, options?: { flash?: boolean }) => boolean;
}

export interface DiffListProps {
  files: DiffListFile[];
  mode: DiffViewMode;
  wrap: boolean;
  ignoreWhitespace?: boolean;
  /** Context lines kept around changes before folding long unchanged runs; >= 999 disables folding. */
  contextLines?: number;
  collapsed: ReadonlySet<string>;
  onToggleFile: (key: string) => void;
  /** Enables "open file" / "reveal in files" actions on file headers. */
  threadId?: string;
  annotations?: DiffLineAnnotation[];
  baselineId?: string;
  onAddAnnotation?: (annotation: Omit<DiffLineAnnotation, "id" | "createdAt" | "stale">) => void;
  onDeleteAnnotation?: (id: string) => void;
  onActiveFileChange?: (key: string) => void;
  onTitleClick?: (key: string) => void;
  handleRef?: Ref<DiffListHandle>;
  /** Bounded mode (drawer): the list sizes to content up to this height. */
  maxHeight?: number | string;
  className?: string;
}

interface LineSelection {
  fi: number;
  fileKey: string;
  side: DiffSide;
  anchor: number;
  head: number;
}

const EMPTY_ANNOTATIONS: DiffLineAnnotation[] = [];
const MAX_ANNOTATED_SPAN = 400;

function estimateRow(row: DiffListRow | undefined): number {
  if (!row) return ROW_HEIGHT.line;
  switch (row.type) {
    case "file":
      return ROW_HEIGHT.file;
    case "hunk":
      return ROW_HEIGHT.hunk;
    case "line":
    case "pair":
      return ROW_HEIGHT.line;
    case "fold":
      return ROW_HEIGHT.fold;
    case "comment":
      return 16 + row.annotations.length * 60;
    case "notice":
      return row.notice === "raw" ? 520 : ROW_HEIGHT.notice;
    case "end":
      return ROW_HEIGHT.end;
    default: {
      const exhaustive: never = row;
      return exhaustive;
    }
  }
}

function rowMatches(row: DiffListRow, side: DiffSide, line: number): boolean {
  if (row.type === "line") {
    if (side === "old") return row.line.kind !== "add" && row.line.oldLine === line;
    return row.line.kind !== "del" && row.line.newLine === line;
  }
  if (row.type === "pair") return side === "old" ? row.left?.oldLine === line : row.right?.newLine === line;
  return false;
}

function useLatest<T>(value: T) {
  const ref = useRef(value);
  useLayoutEffect(() => { ref.current = value; });
  return ref;
}

export function DiffList({
  files,
  mode,
  wrap,
  ignoreWhitespace = false,
  contextLines = 999,
  collapsed,
  onToggleFile,
  threadId,
  annotations = EMPTY_ANNOTATIONS,
  baselineId = "worktree",
  onAddAnnotation,
  onDeleteAnnotation,
  onActiveFileChange,
  onTitleClick,
  handleRef,
  maxHeight,
  className,
}: DiffListProps) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const anchorRef = useRef<HTMLDivElement | null>(null);
  const [viewportWidth, setViewportWidth] = useState(0);
  const [expandedFolds, setExpandedFolds] = useState<ReadonlySet<string>>(() => new Set());
  const [largeOpened, setLargeOpened] = useState<ReadonlySet<string>>(() => new Set());
  const [selection, setSelectionState] = useState<LineSelection | null>(null);
  const [composer, setComposer] = useState<LineSelection | null>(null);
  const [flashKey, setFlashKey] = useState<string | null>(null);
  const [tokens, setTokens] = useState<ReadonlyMap<ParsedPatch, FileTokens>>(() => new Map());
  const selectionRef = useRef<LineSelection | null>(null);
  const dragging = useRef(false);
  const pendingTokens = useRef(new Set<ParsedPatch>());
  const flashTimer = useRef<number | undefined>(undefined);
  const mounted = useRef(true);
  const latest = useLatest({ files, onToggleFile, onTitleClick, onDeleteAnnotation, onActiveFileChange });

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      window.clearTimeout(flashTimer.current);
    };
  }, []);

  const setSelection = useCallback((next: LineSelection | null) => {
    selectionRef.current = next;
    setSelectionState(next);
  }, []);

  const { rows, fileStart } = useMemo(
    () => buildRows(files, { mode, contextLines, ignoreWhitespace, collapsed, expandedFolds, largeOpened, annotations }),
    [files, mode, contextLines, ignoreWhitespace, collapsed, expandedFolds, largeOpened, annotations],
  );

  const commentable = Boolean(onAddAnnotation);
  const canDelete = Boolean(onDeleteAnnotation);
  const measureAll = wrap;

  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: (index) => estimateRow(rows[index]),
    getItemKey: useCallback((index: number) => rows[index]?.key ?? index, [rows]),
    overscan: 14,
    useFlushSync: false,
  });

  useEffect(() => {
    virtualizer.measure();
  }, [virtualizer, wrap, mode]);

  useEffect(() => {
    const element = scrollRef.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const update = () => setViewportWidth(element.clientWidth);
    update();
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const onPointerUp = () => {
      if (!dragging.current) return;
      dragging.current = false;
      const current = selectionRef.current;
      setSelection(null);
      if (current) setComposer(current);
    };
    window.addEventListener("pointerup", onPointerUp);
    window.addEventListener("pointercancel", onPointerUp);
    return () => {
      window.removeEventListener("pointerup", onPointerUp);
      window.removeEventListener("pointercancel", onPointerUp);
    };
  }, [setSelection]);

  useEffect(() => {
    setComposer(null);
    setSelection(null);
  }, [baselineId, setSelection]);

  const ctx = useMemo<RowContext>(() => ({
    commentable,
    threadId,
    onToggleFile: (key) => latest.current.onToggleFile(key),
    onTitleClick: (key) => (latest.current.onTitleClick ?? latest.current.onToggleFile)(key),
    onExpandFold: (id) => setExpandedFolds((prev) => new Set(prev).add(id)),
    onOpenLarge: (key) => setLargeOpened((prev) => new Set(prev).add(key)),
    onGutterPointerDown: (fi, side, line, event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      const fileKey = latest.current.files[fi]?.key;
      if (!fileKey) return;
      dragging.current = true;
      setComposer(null);
      setSelection({ fi, fileKey, side, anchor: line, head: line });
    },
    onGutterActivate: (fi, side, line) => {
      const fileKey = latest.current.files[fi]?.key;
      if (!fileKey) return;
      setSelection(null);
      setComposer({ fi, fileKey, side, anchor: line, head: line });
    },
    onRowEnter: (fi, side, line) => {
      const current = selectionRef.current;
      if (!dragging.current || !current || current.fi !== fi || current.side !== side || current.head === line) return;
      setSelection({ ...current, head: line });
    },
    onDeleteAnnotation: canDelete ? (id) => latest.current.onDeleteAnnotation?.(id) : undefined,
  }), [commentable, threadId, latest, setSelection, canDelete]);

  const annotatedByFile = useMemo(() => {
    const map = new Map<number, Set<string>>();
    if (!annotations.length) return map;
    files.forEach((file, fi) => {
      for (const annotation of annotations) {
        if (annotation.path !== file.meta.path) continue;
        if (annotation.staging && file.meta.staging && annotation.staging !== file.meta.staging) continue;
        const prefix = annotation.side === "old" ? "o" : "n";
        const end = Math.min(annotationEnd(annotation), annotation.lineNumber + MAX_ANNOTATED_SPAN);
        let set = map.get(fi);
        if (!set) {
          set = new Set();
          map.set(fi, set);
        }
        for (let line = annotation.lineNumber; line <= end; line += 1) set.add(`${prefix}:${line}`);
      }
    });
    return map;
  }, [annotations, files]);

  const composerLive = composer && files[composer.fi]?.key === composer.fileKey ? composer : null;
  const highlight = selection ?? composerLive;
  const inHighlight = (fi: number, side: DiffSide, line: number | undefined) => {
    if (!highlight || line === undefined || highlight.fi !== fi || highlight.side !== side) return false;
    const from = Math.min(highlight.anchor, highlight.head);
    const to = Math.max(highlight.anchor, highlight.head);
    return line >= from && line <= to;
  };
  const isAnnotated = (fi: number, side: DiffSide, line: number | undefined) =>
    line !== undefined && Boolean(annotatedByFile.get(fi)?.has(`${side === "old" ? "o" : "n"}:${line}`));

  const items = virtualizer.getVirtualItems();
  const offset = virtualizer.scrollOffset ?? 0;
  const topItem = rows.length ? virtualizer.getVirtualItemForOffset(offset) : undefined;
  const activeFi = topItem ? rows[topItem.index]?.fi ?? 0 : 0;
  const activeFile = files[activeFi];
  const measurements = virtualizer.measurementsCache;
  const headerStart = measurements[fileStart[activeFi] ?? 0]?.start ?? 0;
  const nextHeader = fileStart[activeFi + 1];
  const nextStart = nextHeader !== undefined ? measurements[nextHeader]?.start ?? Number.POSITIVE_INFINITY : Number.POSITIVE_INFINITY;
  const stickyShift = Math.min(0, nextStart - offset - ROW_HEIGHT.file);
  const showSticky = Boolean(activeFile) && offset > headerStart + 1;

  const activeKey = activeFile?.key;
  useEffect(() => {
    if (activeKey) latest.current.onActiveFileChange?.(activeKey);
  }, [activeKey, latest]);

  const visibleFiles = useMemo(() => {
    const set = new Set<number>();
    for (const item of items) {
      const row = rows[item.index];
      if (row) set.add(row.fi);
    }
    return Array.from(set).join(",");
  }, [items, rows]);

  useEffect(() => {
    const live = new Set(files.map((file) => file.parsed));
    setTokens((prev) => {
      let stale = false;
      for (const key of prev.keys()) if (!live.has(key)) { stale = true; break; }
      if (!stale) return prev;
      const next = new Map<ParsedPatch, FileTokens>();
      for (const [key, value] of prev) if (live.has(key)) next.set(key, value);
      return next;
    });
  }, [files]);

  useEffect(() => {
    if (!visibleFiles) return;
    for (const part of visibleFiles.split(",")) {
      const file = files[Number(part)];
      if (!file) continue;
      const parsed = file.parsed;
      if (tokens.has(parsed) || pendingTokens.current.has(parsed)) continue;
      if (parsed.binary || !parsed.hunks.length || parsed.changed > LARGE_FILE_LINES) continue;
      const language = languageFromPath(file.meta.path);
      if (!language) continue;
      pendingTokens.current.add(parsed);
      void Promise.all([
        parsed.oldText ? highlightCode(parsed.oldText, language) : Promise.resolve(null),
        parsed.newText ? highlightCode(parsed.newText, language) : Promise.resolve(null),
      ]).then(([oldLines, newLines]) => {
        pendingTokens.current.delete(parsed);
        if (!mounted.current) return;
        setTokens((prev) => new Map(prev).set(parsed, { old: oldLines, new: newLines }));
      });
    }
  }, [visibleFiles, files, tokens]);

  useImperativeHandle(handleRef, () => ({
    scrollToFile: (key, options) => {
      const fi = latest.current.files.findIndex((file) => file.key === key);
      if (fi < 0 || fileStart[fi] === undefined) return false;
      virtualizer.scrollToIndex(fileStart[fi], { align: "start" });
      if (options?.flash) {
        setFlashKey(key);
        window.clearTimeout(flashTimer.current);
        flashTimer.current = window.setTimeout(() => setFlashKey(null), 1600);
      }
      return true;
    },
  }), [virtualizer, fileStart, latest]);

  const composerRow = useMemo(() => {
    if (!composerLive) return -1;
    const from = fileStart[composerLive.fi] ?? 0;
    const to = fileStart[composerLive.fi + 1] ?? rows.length;
    const end = Math.max(composerLive.anchor, composerLive.head);
    for (let index = from; index < to; index += 1) {
      if (rowMatches(rows[index], composerLive.side, end)) return index;
    }
    return from;
  }, [composerLive, fileStart, rows]);

  const digits = useMemo(() => {
    let max = 0;
    for (const file of files) if (file.parsed.maxLine > max) max = file.parsed.maxLine;
    return Math.max(3, String(max).length);
  }, [files]);

  const cols = useMemo(() => {
    let max = 0;
    for (const file of files) {
      if (collapsed.has(file.key)) continue;
      if (file.parsed.changed > LARGE_FILE_LINES && !largeOpened.has(file.key)) continue;
      if (file.parsed.maxCols > max) max = file.parsed.maxCols;
    }
    return Math.min(max, MAX_SIZER_COLS);
  }, [files, collapsed, largeOpened]);

  const sizerWidth = wrap
    ? "100%"
    : mode === "split"
      ? `max(100%, calc(2 * (${cols}ch + var(--cx-diff-num) + 44px)))`
      : `max(100%, calc(${cols}ch + 2 * var(--cx-diff-num) + 44px))`;

  const renderRow = (row: DiffListRow) => {
    const file = files[row.fi];
    const fileTokens = file ? tokens.get(file.parsed) : undefined;
    switch (row.type) {
      case "file":
        return file ? (
          <FileHeader file={file} collapsed={collapsed.has(file.key)} active={row.fi === activeFi} flash={flashKey === file.key} ctx={ctx} />
        ) : null;
      case "hunk":
        return <HunkRow hunk={row.hunk} hidden={row.hidden} />;
      case "line": {
        const side: DiffSide = row.line.kind === "del" ? "old" : "new";
        const number = side === "old" ? row.line.oldLine : row.line.newLine;
        return (
          <LineRow
            fi={row.fi}
            line={row.line}
            tokens={fileTokens}
            annotated={isAnnotated(row.fi, side, number)}
            selected={inHighlight(row.fi, side, number)}
            ctx={ctx}
          />
        );
      }
      case "pair":
        return (
          <PairRow
            fi={row.fi}
            left={row.left}
            right={row.right}
            tokens={fileTokens}
            leftAnnotated={isAnnotated(row.fi, "old", row.left?.oldLine)}
            rightAnnotated={isAnnotated(row.fi, "new", row.right?.newLine)}
            leftSelected={inHighlight(row.fi, "old", row.left?.oldLine)}
            rightSelected={inHighlight(row.fi, "new", row.right?.newLine)}
            ctx={ctx}
          />
        );
      case "fold":
        return <FoldRow id={row.id} count={row.count} ctx={ctx} />;
      case "comment":
        return <CommentRow annotations={row.annotations} ctx={ctx} />;
      case "notice":
        return file ? <NoticeRow notice={row.notice} file={file} ctx={ctx} /> : null;
      case "end":
        return <div className="cx-diff-end" />;
      default: {
        const exhaustive: never = row;
        return exhaustive;
      }
    }
  };

  const composerFile = composerLive ? files[composerLive.fi] : undefined;
  const composerMeasure = composerRow >= 0 ? measurements[composerRow] : undefined;
  const composerFrom = composerLive ? Math.min(composerLive.anchor, composerLive.head) : 0;
  const composerTo = composerLive ? Math.max(composerLive.anchor, composerLive.head) : 0;
  const composerSnapshot = useMemo(() => {
    if (!composerLive || !composerFile) return "";
    return sideLines(composerFile.parsed, composerLive.side, composerFrom, composerTo).map((line) => line.text).join("\n");
  }, [composerLive, composerFile, composerFrom, composerTo]);

  return (
    <>
      <div
        ref={scrollRef}
        className={cn(
          "cx-diff-list cx-scroll relative min-h-0 overflow-auto",
          maxHeight === undefined && "flex-1",
          selection && "is-dragging",
          className,
        )}
        data-wrap={wrap ? "true" : "false"}
        data-mode={mode}
        style={{
          "--cx-diff-vw": `${viewportWidth}px`,
          "--cx-diff-num": `calc(${digits}ch + 18px)`,
          maxHeight,
        } as CSSProperties}
      >
        {showSticky && activeFile ? (
          <div className="cx-diff-sticky-layer" style={{ width: viewportWidth || "100%" }}>
            <div style={{ transform: stickyShift ? `translateY(${stickyShift}px)` : undefined }}>
              <FileHeader file={activeFile} collapsed={collapsed.has(activeFile.key)} active flash={false} ctx={ctx} sticky />
            </div>
          </div>
        ) : null}
        <div className="cx-diff-sizer" style={{ height: virtualizer.getTotalSize(), width: sizerWidth }}>
          {items.map((item) => {
            const row = rows[item.index];
            if (!row) return null;
            const measured = row.type === "comment" || row.type === "notice" || (measureAll && (row.type === "line" || row.type === "pair"));
            return (
              <div
                key={item.key}
                data-index={item.index}
                ref={measured ? virtualizer.measureElement : undefined}
                className="cx-diff-vrow"
                style={{ transform: `translateY(${item.start}px)`, height: measured ? undefined : item.size }}
              >
                {renderRow(row)}
              </div>
            );
          })}
          {composerLive && composerMeasure ? (
            <div
              ref={anchorRef}
              aria-hidden
              className="pointer-events-none absolute left-0"
              style={{ top: composerMeasure.start, height: composerMeasure.size, width: viewportWidth || "100%" }}
            />
          ) : null}
        </div>
      </div>
      {composerLive && composerFile && onAddAnnotation ? (
        <DiffLineCommentPopover
          key={`${composerLive.fileKey}:${composerLive.side}:${composerFrom}:${composerTo}`}
          anchorRef={anchorRef}
          path={composerFile.meta.path}
          oldPath={composerFile.meta.old_path ?? undefined}
          side={composerLive.side}
          lineNumber={composerFrom}
          endLineNumber={composerTo > composerFrom ? composerTo : undefined}
          snapshot={composerSnapshot}
          staging={composerFile.meta.staging}
          baselineId={baselineId}
          onAdd={onAddAnnotation}
          onDismiss={() => setComposer(null)}
        />
      ) : null}
    </>
  );
}
