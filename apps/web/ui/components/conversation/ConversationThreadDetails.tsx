"use client";

import { useCallback, useEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import type { ConversationView } from "@/lib/useConversation";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { useOpenThreadInEditor } from "./ThreadGitActions";
import { resolveWorkspaceBranchChip } from "@/lib/workspaceBranchDisplay";
import { fetchWorktreeDiff } from "@/lib/conversationDiff";
import { chatPanel, type SingletonKind } from "@/lib/chatPanelStore";
import { setThreadDetailsCardHeight } from "@/lib/threadDetailsOverlayStore";
import { ThreadGitActions } from "./ThreadGitActions";
import { Icon, type IconName } from "@/components/Icon";
import { Badge, DiffStat, IconButton, Popover, Spinner, toast } from "@/components/chat/ui";
import {
  relativeTime,
  statusPresentation,
  useNow,
  workspaceKindLabel,
  workspaceLabel,
} from "@/components/chat/surfaces/shared";

/** Below this main-column width the card would cover the transcript, so it becomes a popover. */
const FLOATING_MIN_WIDTH = 880;

type ChangesState =
  | { phase: "idle" | "loading" }
  | { phase: "ready"; additions: number; deletions: number; files: number }
  | { phase: "error"; message: string };

function copy(value: string, label: string) {
  void copyToClipboard(value).then((ok) => {
    toast({ title: ok ? `已复制${label}` : "复制失败，请手动复制", tone: ok ? "success" : "danger", duration: 1800 });
  });
}

function useElementWidth(ref: RefObject<HTMLElement | null>): number {
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    setWidth(node.clientWidth);
    const observer = new ResizeObserver(() => setWidth(node.clientWidth));
    observer.observe(node);
    return () => observer.disconnect();
  }, [ref]);
  return width;
}

function useWorktreeChanges(threadId: string, enabled: boolean, revision: string): [ChangesState, () => void] {
  const [state, setState] = useState<ChangesState>({ phase: "idle" });
  const [nonce, setNonce] = useState(0);
  useEffect(() => {
    if (!enabled) {
      setState({ phase: "idle" });
      return;
    }
    const controller = new AbortController();
    setState((current) => (current.phase === "ready" ? current : { phase: "loading" }));
    fetchWorktreeDiff(threadId, { signal: controller.signal })
      .then((data) => {
        if (!data.is_repo) setState({ phase: "error", message: "不是 Git 仓库" });
        else setState({ phase: "ready", additions: data.additions || 0, deletions: data.deletions || 0, files: (data.files || []).length });
      })
      .catch((error: unknown) => {
        if (controller.signal.aborted) return;
        setState({ phase: "error", message: error instanceof Error ? error.message : "变更读取失败" });
      });
    return () => controller.abort();
  }, [threadId, enabled, revision, nonce]);
  return [state, () => setNonce((value) => value + 1)];
}

function Row({
  icon,
  label,
  detail,
  trailing,
  onClick,
  title,
  actions,
}: {
  icon: IconName;
  label: ReactNode;
  detail?: ReactNode;
  trailing?: ReactNode;
  onClick?: () => void;
  title?: string;
  actions?: ReactNode;
}) {
  const body = (
    <>
      <Icon name={icon} size={14} className="mt-[3px] shrink-0 text-cx-fg-3" />
      <span className="flex min-w-0 flex-1 flex-col">
        <span className="truncate text-[13px] text-cx-fg">{label}</span>
        {detail ? <span className="truncate font-cx-mono text-[11.5px] text-cx-fg-4">{detail}</span> : null}
      </span>
      {trailing ? <span className="flex shrink-0 items-center gap-1.5 pt-px text-[12px] text-cx-fg-3">{trailing}</span> : null}
    </>
  );
  return (
    <div className="group/row relative">
      {onClick ? (
        <button
          type="button"
          title={title}
          onClick={onClick}
          className="flex w-full items-start gap-2.5 rounded-lg px-2 py-1.5 text-left outline-none transition-colors hover:bg-cx-hover focus-visible:bg-cx-hover"
        >
          {body}
        </button>
      ) : (
        <div title={title} className="flex w-full items-start gap-2.5 rounded-lg px-2 py-1.5">{body}</div>
      )}
      {actions ? (
        <span className="absolute right-1 top-1 flex items-center gap-0.5 rounded-md bg-cx-overlay opacity-0 transition-opacity group-hover/row:opacity-100 group-focus-within/row:opacity-100">
          {actions}
        </span>
      ) : null}
    </div>
  );
}

const QUICK_VIEWS: { kind: SingletonKind; label: string; icon: IconName; needsWorkspace?: boolean }[] = [
  { kind: "files", label: "文件", icon: "folderTree", needsWorkspace: true },
  { kind: "terminal", label: "终端", icon: "terminal", needsWorkspace: true },
  { kind: "overview", label: "概览", icon: "layers" },
  { kind: "plan", label: "计划", icon: "listTodo" },
];

function DetailsBody({
  view,
  projectName,
  onOpenInfo,
  onDone,
}: {
  view: ConversationView;
  projectName?: string;
  onOpenInfo: () => void;
  onDone: () => void;
}) {
  const threadId = view.thread.thread_id;
  const projectId = view.thread.project_id || view.workspace?.project_id || undefined;
  const git = useSharedGitStatus(threadId, projectId);
  const editor = useOpenThreadInEditor(view);
  const root = view.workspace?.root_path || "";
  const hasWorkspace = Boolean(root);
  const branch = view.workspace
    ? resolveWorkspaceBranchChip({
      git: git.status,
      gitError: git.error || null,
      gitLoading: git.loading,
      settingsBranch: typeof view.workspace.settings?.branch === "string" ? view.workspace.settings.branch.trim() : "",
      mode: String(view.workspace.settings?.mode || ""),
      rootPath: root,
      kind: view.workspace.kind,
    })
    : null;
  const isRepo = git.status ? git.status.is_repo : view.workspace?.kind === "git" || Boolean(branch);
  const running = Boolean(view.state.running_turn_id);
  // Re-read once a turn finishes or the shared Git status refreshes.
  const [changes, refreshChanges] = useWorktreeChanges(
    threadId,
    hasWorkspace && isRepo,
    `${view.state.running_turn_id || ""}:${view.turns.length}:${git.revision}`,
  );
  const now = useNow(true);
  const status = statusPresentation(view);
  const lastActivity = view.thread.updated_at || view.thread.created_at;

  const openView = (kind: SingletonKind) => {
    chatPanel.open(threadId, kind);
    onDone();
  };

  return (
    <div className="flex flex-col p-1.5" data-cx-thread-details="">
      <Row
        icon="folder"
        label={projectName || workspaceLabel(view)}
        detail={root || "未绑定工作区"}
        title={root || undefined}
        onClick={hasWorkspace ? () => openView("files") : undefined}
        actions={root ? (
          <>
            {editor.desktop ? (
              <IconButton icon="code" label="在编辑器中打开" size="xs" loading={editor.busy} onClick={() => void editor.open()} />
            ) : (
              <IconButton icon="code" label="在 VS Code 中打开（服务运行在本机时有效）" size="xs" onClick={editor.openVscodeLink} />
            )}
            <IconButton icon="copy" label="复制路径" size="xs" onClick={() => copy(root, "路径")} />
          </>
        ) : null}
      />
      {branch ? (
        <Row
          icon="gitBranch"
          label={branch.label}
          title={branch.title}
          trailing={(
            <>
              {git.status?.dirty ? <span className="size-1.5 rounded-full bg-cx-warning" aria-label="有未提交改动" /> : null}
              <span>{workspaceKindLabel(view)}</span>
            </>
          )}
          actions={git.status?.current_branch
            ? <IconButton icon="copy" label="复制分支名" size="xs" onClick={() => copy(git.status?.current_branch || "", "分支名")} />
            : null}
        />
      ) : null}
      {hasWorkspace && isRepo ? <ThreadGitActions view={view} variant="details" /> : null}
      {hasWorkspace && isRepo ? (
        <Row
          icon="fileDiff"
          label="变更"
          title="打开工作树变更"
          onClick={() => {
            chatPanel.openDiff(threadId, { kind: "worktree" });
            onDone();
          }}
          trailing={
            changes.phase === "ready" ? (
              changes.files ? (
                <>
                  <span className="cx-tabular">{changes.files} 个文件</span>
                  <DiffStat additions={changes.additions} deletions={changes.deletions} />
                </>
              ) : <span>无改动</span>
            ) : changes.phase === "error" ? (
              <span className="max-w-[140px] truncate text-cx-danger" title={changes.message}>{changes.message}</span>
            ) : <Spinner size={12} />
          }
          actions={<IconButton icon="refresh" label="刷新变更" size="xs" onClick={refreshChanges} />}
        />
      ) : null}

      <div className="mx-2 my-1.5 h-px bg-cx-border-subtle" />

      <div className="flex items-center gap-2 px-2 py-1 text-[12px] text-cx-fg-3">
        <Badge tone={status.tone}>
          {running ? <Spinner size={10} /> : null}
          {status.label}
        </Badge>
        <span className="cx-tabular">{view.turns.length} 轮</span>
        {lastActivity ? <span className="ml-auto truncate" title={new Date(lastActivity).toLocaleString()}>{relativeTime(lastActivity, now)}</span> : null}
      </div>

      <div className="mt-1 grid grid-cols-4 gap-1 px-1">
        {QUICK_VIEWS.map((item) => {
          const disabled = Boolean(item.needsWorkspace && !hasWorkspace);
          return (
            <button
              key={item.kind}
              type="button"
              disabled={disabled}
              title={disabled ? "当前会话未绑定工作区" : `打开${item.label}`}
              onClick={() => openView(item.kind)}
              className="cx-press flex flex-col items-center gap-1 rounded-lg py-2 text-[11.5px] text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:bg-cx-hover disabled:cursor-not-allowed disabled:opacity-40"
            >
              <Icon name={item.icon} size={15} />
              {item.label}
            </button>
          );
        })}
      </div>

      <Row
        icon="info"
        label="会话信息与记忆"
        onClick={() => {
          onOpenInfo();
          onDone();
        }}
        trailing={<Icon name="chevronRight" size={12} className="text-cx-fg-4" />}
      />
    </div>
  );
}

const OPEN_KEY = "muteki.chat.thread-details.open";

export interface ThreadDetailsState {
  /** Wide enough to float the card beside the transcript. */
  floating: boolean;
  /** The card or popover is showing right now. */
  visible: boolean;
  /** Floating card: persisted pin. Narrow: a transient popover. */
  toggle: () => void;
  close: () => void;
  measured: boolean;
}

/**
 * The floating card is a persisted preference; in a narrow column it turns into
 * a popover that opens only on an explicit toggle, so opening the right panel
 * never pops anything over the transcript.
 */
export function useThreadDetailsState(containerRef: RefObject<HTMLElement | null>): ThreadDetailsState {
  const width = useElementWidth(containerRef);
  const floating = width >= FLOATING_MIN_WIDTH;
  const [pinned, setPinnedState] = useState(false);
  const [popoverOpen, setPopoverOpen] = useState(false);
  useEffect(() => {
    try { setPinnedState(window.localStorage.getItem(OPEN_KEY) === "1"); } catch { /* default closed */ }
  }, []);
  useEffect(() => {
    if (floating) setPopoverOpen(false);
  }, [floating]);
  const setPinned = useCallback((next: boolean) => {
    setPinnedState(next);
    try { window.localStorage.setItem(OPEN_KEY, next ? "1" : "0"); } catch { /* session only */ }
  }, []);
  const toggle = useCallback(() => {
    if (floating) setPinned(!pinned);
    else setPopoverOpen((current) => !current);
  }, [floating, pinned, setPinned]);
  const close = useCallback(() => {
    if (floating) setPinned(false);
    else setPopoverOpen(false);
  }, [floating, setPinned]);
  return { floating, visible: floating ? pinned : popoverOpen, toggle, close, measured: width > 0 };
}

/**
 * Thread details: floats over the top-right of the transcript when there is
 * room (stays open while you work), otherwise opens as a popover.
 */
export function ConversationThreadDetails({
  view,
  projectName,
  state,
  anchorRef,
  onOpenInfo,
}: {
  view: ConversationView;
  projectName?: string;
  state: ThreadDetailsState;
  anchorRef: RefObject<HTMLElement | null>;
  onOpenInfo: () => void;
}) {
  const cardRef = useRef<HTMLDivElement | null>(null);
  const { floating, visible, close } = state;
  const pinnedCard = state.measured && floating && visible;

  useEffect(() => {
    const node = cardRef.current;
    if (!pinnedCard || !node) {
      setThreadDetailsCardHeight(0);
      return;
    }
    setThreadDetailsCardHeight(node.offsetHeight);
    const observer = new ResizeObserver(() => setThreadDetailsCardHeight(node.offsetHeight));
    observer.observe(node);
    return () => {
      observer.disconnect();
      setThreadDetailsCardHeight(0);
    };
  }, [pinnedCard]);

  useEffect(() => {
    if (!visible || !floating) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || !cardRef.current?.contains(document.activeElement)) return;
      event.preventDefault();
      close();
      anchorRef.current?.focus({ preventScroll: true });
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [visible, floating, close, anchorRef]);

  if (!state.measured) return null;
  if (!floating) {
    return (
      <Popover
        open={visible}
        onOpenChange={(next) => { if (!next) close(); }}
        anchorRef={anchorRef}
        placement="bottom-end"
        ariaLabel="对话详情"
        initialFocus="container"
        className="w-[300px]"
      >
        <DetailsBody view={view} projectName={projectName} onOpenInfo={onOpenInfo} onDone={close} />
      </Popover>
    );
  }

  if (!visible) return null;
  return (
    <div
      ref={cardRef}
      role="complementary"
      aria-label="对话详情"
      data-testid="thread-details-card"
      className={cn(
        "cx-animate-in absolute right-3 top-[calc(var(--cx-header-h,52px)+8px)] z-20 w-[300px]",
        "rounded-xl border border-cx-border-subtle bg-cx-overlay shadow-cx-md",
      )}
    >
      <DetailsBody view={view} projectName={projectName} onOpenInfo={onOpenInfo} onDone={() => {}} />
    </div>
  );
}
