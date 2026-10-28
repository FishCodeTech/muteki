"use client";

import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ComponentProps, type DragEvent, type MouseEvent, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Badge, Button, IconButton, Tooltip, toast } from "@/components/chat/ui";
import { SectionHeader } from "@/components/chat/surfaces/shared";
import {
  EVIDENCE_KIND_LABELS,
  categoryLabel,
  sameRef,
  sourceKindLabel,
  statusLabel,
  submissionStatusLabel,
  TASK_STATUSES,
  type StudioRef,
  type StudioRefKind,
} from "./model";
import { isSheetViewport, openRef, revealConversation, studioComposer } from "./nav";
import { findPassage, findSource, isRecent, isUpdated, readStudioState, refExists, refLabel, studio, useStudioState, type StudioState } from "./store";

export { studioComposer };

export const REF_ICON: Record<StudioRefKind, IconName> = {
  source: "book",
  passage: "quote",
  clue: "bookmark",
  task: "listTodo",
  report: "file",
  evidence: "fileCode",
  import: "upload",
  run: "activity",
  change: "gitCompare",
  feedback: "messageCircle",
  plan: "compass",
};

export const REF_KIND_LABEL: Record<StudioRefKind, string> = {
  source: "资产",
  passage: "段落",
  clue: "发现",
  task: "任务",
  report: "报告",
  evidence: "证据",
  import: "导入批次",
  run: "工作记录",
  change: "变化记录",
  feedback: "平台反馈",
  plan: "任务计划",
};

const VALID_REF_KINDS = new Set<StudioRefKind>([
  "source",
  "passage",
  "clue",
  "task",
  "report",
  "evidence",
  "import",
  "run",
  "change",
  "feedback",
  "plan",
]);

export const STUDIO_REF_MIME = "application/x-muteki-studio-ref";

export const BTN = "h-[36px] gap-2 px-3.5 text-[14px]";
export const BTN_SM = "h-[32px] gap-1.5 px-3 text-[13px]";
export const ICON_BTN = "h-[36px] w-[36px]";
export const ICON_BTN_SM = "h-[32px] w-[32px]";
export const FIELD = "h-[40px] text-[14px]";
export const SEARCH = "[&>input]:h-[40px] [&>input]:text-[14px]";
export const MENU_HIT = "[&_[role^=menuitem]]:min-h-[36px] [&_[role^=menuitem]]:text-[14px]";
export const MENU_ITEM_HIT = "min-h-[36px] text-[14px]";
export const OPTION_HIT = "[&_[role=option]]:min-h-[36px] [&_[role=option]]:text-[14px]";
export const SEGMENTED_HIT = "h-[38px] [&>button]:h-[34px] [&>button]:px-3 [&>button]:text-[13px]";
export const REVEAL = "opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100 [@media(hover:none)]:opacity-100";

export function parseDroppedRef(data: DataTransfer): StudioRef | null {
  try {
    const parsed = JSON.parse(data.getData(STUDIO_REF_MIME)) as Partial<StudioRef>;
    if (typeof parsed.id === "string" && parsed.kind && VALID_REF_KINDS.has(parsed.kind)) {
      return { kind: parsed.kind, id: parsed.id };
    }
  } catch {
    // Not a studio payload.
  }
  return null;
}

/** Makes an element draggable into the composer, where it becomes chat context. */
export function dragRef(target: StudioRef) {
  return {
    draggable: true,
    onDragStart: (event: DragEvent<HTMLElement>) => {
      event.dataTransfer.effectAllowed = "copy";
      event.dataTransfer.setData(STUDIO_REF_MIME, JSON.stringify(target));
      event.dataTransfer.setData("text/plain", refLabel(readStudioState(), target));
    },
  };
}

// ---------------------------------------------------------------------------
// Verbs. Everything in a workspace reaches the conversation through these.
// ---------------------------------------------------------------------------

export function citeRef(target: StudioRef, options: { silent?: boolean } = {}) {
  const current = readStudioState();
  studio.addContext(current.activeThreadId, target);
  if (options.silent || !isSheetViewport()) return;
  toast({
    title: "已带上对话",
    description: refLabel(current, target),
    tone: "success",
    duration: 2400,
    action: { label: "去提问", onClick: () => askAi(null) },
  });
}

export function askAi(target: StudioRef | null, prefill?: string) {
  if (target) citeRef(target, { silent: true });
  revealConversation();
  if (prefill) studioComposer.current?.insertText(prefill);
  else studioComposer.current?.focus();
}

export function quoteToComposer(text: string, owner: StudioRef, options: { silent?: boolean } = {}) {
  citeRef(owner, { silent: true });
  studioComposer.current?.insertText(`> ${text.replace(/\s*\n\s*/g, " ")}\n`, { focus: false });
  if (options.silent || !isSheetViewport()) return;
  toast({ title: "已引用到输入框", tone: "success", duration: 2400, action: { label: "去提问", onClick: () => askAi(null) } });
}

export function useAttached(target: StudioRef): boolean {
  const state = useStudioState();
  return (state.context[state.activeThreadId] ?? []).some((item) => sameRef(item, target));
}

export function AttachButton({ target, withLabel = false, small = false, reveal = false, className }: {
  target: StudioRef;
  withLabel?: boolean;
  small?: boolean;
  reveal?: boolean;
  className?: string;
}) {
  const attached = useAttached(target);
  const onClick = (event: MouseEvent) => {
    event.stopPropagation();
    if (attached) studio.removeContext(readStudioState().activeThreadId, target);
    else citeRef(target);
  };
  if (withLabel) {
    return (
      <Button
        size="md"
        variant={attached ? "soft" : "secondary"}
        icon={attached ? "check" : "at"}
        aria-pressed={attached}
        tooltip={attached ? "已在下一条消息的上下文里，点击取消" : "放进下一条消息的上下文"}
        className={cn(BTN, className)}
        onClick={onClick}
      >
        {attached ? "已带上" : "带上对话"}
      </Button>
    );
  }
  return (
    <IconButton
      size="md"
      icon={attached ? "check" : "at"}
      label={attached ? "已带上对话，点击取消" : "带上对话"}
      active={attached}
      className={cn(small ? ICON_BTN_SM : ICON_BTN, attached && "text-cx-accent hover:text-cx-accent", reveal && !attached && REVEAL, className)}
      onClick={onClick}
    />
  );
}

export function StudioBadge({ className, ...props }: ComponentProps<typeof Badge>) {
  return <Badge {...props} className={cn("h-6 px-2 text-[13px]", className)} />;
}

export function StudioSectionHeader({ className, ...props }: ComponentProps<typeof SectionHeader>) {
  return <SectionHeader {...props} className={cn("h-[36px] [&_h3]:text-[13px] [&_h3]:text-cx-fg-2 [&_h3>span]:text-cx-fg-3", className)} />;
}

function clip(text: string, length: number): string {
  return text.length > length ? `${text.slice(0, length)}…` : text;
}

/** Hover preview for all 11 StudioRefKind object types. */
export function refPreview(current: StudioState, ref: StudioRef): { title: string; excerpt: string; origin: string } {
  const title = refLabel(current, ref);
  if (ref.kind === "clue") {
    const clue = current.clues.find((item) => item.id === ref.id);
    const hit = clue ? findPassage(current, clue.passageId) : undefined;
    return {
      title,
      excerpt: clue ? `「${clip(clue.quote, 70)}」` : "",
      origin: clue ? `${hit ? `《${hit.source.title}》第 ${hit.passage.index} 段 · ` : ""}${categoryLabel(clue.category)} · ${statusLabel(clue.status)}` : "",
    };
  }
  if (ref.kind === "passage") {
    const hit = findPassage(current, ref.id);
    return { title, excerpt: hit ? clip(hit.passage.text, 80) : "", origin: hit ? `${sourceKindLabel(hit.source.kind)} · ${hit.source.author}` : "" };
  }
  if (ref.kind === "source") {
    const source = findSource(current, ref.id);
    return { title, excerpt: source?.passages[0] ? clip(source.passages[0].text, 80) : "", origin: source ? `${sourceKindLabel(source.kind)} · ${source.author} · ${source.passages.length} 段` : "" };
  }
  if (ref.kind === "report") {
    const rep = current.reports.find((r) => r.id === ref.id);
    return {
      title,
      excerpt: rep ? clip(rep.sections.mechanism, 72) : "",
      origin: rep ? `版本 v${rep.version} · ${rep.readiness} · ${submissionStatusLabel(rep.submission.status)}` : "",
    };
  }
  if (ref.kind === "evidence") {
    const ev = current.evidences.find((e) => e.id === ref.id);
    return {
      title,
      excerpt: ev ? `${EVIDENCE_KIND_LABELS[ev.kind]} · ${ev.completeness} · SHA256:${ev.sha256.slice(0, 12)}…` : "",
      origin: ev ? `${ev.actorRef} · ${ev.capturedAt}` : "",
    };
  }
  if (ref.kind === "import") {
    const imp = current.imports.find((b) => b.id === ref.id);
    return {
      title,
      excerpt: imp ? `共 ${imp.items.length} 条记录 · ${imp.format.toUpperCase()}` : "",
      origin: imp ? `${imp.importedAt}` : "",
    };
  }
  if (ref.kind === "run") {
    const run = current.workRuns.find((r) => r.id === ref.id);
    return {
      title,
      excerpt: run ? clip(run.question, 72) : "",
      origin: run ? `代次 G${run.generation} · ${RUN_STATE_LABEL[run.state]} · ${run.identityContext}` : "",
    };
  }
  if (ref.kind === "change") {
    const chg = current.changes.find((c) => c.id === ref.id);
    return {
      title,
      excerpt: chg ? clip(chg.denoisedDiff, 72) : "",
      origin: chg ? `基线 ${chg.baselineCycleId} · ${chg.observedAt}` : "",
    };
  }
  if (ref.kind === "feedback") {
    const fb = current.feedbacks.find((f) => f.id === ref.id);
    return {
      title,
      excerpt: fb ? clip(fb.rawText, 72) : "",
      origin: fb ? `${fb.platformAuthor} · ${fb.receivedAt}` : "",
    };
  }
  if (ref.kind === "plan") {
    const pl = current.planVersions.find((p) => p.id === ref.id);
    return {
      title,
      excerpt: pl ? clip(pl.focusQuestion, 72) : "",
      origin: pl ? `v${pl.version} · ${pl.createdAt} · ${pl.author}` : "",
    };
  }
  const task = current.tasks.find((item) => item.id === ref.id);
  return {
    title,
    excerpt: task ? `关联 ${task.links.length} 个对象` : "",
    origin: task ? `${task.assignee} · ${task.due} · ${TASK_STATUSES.find((item) => item.value === task.status)?.label ?? ""}` : "",
  };
}

function RefPreviewCard({ target }: { target: StudioRef }) {
  const state = useStudioState();
  const preview = refPreview(state, target);
  return (
    <span className="flex max-w-[300px] flex-col gap-1 py-0.5 text-left">
      <span className="text-[13px] font-medium">{REF_KIND_LABEL[target.kind]} · {preview.title}</span>
      {preview.excerpt ? <span className="text-[13px] leading-5 opacity-90">{preview.excerpt}</span> : null}
      {preview.origin ? <span className="text-[13px] opacity-75">{preview.origin}</span> : null}
    </span>
  );
}

export function RefLink({ target, className }: { target: StudioRef; className?: string }) {
  const state = useStudioState();
  const exists = refExists(state, target);
  return (
    <Tooltip content={exists ? <RefPreviewCard target={target} /> : "对象已不存在"}>
      <button
        type="button"
        disabled={!exists}
        onClick={(event) => { event.stopPropagation(); openRef(target); }}
        aria-label={`打开${REF_KIND_LABEL[target.kind]}：${refLabel(state, target)}`}
        {...dragRef(target)}
        className={cn(
          "inline-flex h-[32px] max-w-[260px] items-center gap-1.5 rounded-lg border border-cx-border-subtle bg-cx-bg px-2.5 text-[13px] text-cx-fg-2 outline-none",
          "hover:border-cx-border hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] disabled:opacity-50",
          className,
        )}
      >
        <Icon name={REF_ICON[target.kind]} size={14} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 truncate">{refLabel(state, target)}</span>
      </button>
    </Tooltip>
  );
}

export function RefLinkList({ targets, max = 3, className }: { targets: StudioRef[]; max?: number; className?: string }) {
  const [expanded, setExpanded] = useState(false);
  if (!targets.length) return null;
  const shown = expanded ? targets : targets.slice(0, max);
  const hidden = targets.length - shown.length;
  return (
    <div className={cn("flex flex-wrap items-center gap-1.5", className)}>
      {shown.map((target) => <RefLink key={`${target.kind}:${target.id}`} target={target} />)}
      {hidden > 0 ? (
        <Button size="md" variant="ghost" className={BTN_SM} onClick={(event) => { event.stopPropagation(); setExpanded(true); }} aria-label={`显示其余 ${hidden} 个关联对象`}>
          +{hidden}
        </Button>
      ) : expanded && targets.length > max ? (
        <Button size="md" variant="ghost" className={BTN_SM} onClick={(event) => { event.stopPropagation(); setExpanded(false); }}>收起</Button>
      ) : null}
    </div>
  );
}

export function NewMark({ target }: { target: StudioRef }) {
  const state = useStudioState();
  if (isRecent(state, target)) return <StudioBadge tone="accent">新</StudioBadge>;
  if (isUpdated(state, target)) return <StudioBadge tone="accent">已更新</StudioBadge>;
  return null;
}

export function Meta({ parts, className }: { parts: Array<ReactNode | null | undefined | false>; className?: string }) {
  const shown = parts.filter((part) => part !== null && part !== undefined && part !== false && part !== "");
  return (
    <span className={cn("flex min-w-0 items-center gap-1.5 overflow-hidden text-[13px] text-cx-fg-3", className)}>
      {shown.map((part, index) => (
        <span key={index} className={cn("flex min-w-0 items-center gap-1.5", index === shown.length - 1 ? "min-w-0 truncate" : "shrink-0")}>
          {index ? <span aria-hidden>·</span> : null}
          <span className="min-w-0 truncate">{part}</span>
        </span>
      ))}
    </span>
  );
}

export function WorkspaceToolbar({ children, trailing }: { children: ReactNode; trailing?: ReactNode }) {
  return (
    <div className="flex shrink-0 items-center gap-2 px-4 pb-2 pt-3">
      <div className="flex min-w-0 flex-1 items-center gap-2">{children}</div>
      {trailing}
    </div>
  );
}

export function ScopeNotice() {
  const state = useStudioState();
  const home = state.threads[state.activeThreadId]?.projectId ?? "";
  const scope = state.workspace.projectId;
  if (!home || scope === home) return null;
  const scopeLabel = scope === "all" ? "全部项目" : `「${state.projects.find((project) => project.id === scope)?.title ?? "其他项目"}」`;
  const homeLabel = state.projects.find((project) => project.id === home)?.title ?? "本对话的项目";
  return (
    <div className="mx-4 mb-2 flex min-h-[44px] items-center gap-2 rounded-xl bg-cx-hover px-3 py-1.5" role="status">
      <Icon name="info" size={15} className="shrink-0 text-cx-fg-3" />
      <span className="min-w-0 flex-1 text-[13px] text-cx-fg-2">正在看{scopeLabel}的内容</span>
      <Button size="md" variant="ghost" className={cn(BTN_SM, "max-w-[200px]")} onClick={() => studio.setWorkspaceProject(home)}>
        <span className="truncate">回到「{homeLabel}」</span>
      </Button>
    </div>
  );
}

export function SheetAskBar({ sheet, target, prefill, label }: { sheet: boolean; target: StudioRef | null; prefill?: string; label?: string }) {
  const state = useStudioState();
  if (!sheet) return null;
  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-cx-border-subtle bg-cx-bg px-4 py-2.5" data-testid="studio-sheet-ask">
      <span className="min-w-0 flex-1 truncate text-[14px] text-cx-fg-2">
        {label ?? (target ? `关于：${refLabel(state, target)}` : "回到对话继续提问")}
      </span>
      <Button size="md" variant="primary" icon="sparkles" className={BTN} onClick={() => askAi(target, prefill)}>问 AI</Button>
    </div>
  );
}

const scrollMemory = new Map<string, number>();

export function useRememberedScroll(key: string, active: boolean) {
  const node = useRef<HTMLDivElement | null>(null);
  const ref = useCallback((element: HTMLDivElement | null) => { node.current = element; }, []);
  useLayoutEffect(() => {
    const element = node.current;
    if (!element) return;
    const onScroll = () => {
      if (element.clientHeight) scrollMemory.set(key, element.scrollTop);
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    return () => element.removeEventListener("scroll", onScroll);
  }, [key]);
  useLayoutEffect(() => {
    const element = node.current;
    if (!element || !active) return;
    const saved = scrollMemory.get(key);
    if (saved && Math.abs(element.scrollTop - saved) > 1) element.scrollTop = saved;
  }, [key, active]);
  return { ref, node };
}

export function useElementWidth() {
  const [element, setElement] = useState<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(([entry]) => setWidth(Math.round(entry.contentRect.width)));
    observer.observe(element);
    setWidth(Math.round(element.getBoundingClientRect().width));
    return () => observer.disconnect();
  }, [element]);
  return { ref: setElement, width };
}

export function useViewing(active: boolean, target: StudioRef | null) {
  const kind = target?.kind;
  const id = target?.id;
  useEffect(() => {
    if (!active) return;
    studio.setViewing(kind && id ? { kind, id } : null);
  }, [active, kind, id]);
}


export { displayTitle, RUN_STATE_LABEL, RUN_STATE_TONE } from "./records";
import { RUN_STATE_LABEL } from "./records";

export function HighlightedText({ text, quotes, activeQuote, onSelectQuote }: {
  text: string;
  quotes: Array<{ id: string; quote: string }>;
  activeQuote?: string;
  onSelectQuote?: (id: string) => void;
}) {
  const ranges = quotes
    .map((item) => ({ ...item, start: text.indexOf(item.quote) }))
    .filter((item) => item.start >= 0)
    .sort((a, b) => a.start - b.start);
  const parts: ReactNode[] = [];
  let cursor = 0;
  for (const range of ranges) {
    if (range.start < cursor) continue;
    if (range.start > cursor) parts.push(text.slice(cursor, range.start));
    const active = range.id === activeQuote;
    parts.push(
      <mark
        key={range.id}
        tabIndex={onSelectQuote ? 0 : undefined}
        role={onSelectQuote ? "button" : undefined}
        onClick={onSelectQuote ? () => onSelectQuote(range.id) : undefined}
        onKeyDown={onSelectQuote ? (event) => { if (event.key === "Enter") onSelectQuote(range.id); } : undefined}
        className={cn(
          "rounded-[3px] px-0.5 text-inherit outline-none",
          onSelectQuote && "cursor-pointer",
          active ? "bg-cx-accent-soft text-cx-fg shadow-[inset_0_-2px_0_var(--cx-accent)]" : "bg-cx-hover shadow-[inset_0_-1px_0_var(--cx-border)] hover:bg-cx-active",
          "focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
        )}
      >
        {range.quote}
      </mark>,
    );
    cursor = range.start + range.quote.length;
  }
  if (cursor < text.length) parts.push(text.slice(cursor));
  return <>{parts}</>;
}
