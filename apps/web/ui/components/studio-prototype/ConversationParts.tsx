"use client";

import { useMemo, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Button, IconButton, Popover, StatusDot, Tooltip, toast } from "@/components/chat/ui";
import { CapabilityRefPill } from "@/components/chat/composer/AttachmentTray";
import { useChatPanel } from "@/lib/chatPanelStore";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { STUDIO_TEAM } from "./demoData";
import { categoryLabel, type StudioRef, type StudioRefKind } from "./model";
import {
  STUDIO_SHEET_BREAKPOINT,
  isSheetViewport,
  openRef,
  openWorkspace,
  showFindings,
  turnAnchorId,
  visibleWorkspace,
  workspaceFromSurfaceId,
  type WorkspaceKey,
} from "./nav";
import { BTN, BTN_SM, ICON_BTN_SM, REF_ICON, REF_KIND_LABEL, RefLink } from "./parts";
import { countLabel } from "./records";
import { StudioSaveIndicator } from "./SaveStatus";
import {
  AGENT_MCP_MODEL_LABEL,
  LOCAL_MODEL_LABEL,
  SIMULATION_DESCRIPTION,
  contextParts,
  effectiveContext,
  findPassage,
  readStudioState,
  refExists,
  refLabel,
  studio,
  turnKey,
  useStudioState,
  type StudioSuggestion,
  type SuggestionKind,
} from "./store";

// Reuse the composer's reference pill; its icon set is keyed by capability kind.
export const PILL_KIND: Record<StudioRefKind, string> = {
  source: "file",
  passage: "file",
  clue: "message_span",
  task: "thread",
  report: "file",
  evidence: "file",
  import: "file",
  run: "thread",
  change: "message_span",
  feedback: "message_span",
  plan: "thread",
};

const TRAY_VISIBLE = 2;

export function useVisibleWorkspace(threadId: string, contextual = false): WorkspaceKey | null {
  const panel = useChatPanel(threadId);
  const narrow = useMediaQuery(`(max-width: ${STUDIO_SHEET_BREAKPOINT}px)`);
  if (!panel.isOpen && !(contextual && narrow)) return null;
  return workspaceFromSurfaceId(panel.activeId);
}

export function StudioHeaderActions() {
  const state = useStudioState();
  const threadId = state.activeThreadId;
  const panel = useChatPanel(threadId);
  if (panel.isOpen && workspaceFromSurfaceId(panel.activeId)) return null;
  const last = workspaceFromSurfaceId(panel.activeId) ?? "library";
  return (
    <Button
      size="md"
      variant="ghost"
      icon="panelRightOpen"
      tooltip="在对话旁打开资产库、发现、任务与报告工作区"
      shortcut="mod+alt+1"
      className={cn(BTN, "mr-1")}
      onClick={() => openWorkspace(last)}
    >
      工作区
    </Button>
  );
}

/** Model / Engine toggle in the composer toolbar + Save / Sync status. */
export function StudioComposerControls() {
  const state = useStudioState();
  const isMcp = state.runtimeMode === "simulated_agent_mcp";
  const label = isMcp ? AGENT_MCP_MODEL_LABEL : LOCAL_MODEL_LABEL;
  return (
    <span className="inline-flex min-w-0 items-center gap-0.5">
      <Tooltip content={`${SIMULATION_DESCRIPTION}（点击可切换「本地规则模拟」与「Agent + MCP 仿真引擎」）`}>
        <button
          type="button"
          onClick={() => {
            const next = isMcp ? "local_rules" : "simulated_agent_mcp";
            studio.setRuntimeMode(next);
            toast({
              title: next === "simulated_agent_mcp" ? `已切换为「${AGENT_MCP_MODEL_LABEL}」` : `已切换为「${LOCAL_MODEL_LABEL}」`,
              tone: "success",
              duration: 2400,
            });
          }}
          className="inline-flex h-[32px] min-w-0 items-center gap-2 rounded-lg px-2 text-[13px] text-cx-fg-2 outline-none hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          aria-label={`${label}：${SIMULATION_DESCRIPTION}`}
        >
          <StatusDot tone={isMcp ? "accent" : "warning"} />
          <span className="truncate">{label}</span>
        </button>
      </Tooltip>
      <StudioSaveIndicator />
    </span>
  );
}

function uncheckSelected(ref: StudioRef) {
  const workspace = readStudioState().workspace;
  if (ref.kind === "source") studio.setLibraryUi({ checked: workspace.library.checked.filter((id) => id !== ref.id) });
  else studio.setFindingsUi({ checked: workspace.findings.checked.filter((id) => id !== ref.id) });
}

const SELECTION_PILLS = 3;

function ContextPill({ kind, label, title, missing, onRemove, removeLabel }: {
  kind: StudioRefKind;
  label: string;
  title: string;
  missing?: boolean;
  onRemove: () => void;
  removeLabel: string;
}) {
  return (
    <span className="inline-flex min-w-[72px] max-w-[260px] shrink items-center [&_.cx-ref-pill]:h-[32px] [&_.cx-ref-pill]:max-w-full [&_.cx-ref-pill]:text-[13px] [&_.cx-ref-pill_button]:size-6">
      <CapabilityRefPill
        kind={PILL_KIND[kind]}
        label={label}
        status={missing ? "missing" : undefined}
        title={title}
        onRemove={onRemove}
        removeLabel={removeLabel}
      />
    </span>
  );
}

export function sendNow(threadId: string, text: string) {
  const current = readStudioState();
  const refs = effectiveContext(current, threadId, visibleWorkspace(threadId));
  studio.send(threadId, text, refs, { taskId: current.handoff[threadId] });
}

interface QuickPrompt {
  label: string;
  text: string;
  icon: IconName;
}

const STARTERS: QuickPrompt[] = [
  { label: "还有哪些没核实？", text: "这个项目里还有哪些发现没核实？", icon: "checkCircle" },
  { label: "同源归并建议", text: "检查同源重复的发现并给出归并建议", icon: "listCollapse" },
  { label: "生成报告与计划更新", text: "根据当前上下文更新任务计划并生成报告草稿", icon: "file" },
];

function PromptChip({ prompt, onSend }: { prompt: QuickPrompt; onSend: () => void }) {
  return (
    <button
      type="button"
      onClick={onSend}
      title={`直接发送：${prompt.text}`}
      className="inline-flex h-[32px] shrink-0 items-center gap-1.5 rounded-full border border-cx-border-subtle bg-cx-bg px-3 text-[13px] text-cx-fg-2 outline-none hover:border-cx-border hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
    >
      <Icon name={prompt.icon} size={14} className="shrink-0 text-cx-fg-3" />
      {prompt.label}
    </button>
  );
}

function DetailRow({ icon, title, meta, children }: { icon: IconName; title: ReactNode; meta: ReactNode; children?: ReactNode }) {
  return (
    <div className="flex min-h-[48px] items-center gap-2.5 py-1">
      <Icon name={icon} size={16} className="shrink-0 text-cx-fg-3" />
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-[14px] text-cx-fg">{title}</span>
        <span className="truncate text-[13px] text-cx-fg-3">{meta}</span>
      </span>
      <span className="flex shrink-0 items-center gap-0.5">{children}</span>
    </div>
  );
}

/** Unbound agent suggestions (no turn id): rendered above the composer tray. */
export function PendingSuggestionTray() {
  const state = useStudioState();
  const pending = state.suggestions["_"] ?? [];
  if (!pending.length) return null;
  return (
    <div className="flex w-full flex-col gap-1" data-testid="pending-suggestion-tray">
      {pending.map((sug) => (
        <span key={sug.id} className="flex items-center gap-2 rounded-full border border-cx-border bg-cx-bg-2 px-3 py-1 text-[12px]">
          <span className="text-cx-fg-3">待处理建议</span>
          <span className="min-w-0 flex-1 truncate text-cx-fg">{sug.title || sug.kind}</span>
          <Button size="sm" variant="ghost" onClick={() => studio.adoptSuggestions("_", [sug.id], { threadId: state.activeThreadId, assignee: "local-user" })}>采纳</Button>
          <Button size="sm" variant="ghost" onClick={() => studio.dismissSuggestion(sug.id)}>忽略</Button>
        </span>
      ))}
    </div>
  );
}

export function StudioContextTray({ threadId }: { threadId: string }) {
  const state = useStudioState();
  const visible = useVisibleWorkspace(threadId, true);
  const parts = contextParts(state, threadId, visible);
  const empty = !(state.threads[threadId]?.messages.length);

  const items: Array<{ key: string; ref: StudioRef; node: ReactNode }> = [];
  if (parts.handoffTask) {
    const task = parts.handoffTask;
    items.push({
      key: "handoff",
      ref: { kind: "task", id: task.id },
      node: (
        <ContextPill
          kind="task"
          label={`交给 AI · ${task.title}`}
          title={`发送后由 AI 推进这项任务，并带上它关联的 ${task.links.length} 个对象`}
          onRemove={() => studio.clearHandoff(threadId)}
          removeLabel="取消交给 AI"
        />
      ),
    });
  }
  if (parts.viewing && parts.viewingIncluded) {
    const viewing = parts.viewing;
    items.push({
      key: "viewing",
      ref: viewing,
      node: (
        <span className="inline-flex min-w-0 shrink" data-testid="studio-viewing-pill">
          <ContextPill
            kind={viewing.kind}
            label={`正在看 · ${refLabel(state, viewing)}`}
            title="工作区里正在看的内容会自动带上；换一个对象后跟着变化"
            missing={!refExists(state, viewing)}
            onRemove={() => studio.setViewingExcluded(true)}
            removeLabel="这条消息不带上正在看的内容"
          />
        </span>
      ),
    });
  }
  if (parts.selectionIncluded) {
    const selectedKind = parts.selected[0]?.kind === "source" ? "source" : "clue";
    const where = selectedKind === "source" ? "资产库里勾选的资产" : "发现工作区里勾选的发现";
    if (parts.selected.length > SELECTION_PILLS) {
      items.push({
        key: "selection",
        ref: parts.selected[0],
        node: (
          <ContextPill
            kind={selectedKind}
            label={`勾选的 ${countLabel(selectedKind, parts.selected.length)}`}
            title={`${where}都会带上；在工作区里改勾选，这里跟着变化`}
            onRemove={() => studio.setSelectionExcluded(true)}
            removeLabel="这条消息不带上勾选的记录"
          />
        ),
      });
    } else {
      parts.selected.forEach((ref) => {
        items.push({
          key: `sel:${ref.kind}:${ref.id}`,
          ref,
          node: (
            <ContextPill
              kind={ref.kind}
              label={refLabel(state, ref)}
              title={where}
              missing={!refExists(state, ref)}
              onRemove={() => uncheckSelected(ref)}
              removeLabel={`不带上 ${refLabel(state, ref)}`}
            />
          ),
        });
      });
    }
  }
  parts.pinned.forEach((ref) => {
    items.push({
      key: `pin:${ref.kind}:${ref.id}`,
      ref,
      node: (
        <ContextPill
          kind={ref.kind}
          label={refLabel(state, ref)}
          title={`${REF_KIND_LABEL[ref.kind]}：${refLabel(state, ref)}`}
          missing={!refExists(state, ref)}
          onRemove={() => studio.removeContext(threadId, ref)}
          removeLabel={`不再带上 ${refLabel(state, ref)}`}
        />
      ),
    });
  });

  const refs = items.map((item) => item.ref);
  const pendingApprovals = state.approvals.filter((a) => a.status === "pending").length;
  // Starter chips drive studio.send (mock flowback); real threads never show them.
  const prompts = empty && state.hostTag === "demo" ? STARTERS : [];
  if (!items.length && !prompts.length && !pendingApprovals) return null;

  const shown = items.slice(0, TRAY_VISIBLE);
  const hidden = items.length - shown.length;
  const detail = (
    <div className="flex flex-col gap-1">
      <p className="px-1 text-[13px] text-cx-fg-3">下一条消息会带上这些内容</p>
      <div className="cx-scroll flex max-h-[min(420px,60vh)] flex-col divide-y divide-cx-border-subtle overflow-y-auto px-1">
        {items.map((item) => (
          item.key === "selection" ? (
            <DetailRow
              key={item.key}
              icon={REF_ICON[item.ref.kind]}
              title={`勾选的 ${countLabel(item.ref.kind === "source" ? "source" : "clue", parts.selected.length)}`}
              meta="在工作区里勾选 · 自动带上"
            >
              <IconButton size="md" icon="x" label="不带上" className={ICON_BTN_SM} onClick={() => studio.setSelectionExcluded(true)} />
            </DetailRow>
          ) : (
            <DetailRow
              key={item.key}
              icon={REF_ICON[item.ref.kind]}
              title={item.key === "viewing" ? `正在看 · ${refLabel(state, item.ref)}` : refLabel(state, item.ref)}
              meta={`${REF_KIND_LABEL[item.ref.kind]}${item.key === "viewing" ? " · 自动带上" : item.key.startsWith("sel:") ? " · 勾选" : ""}${refExists(state, item.ref) ? "" : " · 已不存在"}`}
            >
              <IconButton size="md" icon="arrowUpRight" label="在工作区打开" className={ICON_BTN_SM} onClick={() => openRef(item.ref)} />
              <IconButton
                size="md"
                icon="x"
                label="不带上"
                className={ICON_BTN_SM}
                onClick={() => {
                  if (item.key === "viewing") studio.setViewingExcluded(true);
                  else if (item.key === "handoff") studio.clearHandoff(threadId);
                  else if (item.key.startsWith("sel:")) uncheckSelected(item.ref);
                  else studio.removeContext(threadId, item.ref);
                }}
              />
            </DetailRow>
          )
        ))}
      </div>
      {parts.pinned.length > 1 ? (
        <div className="border-t border-cx-border-subtle px-1 pt-1.5">
          <Button size="md" variant="ghost" className={BTN_SM} onClick={() => studio.clearContext(threadId)}>全部移除</Button>
        </div>
      ) : null}
    </div>
  );

  return (
    <div className="mb-2 flex min-w-0 flex-col gap-2 px-1" aria-label="下一条消息的上下文" data-testid="studio-context-tray">
      {items.length ? (
        <div className="flex min-w-0 flex-nowrap items-center gap-1.5 overflow-hidden">
          {shown.map((item) => <span key={item.key} className="inline-flex min-w-0 shrink items-center">{item.node}</span>)}
          {hidden > 0 ? (
            <Popover
              placement="top-end"
              ariaLabel="下一条消息的全部上下文"
              className="w-[min(420px,92vw)] p-2"
              trigger={<Button size="md" variant="ghost" className={cn(BTN_SM, "shrink-0")} aria-label={`查看全部上下文，另有 ${hidden} 项`}>+{hidden}</Button>}
            >
              {detail}
            </Popover>
          ) : null}
        </div>
      ) : null}
      {pendingApprovals ? (
        <button
          type="button"
          onClick={() => { openWorkspace("tasks"); studio.setTasksUi({ tab: "scope" }); }}
          className="flex w-fit items-center gap-1.5 rounded-lg px-1.5 py-1 text-[12px] font-medium text-cx-fg-2 outline-none hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
          data-testid="studio-pending-approvals"
        >
          <StatusDot tone="warning" />
          Agent 等待确认 {pendingApprovals} 项
          <Icon name="chevronRight" size={12} className="text-cx-fg-4" />
        </button>
      ) : null}
      {prompts.length ? (
        <div className="cx-scroll flex min-w-0 items-center gap-1.5 overflow-x-auto" aria-label={refs.length ? "针对这些内容" : "可以这样开始"}>
          {prompts.map((prompt) => <PromptChip key={prompt.label} prompt={prompt} onSend={() => sendNow(threadId, prompt.text)} />)}
        </div>
      ) : null}
    </div>
  );
}

const SUGGESTION_KIND: Record<SuggestionKind, { label: string; icon: IconName }> = {
  recategorize: { label: "改分类", icon: "layers" },
  finding: { label: "新发现", icon: "bookmark" },
  merge: { label: "AI 归并建议", icon: "listCollapse" },
  conflict_resolve: { label: "裁决字段冲突", icon: "gitCompare" },
  report_draft: { label: "生成/更新报告", icon: "file" },
  plan_update: { label: "修订任务计划", icon: "compass" },
  retest_record: { label: "登记复核结论", icon: "refresh" },
  task: { label: "新任务", icon: "listTodo" },
};
const SUGGESTION_ORDER: SuggestionKind[] = [
  "merge",
  "conflict_resolve",
  "report_draft",
  "plan_update",
  "retest_record",
  "recategorize",
  "finding",
  "task",
];
const SUGGESTION_PREVIEW = 5;

function SuggestionRow({ item, onAdopt }: { item: StudioSuggestion; onAdopt: () => void }) {
  const state = useStudioState();
  const hit = item.passageId ? findPassage(state, item.passageId) : undefined;
  const meta = SUGGESTION_KIND[item.kind] ?? SUGGESTION_KIND.task;
  let main: ReactNode = item.title;
  let detail: ReactNode = item.rationale ?? meta.label;
  if (item.kind === "recategorize" && item.from && item.to) {
    detail = <>{meta.label} · {categoryLabel(item.from)} <Icon name="arrowRight" size={12} className="inline" /> <span className="text-cx-fg-2">{categoryLabel(item.to)}</span></>;
  } else if (item.kind === "finding") {
    main = <span className="line-clamp-2 whitespace-normal">「{item.quote}」</span>;
    detail = `${meta.label}${hit ? ` · 《${hit.source.title}》第 ${hit.passage.index} 段` : ""}`;
  }
  return (
    <div className="flex min-h-[52px] items-center gap-3 px-3 py-2">
      <Icon name={item.result ? "checkCircle" : meta.icon} size={16} className={cn("shrink-0", item.result ? "text-cx-success" : "text-cx-fg-3")} />
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className={cn("min-w-0 truncate text-[14px]", item.result ? "text-cx-fg-3" : "text-cx-fg")}>{main}</span>
        <span className="min-w-0 truncate text-[13px] text-cx-fg-3">{detail}</span>
      </span>
      {item.result ? (
        <button type="button" className="shrink-0 text-[12px] font-medium text-cx-accent hover:underline" onClick={() => openRef(item.result!)}>已采纳 · 查看</button>
      ) : (
        <button type="button" className="shrink-0 text-[12px] font-medium text-cx-accent hover:underline" onClick={onAdopt}>采纳</button>
      )}
    </div>
  );
}

/** Under a settled reply: what it worked on, what it cited (all object types!), and changes it proposes. */
export function StudioTurnAccessory({ threadId, turnId }: { threadId: string; turnId: string }) {
  const state = useStudioState();
  const key = turnKey(threadId, turnId);
  const ids = state.turnFindings[key];
  const turnRefs = state.turnRefs[key] ?? [];
  const otherRefs = turnRefs.filter((r) => r.kind !== "clue");
  const taskId = state.turnTasks[key];
  const suggestions = useMemo(
    () => [...(state.suggestions[key] ?? [])].sort((a, b) => SUGGESTION_ORDER.indexOf(a.kind) - SUGGESTION_ORDER.indexOf(b.kind)),
    [key, state.suggestions],
  );
  const [expanded, setExpanded] = useState(false);
  const pending = suggestions.filter((item) => !item.result);
  const emptyNotes = SUGGESTION_ORDER.flatMap((kind) => {
    const reason = state.suggestionEmpty[key]?.[kind];
    return reason ? [{ kind, reason }] : [];
  });
  const anchor = <span id={turnAnchorId(turnId)} className="block scroll-mt-24" aria-hidden />;
  if (!ids?.length && !otherRefs.length && !suggestions.length && !taskId && !emptyNotes.length) return anchor;

  const adopt = (suggestionIds: string[]) => {
    const result = studio.adoptSuggestions(key, suggestionIds, { threadId, assignee: STUDIO_TEAM[1] ?? STUDIO_TEAM[0] ?? "" });
    const allRefs = [...result.updated, ...result.created];
    if (!allRefs.length) return;
    const clues = allRefs.filter((ref) => ref.kind === "clue");
    const first = allRefs[0];
    const reveal = () => (clues.length ? showFindings(clues.map((ref) => ref.id), "刚采纳的改动") : openRef(first));
    const sheet = isSheetViewport();
    if (!sheet) reveal();
    toast({
      title: `已采纳 ${allRefs.length} 项 AI 建议并写回工作区`,
      description: "所有改动均已记录来源追溯，可在「处理记录」中随时撤销。",
      tone: "success",
      action: { label: "在工作区查看", onClick: reveal },
    });
  };

  const shown = expanded ? suggestions : suggestions.slice(0, SUGGESTION_PREVIEW);
  const more = suggestions.length - shown.length;

  return (
    <div className="mt-3 flex flex-col gap-3" data-testid="studio-turn-accessory">
      {anchor}
      {taskId ? (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-[13px] text-cx-fg-3">推进了任务</span>
          <RefLink target={{ kind: "task", id: taskId }} />
        </div>
      ) : null}
      {(ids?.length || otherRefs.length) ? (
        <Popover
          placement="top-start"
          ariaLabel="这条回复引用的对象"
          className="w-[min(360px,92vw)] p-1.5"
          trigger={(
            <Button size="sm" variant="ghost" className="text-cx-fg-3" aria-label="查看这条回复引用的对象">
              引用 {(ids?.length ?? 0) + otherRefs.length} 项
            </Button>
          )}
        >
          <div className="cx-scroll flex max-h-[min(320px,60vh)] flex-col divide-y divide-cx-border-subtle overflow-y-auto">
            {[...(ids ?? []).map((id): StudioRef => ({ kind: "clue", id })), ...otherRefs]
              .filter((ref, index, list) => list.findIndex((item) => item.kind === ref.kind && item.id === ref.id) === index)
              .map((ref) => (
                <div key={`${ref.kind}:${ref.id}`} className="flex items-center gap-2 px-2 py-1.5">
                  <Icon name={REF_ICON[ref.kind]} size={14} className="shrink-0 text-cx-fg-3" />
                  <span className="min-w-0 flex-1 truncate text-[13px] text-cx-fg">{refLabel(state, ref)}</span>
                  <button type="button" className="shrink-0 text-[12px] font-medium text-cx-accent hover:underline" onClick={() => openRef(ref)}>查看</button>
                </div>
              ))}
          </div>
          {(ids?.length ?? 0) > 4 ? (
            <button type="button" className="mt-1 w-full rounded-lg px-2 py-1.5 text-left text-[12px] font-medium text-cx-accent hover:bg-cx-hover" onClick={() => showFindings(ids, "这条回复引用的发现")}>在工作区查看全部</button>
          ) : null}
        </Popover>
      ) : null}
      {emptyNotes.length ? (
        <div className="flex flex-col gap-1.5" data-testid="studio-suggestions-empty">
          {emptyNotes.map((note) => (
            <p key={note.kind} className="flex items-start gap-2 text-[13px] leading-5 text-cx-fg-3">
              <Icon name="checkCircle" size={14} className="mt-0.5 shrink-0" />
              <span className="min-w-0 flex-1">{note.reason}</span>
            </p>
          ))}
        </div>
      ) : null}
      {suggestions.length ? (
        <section className="flex flex-col overflow-hidden rounded-2xl bg-cx-elevated shadow-cx-sm" aria-label="AI 建议的改动" data-testid="studio-suggestions">
          <div className="flex items-center gap-2 px-3 py-2">
            <Icon name="sparkles" size={15} className="shrink-0 text-cx-accent" />
            <span className="min-w-0 flex-1 text-[13px] font-medium text-cx-fg">建议改动 {suggestions.length} 项</span>
            {pending.length ? (
              <button type="button" className="text-[12px] font-medium text-cx-accent hover:underline" onClick={() => adopt(pending.map((item) => item.id))}>
                {pending.length === suggestions.length ? "全部采纳" : `采纳其余 ${pending.length} 项`}
              </button>
            ) : <span className="text-[12px] text-cx-fg-4">已全部采纳</span>}
          </div>
          <div className="flex flex-col divide-y divide-cx-border-subtle">
            {shown.map((item) => <SuggestionRow key={item.id} item={item} onAdopt={() => adopt([item.id])} />)}
          </div>
          {more > 0 || (expanded && suggestions.length > SUGGESTION_PREVIEW) ? (
            <Button size="md" variant="ghost" className={cn(BTN_SM, "m-1 self-start")} onClick={() => setExpanded((value) => !value)}>
              {expanded ? "收起" : `再显示 ${more} 项`}
            </Button>
          ) : null}
        </section>
      ) : null}
    </div>
  );
}
