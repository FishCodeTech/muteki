"use client";

import { memo, type CSSProperties, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import {
  FONT_STYLE_BOLD,
  FONT_STYLE_ITALIC,
  FONT_STYLE_UNDERLINE,
  type CodeLines,
  type CodeToken,
} from "@/lib/chatHighlighter";
import { chatPanel } from "@/lib/chatPanelStore";
import type { DiffLineAnnotation, DiffStaging } from "@/lib/conversationDiff";
import { useThreadEditor } from "@/components/conversation/threadEditorContext";
import { Badge, Button, CopyButton, DiffStat, IconButton, Skeleton } from "@/components/chat/ui";
import {
  annotationEnd,
  intralineFor,
  splitPath,
  type CharRange,
  type DiffHunk,
  type DiffLine,
  type DiffListFile,
  type DiffSide,
  type NoticeKind,
} from "./model";

export const ROW_HEIGHT = {
  file: 36,
  hunk: 24,
  line: 20,
  fold: 28,
  comment: 76,
  notice: 44,
  end: 10,
} as const;

export interface FileTokens {
  old: CodeLines | null;
  new: CodeLines | null;
}

export interface RowContext {
  commentable: boolean;
  threadId?: string;
  onToggleFile: (key: string) => void;
  onTitleClick?: (key: string) => void;
  onExpandFold: (id: string) => void;
  onOpenLarge: (key: string) => void;
  onGutterPointerDown: (fi: number, side: DiffSide, line: number, event: React.PointerEvent) => void;
  onGutterActivate: (fi: number, side: DiffSide, line: number) => void;
  onRowEnter: (fi: number, side: DiffSide, line: number) => void;
  onDeleteAnnotation?: (id: string) => void;
}

function tokenStyle(token: CodeToken): CSSProperties {
  return {
    "--cx-tk": token.light,
    "--cx-tk-dark": token.dark ?? token.light,
    fontStyle: token.fontStyle && token.fontStyle & FONT_STYLE_ITALIC ? "italic" : undefined,
    fontWeight: token.fontStyle && token.fontStyle & FONT_STYLE_BOLD ? 600 : undefined,
    textDecoration: token.fontStyle && token.fontStyle & FONT_STYLE_UNDERLINE ? "underline" : undefined,
  } as CSSProperties;
}

/** Syntax tokens split at word-diff boundaries so both layers render at once. */
const CodeText = memo(function CodeText({
  text,
  tokens,
  ranges,
  tone,
}: {
  text: string;
  tokens?: CodeToken[] | null;
  ranges?: CharRange[];
  tone: "add" | "del";
}) {
  if (!text) return <>{"\u200b"}</>;
  const usable = tokens && tokens.reduce((sum, token) => sum + token.content.length, 0) === text.length ? tokens : null;
  if (!ranges?.length) {
    if (!usable) return <>{text}</>;
    return (
      <>
        {usable.map((token, index) => (
          <span key={index} className="cx-tk" style={tokenStyle(token)}>{token.content}</span>
        ))}
      </>
    );
  }
  const source: CodeToken[] = usable ?? [{ content: text }];
  const out: ReactNode[] = [];
  let pos = 0;
  let cursor = 0;
  for (const token of source) {
    const length = token.content.length;
    let start = 0;
    while (start < length) {
      const abs = pos + start;
      while (cursor < ranges.length && ranges[cursor][1] <= abs) cursor += 1;
      const range = ranges[cursor];
      const inside = Boolean(range && range[0] <= abs);
      const boundary = range ? (inside ? range[1] : range[0]) : Number.POSITIVE_INFINITY;
      const end = Math.min(length, boundary - pos);
      out.push(
        <span
          key={out.length}
          className={cn(token.light && "cx-tk", inside && (tone === "add" ? "cx-diff-word-add" : "cx-diff-word-del"))}
          style={token.light ? tokenStyle(token) : undefined}
        >
          {token.content.slice(start, end)}
        </span>,
      );
      start = end;
    }
    pos += length;
  }
  return <>{out}</>;
});

const STATUS_META: Record<string, { letter: string; label: string; className: string }> = {
  A: { letter: "A", label: "新增", className: "cx-diff-status-add" },
  M: { letter: "M", label: "修改", className: "cx-diff-status-mod" },
  D: { letter: "D", label: "删除", className: "cx-diff-status-del" },
  R: { letter: "R", label: "重命名", className: "cx-diff-status-ren" },
  C: { letter: "C", label: "复制", className: "cx-diff-status-ren" },
};

export function StatusLetter({ status, className }: { status: string; className?: string }) {
  const code = (status || "M").trim().charAt(0).toUpperCase();
  const meta = STATUS_META[code] ?? { letter: code || "?", label: status || "变更", className: "cx-diff-status-mod" };
  return (
    <span className={cn("cx-diff-status", meta.className, className)} title={meta.label} aria-label={meta.label}>
      {meta.letter}
    </span>
  );
}

export function stagingLabel(staging?: DiffStaging | string): string {
  switch (staging) {
    case "staged":
      return "已暂存";
    case "unstaged":
      return "未暂存";
    case "untracked":
      return "未跟踪";
    case "both":
      return "双方";
    case "artifact":
    case undefined:
    case "":
      return "";
    default:
      return staging;
  }
}

export const FileHeader = memo(function FileHeader({
  file,
  collapsed,
  active,
  flash,
  ctx,
  sticky,
}: {
  file: DiffListFile;
  collapsed: boolean;
  active: boolean;
  flash?: boolean;
  ctx: RowContext;
  sticky?: boolean;
}) {
  const { meta, parsed } = file;
  const { dir, name } = splitPath(meta.path);
  const staging = stagingLabel(meta.staging);
  const deleted = meta.status.startsWith("D");
  const additions = meta.additions ?? parsed.additions;
  const deletions = meta.deletions ?? parsed.deletions;
  const threadId = ctx.threadId;
  const editor = useThreadEditor();
  const firstLine = parsed.hunks.find((hunk) => hunk.newCount > 0)?.newStart;
  return (
    <div
      className={cn("cx-diff-file-header group/file", sticky && "is-sticky", flash && "is-flash")}
      data-testid={sticky ? undefined : "diff-file"}
      data-path={meta.path}
      data-staging={meta.staging || ""}
      data-active={active ? "true" : "false"}
    >
      <div className="cx-diff-sticky-x flex h-full items-center gap-1.5 pl-1.5 pr-2">
        <button
          type="button"
          className="cx-diff-file-toggle"
          aria-expanded={!collapsed}
          aria-label={collapsed ? `展开 ${meta.path}` : `折叠 ${meta.path}`}
          onClick={() => ctx.onToggleFile(file.key)}
        >
          <Icon name="chevronRight" size={13} className={cn("transition-transform duration-150", !collapsed && "rotate-90")} />
          <StatusLetter status={meta.status} />
        </button>
        <button
          type="button"
          title={meta.old_path ? `${meta.old_path} → ${meta.path}` : meta.path}
          onClick={() => (ctx.onTitleClick ?? ctx.onToggleFile)(file.key)}
          className="flex min-w-0 flex-1 items-baseline gap-1.5 overflow-hidden text-left font-cx-mono text-[12px] outline-none"
        >
          {meta.old_path ? (
            <>
              <span className="min-w-0 shrink truncate text-cx-fg-4 line-through decoration-cx-fg-4/40">{meta.old_path}</span>
              <Icon name="arrowRight" size={11} className="shrink-0 self-center text-cx-fg-4" />
            </>
          ) : null}
          <span className="flex min-w-0 items-baseline">
            {dir ? <span className="min-w-0 truncate text-cx-fg-3">{dir}</span> : null}
            <span className="min-w-0 truncate font-medium text-cx-fg">{name}</span>
          </span>
        </button>
        {staging ? <Badge className="h-[18px] px-1.5 text-[12px]" tone={meta.staging === "untracked" ? "success" : meta.staging === "staged" ? "accent" : "neutral"}>{staging}</Badge> : null}
        {parsed.binary || meta.binary ? <Badge className="h-[18px] px-1.5 text-[12px]">二进制</Badge> : null}
        <DiffStat additions={additions} deletions={deletions} className="shrink-0 pl-1" />
        <div className="cx-diff-file-actions flex shrink-0 items-center opacity-0 transition-opacity duration-150 focus-within:opacity-100 group-hover/file:opacity-100">
          <CopyButton text={meta.path} label="复制路径" />
          {threadId && !deleted ? (
            <>
              <IconButton size="xs" icon="externalLink" label="打开文件" onClick={() => chatPanel.openFile(threadId, meta.path)} />
              <IconButton size="xs" icon="folderTree" label="在文件中定位" onClick={() => chatPanel.revealInFiles(threadId, { kind: "file", path: meta.path })} />
              {editor ? (
                <IconButton
                  size="xs"
                  icon="code"
                  label={firstLine ? `在编辑器中打开（第 ${firstLine} 行）` : "在编辑器中打开"}
                  loading={editor.busy}
                  onClick={() => void editor.open(meta.path, firstLine || undefined)}
                />
              ) : null}
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
});

function hunkRange(header: string): string {
  const close = header.indexOf("@@", 2);
  return close > 0 ? header.slice(0, close + 2) : header;
}

export const HunkRow = memo(function HunkRow({ hunk, hidden }: { hunk: DiffHunk; hidden: number }) {
  return (
    <div className="cx-diff-hunk">
      <div className="cx-diff-sticky-x flex h-full items-center gap-2 px-3">
        <span className="shrink-0 text-cx-fg-4">{hunkRange(hunk.header)}</span>
        {hunk.context ? <span className="min-w-0 truncate text-cx-fg-3">{hunk.context}</span> : null}
        {hidden > 0 ? (
          <span className="ml-auto flex shrink-0 items-center gap-1 font-cx-sans text-[12px] text-cx-fg-4">
            <Icon name="foldVertical" size={11} />
            {hidden} 行未变更
          </span>
        ) : null}
      </div>
    </div>
  );
});

export const FoldRow = memo(function FoldRow({ id, count, ctx }: { id: string; count: number; ctx: RowContext }) {
  return (
    <button type="button" className="cx-diff-fold" onClick={() => ctx.onExpandFold(id)}>
      <span className="cx-diff-sticky-x flex h-full items-center gap-1.5 px-3">
        <Icon name="unfoldVertical" size={13} />
        展开 {count} 行未变更
      </span>
    </button>
  );
});

export const NoticeRow = memo(function NoticeRow({ notice, file, ctx }: { notice: NoticeKind; file: DiffListFile; ctx: RowContext }) {
  switch (notice) {
    case "binary":
      return (
        <div className="cx-diff-notice">
          <div className="cx-diff-sticky-x flex items-center gap-2 px-4 py-3">
            <Icon name="file" size={14} className="text-cx-fg-4" />
            二进制文件，无法显示文本差异
          </div>
        </div>
      );
    case "meta":
      return (
        <div className="cx-diff-notice">
          <div className="cx-diff-sticky-x flex items-center gap-2 px-4 py-3">
            <Icon name="info" size={14} className="text-cx-fg-4" />
            没有内容变更（仅重命名或权限变更）
          </div>
        </div>
      );
    case "truncated":
      return (
        <div className="cx-diff-notice is-warning">
          <div className="cx-diff-sticky-x flex items-center gap-2 px-4 py-2.5">
            <Icon name="alert" size={14} className="text-cx-warning" />
            文件超过预览上限，差异内容已截断
          </div>
        </div>
      );
    case "large":
      return (
        <div className="cx-diff-notice">
          <div className="cx-diff-sticky-x flex items-center gap-3 px-4 py-2">
            <Icon name="fileDiff" size={14} className="text-cx-fg-4" />
            <span className="min-w-0 flex-1">此文件变更较大（{file.parsed.changed.toLocaleString()} 行），已折叠以保持流畅</span>
            <Button size="xs" variant="secondary" onClick={() => ctx.onOpenLarge(file.key)}>加载差异</Button>
          </div>
        </div>
      );
    case "raw":
      return (
        <div className="cx-diff-notice is-raw">
          <div className="cx-diff-sticky-x px-4 py-2.5">
            <p className="mb-2 flex items-center gap-1.5 font-cx-sans text-[12px] text-cx-fg-3">
              <Icon name="info" size={13} />
              无法解析此补丁，显示原始内容
            </p>
            <pre className="cx-scroll m-0 max-h-[480px] overflow-auto whitespace-pre font-cx-mono text-[12px] leading-5 text-cx-fg-2">
              {file.parsed.raw.split("\n").slice(0, 800).join("\n")}
            </pre>
          </div>
        </div>
      );
    default: {
      const exhaustive: never = notice;
      return exhaustive;
    }
  }
});

function GutterButton({ fi, side, line, ctx }: { fi: number; side: DiffSide; line: number; ctx: RowContext }) {
  return (
    <button
      type="button"
      tabIndex={-1}
      className="cx-diff-comment-btn"
      aria-label={`在第 ${line} 行添加评审意见`}
      onPointerDown={(event) => ctx.onGutterPointerDown(fi, side, line, event)}
      onClick={(event) => { if (event.detail === 0) ctx.onGutterActivate(fi, side, line); }}
    >
      <Icon name="plus" size={12} />
    </button>
  );
}

function lineTokens(tokens: FileTokens | undefined, line: DiffLine, side: DiffSide): CodeToken[] | null | undefined {
  if (!tokens) return undefined;
  if (side === "old") return line.oldIndex === undefined ? undefined : tokens.old?.[line.oldIndex];
  return line.newIndex === undefined ? undefined : tokens.new?.[line.newIndex];
}

function EofMark({ line }: { line: DiffLine }) {
  if (!line.noNewline) return null;
  return <span className="cx-diff-eof" title="文件末尾没有换行符" aria-label="文件末尾没有换行符"><Icon name="circleDashed" size={10} /></span>;
}

export const LineRow = memo(function LineRow({
  fi,
  line,
  tokens,
  annotated,
  selected,
  ctx,
}: {
  fi: number;
  line: DiffLine;
  tokens?: FileTokens;
  annotated: boolean;
  selected: boolean;
  ctx: RowContext;
}) {
  const side: DiffSide = line.kind === "del" ? "old" : "new";
  const number = side === "old" ? line.oldLine : line.newLine;
  return (
    <div
      className={cn("cx-diff-line", `is-${line.kind}`, annotated && "is-annotated", selected && "is-selected")}
      data-old-line={line.kind !== "add" ? line.oldLine : undefined}
      data-new-line={line.kind !== "del" ? line.newLine : undefined}
      onPointerEnter={number === undefined ? undefined : () => ctx.onRowEnter(fi, side, number)}
    >
      <span className="cx-diff-num">{line.kind !== "add" ? line.oldLine : ""}</span>
      <span className="cx-diff-num">{line.kind !== "del" ? line.newLine : ""}</span>
      <span className="cx-diff-marker" aria-hidden>{line.kind === "add" ? "+" : line.kind === "del" ? "−" : ""}</span>
      <span className="cx-diff-code">
        <CodeText
          text={line.text}
          tokens={lineTokens(tokens, line, side)}
          ranges={line.kind === "ctx" ? undefined : intralineFor(line)}
          tone={line.kind === "del" ? "del" : "add"}
        />
        <EofMark line={line} />
      </span>
      {ctx.commentable && number !== undefined ? <GutterButton fi={fi} side={side} line={number} ctx={ctx} /> : null}
    </div>
  );
});

function Half({
  fi,
  side,
  line,
  tokens,
  annotated,
  selected,
  ctx,
}: {
  fi: number;
  side: DiffSide;
  line: DiffLine | null;
  tokens?: FileTokens;
  annotated: boolean;
  selected: boolean;
  ctx: RowContext;
}) {
  if (!line) return <div className="cx-diff-half is-empty" aria-hidden />;
  const number = side === "old" ? line.oldLine : line.newLine;
  const kind = line.kind;
  return (
    <div
      className={cn("cx-diff-half", `is-${kind}`, annotated && "is-annotated", selected && "is-selected")}
      data-old-line={side === "old" ? line.oldLine : undefined}
      data-new-line={side === "new" ? line.newLine : undefined}
      onPointerEnter={number === undefined ? undefined : () => ctx.onRowEnter(fi, side, number)}
    >
      <span className="cx-diff-num">{number ?? ""}</span>
      <span className="cx-diff-marker" aria-hidden>{kind === "add" ? "+" : kind === "del" ? "−" : ""}</span>
      <span className="cx-diff-code">
        <CodeText
          text={line.text}
          tokens={lineTokens(tokens, line, side)}
          ranges={kind === "ctx" ? undefined : intralineFor(line)}
          tone={kind === "del" ? "del" : "add"}
        />
        <EofMark line={line} />
      </span>
      {ctx.commentable && number !== undefined ? <GutterButton fi={fi} side={side} line={number} ctx={ctx} /> : null}
    </div>
  );
}

export const PairRow = memo(function PairRow({
  fi,
  left,
  right,
  tokens,
  leftAnnotated,
  rightAnnotated,
  leftSelected,
  rightSelected,
  ctx,
}: {
  fi: number;
  left: DiffLine | null;
  right: DiffLine | null;
  tokens?: FileTokens;
  leftAnnotated: boolean;
  rightAnnotated: boolean;
  leftSelected: boolean;
  rightSelected: boolean;
  ctx: RowContext;
}) {
  return (
    <div className="cx-diff-pair">
      <Half fi={fi} side="old" line={left} tokens={tokens} annotated={leftAnnotated} selected={leftSelected} ctx={ctx} />
      <Half fi={fi} side="new" line={right} tokens={tokens} annotated={rightAnnotated} selected={rightSelected} ctx={ctx} />
    </div>
  );
});

export function annotationRangeLabel(annotation: Pick<DiffLineAnnotation, "lineNumber" | "endLineNumber">): string {
  const end = annotationEnd(annotation);
  return end > annotation.lineNumber ? `第 ${annotation.lineNumber}–${end} 行` : `第 ${annotation.lineNumber} 行`;
}

export function sideLabel(side: DiffLineAnnotation["side"]): string {
  return side === "old" ? "旧版本" : side === "new" ? "新版本" : "统一视图";
}

export const CommentRow = memo(function CommentRow({ annotations, ctx }: { annotations: DiffLineAnnotation[]; ctx: RowContext }) {
  return (
    <div className="cx-diff-comment-row">
      <div className="cx-diff-sticky-x flex flex-col gap-1.5 px-3 py-1.5">
        {annotations.map((annotation) => (
          <div key={annotation.id} className={cn("cx-diff-comment-card", annotation.stale && "is-stale")}>
            <span className="mt-[1px] grid size-5 shrink-0 place-items-center rounded-full bg-cx-accent-soft text-cx-accent">
              <Icon name="messageCircle" size={11} />
            </span>
            <div className="min-w-0 flex-1">
              <p className="flex items-center gap-1.5 text-[12px] text-cx-fg-4">
                <span>{sideLabel(annotation.side)} · {annotationRangeLabel(annotation)}</span>
                {annotation.stale ? <Badge tone="warning" className="h-4 px-1 text-[12px]">已过期</Badge> : null}
              </p>
              <p className="whitespace-pre-wrap break-words text-[13px] leading-[1.55] text-cx-fg">{annotation.comment}</p>
            </div>
            {ctx.onDeleteAnnotation ? (
              <IconButton size="xs" icon="trash" label="删除评审意见" className="opacity-60 hover:opacity-100" onClick={() => ctx.onDeleteAnnotation?.(annotation.id)} />
            ) : null}
          </div>
        ))}
      </div>
    </div>
  );
});

export function DiffSkeleton({ rows = 14 }: { rows?: number }) {
  const widths = ["w-[62%]", "w-[48%]", "w-[74%]", "w-[36%]", "w-[58%]", "w-[80%]", "w-[42%]"];
  return (
    <div className="flex flex-col" aria-busy aria-label="正在读取变更">
      {[0, 1].map((block) => (
        <div key={block} className="border-b border-cx-border-subtle">
          <div className="flex h-9 items-center gap-2 border-b border-cx-border-subtle px-3">
            <Skeleton className="size-4 rounded-[4px]" />
            <Skeleton className="h-3 w-[38%]" />
            <span className="flex-1" />
            <Skeleton className="h-3 w-12" />
          </div>
          <div className="flex flex-col gap-[9px] px-3 py-3">
            {Array.from({ length: block === 0 ? rows : Math.ceil(rows / 2) }, (_, index) => (
              <div key={index} className="flex items-center gap-3">
                <Skeleton className="h-2.5 w-6 opacity-60" />
                <Skeleton className={cn("h-2.5", widths[(index + block * 3) % widths.length])} />
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
