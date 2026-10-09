"use client";

import { useEffect, useRef, useState, type ReactNode } from "react";
import type { ConversationView, ProjectGitStatus } from "@/lib/useConversation";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import {
  ThreadGitError,
  commitThreadGit,
  createThreadPullRequest,
  pullThreadGit,
  pushThreadGit,
  useThreadPullRequests,
  type ThreadPullRequestListing,
} from "@/lib/threadGitActions";
import {
  nativeDisplayMessage,
  openNativeWorkspaceInEditor,
  selectNativeWorkspaceRoot,
  useNativeDesktopState,
  useNativeWorkspaceGrant,
} from "@/lib/nativeDesktop";
import { chatPanel } from "@/lib/chatPanelStore";
import { readChatPreferences } from "@/lib/chatPreferences";
import { type IconName } from "@/components/Icon";
import {
  Button,
  Callout,
  Checkbox,
  Dialog,
  IconButton,
  Input,
  Label,
  Menu,
  MenuItem,
  MenuSeparator,
  TextArea,
  toast,
} from "@/components/chat/ui";

type GitBusy = "commit" | "push" | "pull" | "pr" | null;
type PrimaryKind = "commit" | "push" | "pull" | "create-pr" | "view-pr";

function errorText(error: unknown): { title: string; hint: string } {
  if (error instanceof ThreadGitError) return { title: error.message, hint: error.recoveryHint };
  if (error instanceof Error) return { title: error.message, hint: "" };
  return { title: String(error), hint: "" };
}

function toastError(title: string, error: unknown) {
  const { title: message, hint } = errorText(error);
  toast({
    title,
    description: (
      <span className="whitespace-pre-wrap break-words font-cx-mono text-[11.5px]">
        {message}
        {hint ? <span className="mt-1 block font-sans text-[12px] text-cx-fg-3">{hint}</span> : null}
      </span>
    ),
    tone: "danger",
    duration: 9000,
  });
}

function copyText(value: string, label: string) {
  void copyToClipboard(value).then((ok) => {
    toast({ title: ok ? `已复制${label}` : "复制失败，请手动复制", tone: ok ? "success" : "danger", duration: 1800 });
  });
}

const NON_GITHUB_STATES = new Set<ThreadPullRequestListing["state"]>(["no_workspace", "no_git", "no_remote", "not_github"]);

function isGithubListing(listing: ThreadPullRequestListing | null): boolean {
  return Boolean(listing && !NON_GITHUB_STATES.has(listing.state));
}

function openPrFrom(listing: ThreadPullRequestListing | null) {
  return listing?.pull_requests.find((pr) => pr.state === "open" || pr.state === "draft") ?? null;
}

/** State-driven primary action, in the order a user would resolve them. */
function primaryAction(status: ProjectGitStatus, listing: ThreadPullRequestListing | null): PrimaryKind | null {
  if (status.detached_head || !status.current_branch) return null;
  if (status.dirty) return "commit";
  if ((status.behind ?? 0) > 0) return "pull";
  const hasRemote = (status.remotes?.length ?? 0) > 0;
  if (hasRemote && (!status.upstream || (status.ahead ?? 0) > 0)) return "push";
  if (openPrFrom(listing)) return "view-pr";
  if (status.upstream && listing?.state === "no_pr") return "create-pr";
  return null;
}

const PRIMARY_COPY: Record<PrimaryKind, { label: string; icon: IconName }> = {
  commit: { label: "提交…", icon: "gitCommit" },
  push: { label: "推送", icon: "upload" },
  pull: { label: "拉取", icon: "download" },
  "create-pr": { label: "创建 PR…", icon: "gitPullRequest" },
  "view-pr": { label: "查看 PR", icon: "gitPullRequest" },
};

function vscodeUrl(root: string): string {
  const normalized = root.replace(/\\/g, "/");
  return `vscode://file${normalized.startsWith("/") ? "" : "/"}${encodeURI(normalized)}`;
}

/**
 * "在编辑器中打开" for the thread workspace. Desktop opens the user-mapped
 * client directory; the web build can only offer a vscode:// link for the
 * service path, which is valid when the service runs on this machine.
 */
export function useOpenThreadInEditor(view: ConversationView | null) {
  const { desktop, state } = useNativeDesktopState();
  const threadId = view?.thread.thread_id || "";
  const workspaceId = view?.workspace?.workspace_id || "";
  const serviceRoot = view?.workspace?.root_path || "";
  const context = { threadId, workspaceId, serviceRoot };
  const grant = useNativeWorkspaceGrant(state, context);
  const [busy, setBusy] = useState(false);

  const openDesktop = async (relativePath?: string, line?: number) => {
    if (!threadId || !workspaceId || !serviceRoot) return;
    setBusy(true);
    try {
      const mapped = grant ?? await selectNativeWorkspaceRoot(state, context);
      if (!mapped) return;
      const preferred = readChatPreferences().editor;
      const result = await openNativeWorkspaceInEditor(state, context, mapped, relativePath, line, preferred);
      toast({
        title: result.opener !== "system" ? `已在 ${result.label} 中打开`
          : preferred === "system" ? "已用系统默认应用打开"
          : "未找到 VS Code / Cursor / Windsurf / Zed 命令行，已用系统默认应用打开",
        description: <span className="break-all font-cx-mono text-[11.5px]">{result.path}</span>,
        tone: "success",
        duration: 3200,
      });
    } catch (error) {
      toast({
        title: "无法在编辑器中打开",
        description: nativeDisplayMessage(error instanceof Error ? error : { message: String(error) }, false),
        tone: "danger",
      });
    } finally {
      setBusy(false);
    }
  };

  return {
    available: Boolean(serviceRoot),
    desktop,
    busy,
    serviceRoot,
    mappedRoot: grant?.clientRoot || "",
    open: openDesktop,
    openVscodeLink: () => { if (serviceRoot) window.location.assign(vscodeUrl(serviceRoot)); },
  };
}

/** Menu items for the editor action; shared by the Git ⋯ menu and other workspace menus. */
export function OpenInEditorMenuItems({ editor }: { editor: ReturnType<typeof useOpenThreadInEditor> }) {
  if (!editor.available) return null;
  if (editor.desktop) {
    return (
      <MenuItem
        icon="code"
        disabled={editor.busy}
        description={editor.mappedRoot ? undefined : "首次使用需选择本机对应目录"}
        onSelect={() => void editor.open()}
      >
        在编辑器中打开
      </MenuItem>
    );
  }
  return (
    <>
      <MenuItem icon="code" description="仅当服务运行在本机时有效" onSelect={editor.openVscodeLink}>
        在 VS Code 中打开
      </MenuItem>
      <MenuItem icon="copy" onSelect={() => copyText(editor.serviceRoot, "工作区路径")}>复制工作区路径</MenuItem>
    </>
  );
}

function submitShortcut(event: React.KeyboardEvent, submit: () => void) {
  if (event.key === "Enter" && (event.metaKey || event.ctrlKey) && !event.nativeEvent.isComposing) {
    event.preventDefault();
    submit();
  }
}

export function CommitDialog({
  open,
  onOpenChange,
  threadId,
  status,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  threadId: string;
  status: ProjectGitStatus | null;
}) {
  const [message, setMessage] = useState("");
  const [amend, setAmend] = useState(false);
  const [busy, setBusy] = useState<"commit" | "commit-push" | null>(null);
  const [error, setError] = useState<{ title: string; hint: string } | null>(null);
  const messageRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setAmend(false);
  }, [open]);

  const files = status?.changed_file_count ?? 0;
  const canPush = (status?.remotes?.length ?? 0) > 0;
  const empty = !message.trim();

  const run = async (push: boolean) => {
    if (busy) return;
    if (empty) {
      setError({ title: "请填写提交信息", hint: "" });
      messageRef.current?.focus();
      return;
    }
    setBusy(push ? "commit-push" : "commit");
    setError(null);
    let committed: Awaited<ReturnType<typeof commitThreadGit>> | null = null;
    try {
      committed = await commitThreadGit(threadId, { message, amend });
    } catch (err) {
      setError(errorText(err));
      setBusy(null);
      return;
    }
    setMessage("");
    onOpenChange(false);
    const label = `${committed.short_sha} ${committed.summary}`;
    if (!push) {
      toast({ title: amend ? "已修订提交" : "已提交", description: label, tone: "success" });
      setBusy(null);
      return;
    }
    try {
      const pushed = await pushThreadGit(threadId);
      toast({
        title: "已提交并推送",
        description: `${label} → ${pushed.upstream || pushed.remote || "远端"}`,
        tone: "success",
      });
    } catch (err) {
      toastError(`已提交 ${committed.short_sha}，但推送失败`, err);
    } finally {
      setBusy(null);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!busy) onOpenChange(next); }}
      icon="gitCommit"
      title="提交改动"
      description={
        status?.current_branch
          ? `提交到分支 ${status.current_branch}，将暂存全部 ${files} 个改动文件。`
          : `将暂存全部 ${files} 个改动文件。`
      }
      initialFocusRef={messageRef}
      dismissable={!busy}
      testId="thread-git-commit-dialog"
      footer={(
        <>
          <Button variant="ghost" disabled={Boolean(busy)} onClick={() => onOpenChange(false)}>取消</Button>
          <Button
            variant={canPush ? "secondary" : "primary"}
            loading={busy === "commit"}
            disabled={Boolean(busy)}
            shortcut="mod+enter"
            tooltip="提交"
            onClick={() => void run(false)}
          >
            提交
          </Button>
          {canPush ? (
            <Button variant="primary" icon="upload" loading={busy === "commit-push"} disabled={Boolean(busy)} onClick={() => void run(true)}>
              提交并推送
            </Button>
          ) : null}
        </>
      )}
    >
      <div className="flex flex-col gap-3">
        <TextArea
          ref={messageRef}
          label="提交信息"
          value={message}
          rows={4}
          autoResize
          placeholder="简要描述这次改动"
          disabled={Boolean(busy)}
          onChange={(event) => setMessage(event.target.value)}
          onKeyDown={(event) => submitShortcut(event, () => void run(false))}
          description="⌘/Ctrl + Enter 提交"
        />
        <Checkbox
          checked={amend}
          onCheckedChange={setAmend}
          disabled={Boolean(busy)}
          label="修订上一次提交（amend）"
          description={status?.head_subject ? `上一次提交：${status.head_subject}` : undefined}
        />
        {error ? (
          <Callout tone="danger" role="alert" title="提交失败">
            <span className="whitespace-pre-wrap break-words font-cx-mono text-[12px]">{error.title}</span>
            {error.hint ? <span className="mt-1 block text-[12px] text-cx-fg-3">{error.hint}</span> : null}
          </Callout>
        ) : null}
      </div>
    </Dialog>
  );
}

export function CreatePullRequestDialog({
  open,
  onOpenChange,
  threadId,
  status,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  threadId: string;
  status: ProjectGitStatus | null;
}) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [base, setBase] = useState("");
  const [draft, setDraft] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ title: string; hint: string } | null>(null);
  const titleRef = useRef<HTMLInputElement | null>(null);
  const needsPush = !status?.upstream || (status?.ahead ?? 0) > 0;

  useEffect(() => {
    if (!open) return;
    setTitle(status?.head_subject || status?.current_branch || "");
    setError(null);
    // Prefill only when the dialog opens; later status refreshes must not clobber edits.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const submit = async () => {
    if (busy) return;
    if (!title.trim()) {
      setError({ title: "请填写 PR 标题", hint: "" });
      titleRef.current?.focus();
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const created = await createThreadPullRequest(threadId, {
        title: title.trim(),
        body,
        base: base.trim(),
        draft,
        pushFirst: needsPush,
      });
      onOpenChange(false);
      setBody("");
      chatPanel.open(threadId, "pull-request");
      toast({
        title: `已创建 PR #${created.number}`,
        description: `${created.head} → ${created.base}${created.pushed ? "（已先推送分支）" : ""}`,
        tone: "success",
        duration: 8000,
        action: created.url ? { label: "在 GitHub 打开", onClick: () => window.open(created.url, "_blank", "noopener,noreferrer") } : undefined,
      });
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => { if (!busy) onOpenChange(next); }}
      icon="gitPullRequest"
      size="lg"
      title="创建 Pull request"
      description={status?.current_branch ? `从分支 ${status.current_branch} 创建。` : undefined}
      initialFocusRef={titleRef}
      dismissable={!busy}
      testId="thread-git-pr-dialog"
      footer={(
        <>
          <Button variant="ghost" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" icon="gitPullRequest" loading={busy} shortcut="mod+enter" tooltip="创建 PR" onClick={() => void submit()}>
            {needsPush ? "推送并创建 PR" : "创建 PR"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-3" onKeyDown={(event) => submitShortcut(event, () => void submit())}>
        <div>
          <Label htmlFor="thread-pr-title">标题</Label>
          <Input id="thread-pr-title" ref={titleRef} value={title} disabled={busy} onChange={(event) => setTitle(event.target.value)} />
        </div>
        <TextArea label="描述" value={body} rows={6} disabled={busy} placeholder="可选，支持 Markdown" onChange={(event) => setBody(event.target.value)} />
        <div>
          <Label htmlFor="thread-pr-base" hint="留空使用仓库默认分支">目标分支</Label>
          <Input id="thread-pr-base" value={base} disabled={busy} placeholder="默认分支" onChange={(event) => setBase(event.target.value)} />
        </div>
        <Checkbox checked={draft} onCheckedChange={setDraft} disabled={busy} label="创建为草稿" />
        {needsPush ? (
          <Callout tone="neutral" title="分支尚未完全推送">
            {status?.upstream ? `有 ${status.ahead ?? 0} 个提交未推送到 ${status.upstream}` : "分支还没有上游"}，创建前会先推送。
          </Callout>
        ) : null}
        {error ? (
          <Callout tone="danger" role="alert" title="创建失败">
            <span className="whitespace-pre-wrap break-words font-cx-mono text-[12px]">{error.title}</span>
            {error.hint ? <span className="mt-1 block text-[12px] text-cx-fg-3">{error.hint}</span> : null}
          </Callout>
        ) : null}
      </div>
    </Dialog>
  );
}

function syncHint(status: ProjectGitStatus): string {
  const parts: string[] = [];
  if (status.dirty) parts.push(`${status.changed_file_count ?? 0} 个文件未提交`);
  if (!status.upstream) parts.push((status.remotes?.length ?? 0) ? "未设置上游" : "没有远端");
  else {
    if (status.ahead) parts.push(`领先 ${status.ahead}`);
    if (status.behind) parts.push(`落后 ${status.behind}`);
    if (!status.ahead && !status.behind) parts.push(`与 ${status.upstream} 同步`);
  }
  return parts.join(" · ");
}

/**
 * Header Git action: a primary button chosen from the live status (commit →
 * pull → push → PR) plus a ⋯ menu with every operation.
 */
export function ThreadGitActions({
  view,
  className,
  variant = "header",
}: {
  view: ConversationView;
  className?: string;
  /** "details": a full-width row for the thread details card. */
  variant?: "header" | "details";
}): ReactNode {
  const threadId = view.thread.thread_id;
  const projectId = view.thread.project_id || view.workspace?.project_id || undefined;
  const hasWorkspace = Boolean(view.workspace?.root_path);
  const git = useSharedGitStatus(threadId, projectId);
  const status = git.status;
  const isRepo = Boolean(hasWorkspace && status?.is_repo);
  const prs = useThreadPullRequests(threadId, isRepo);
  const editor = useOpenThreadInEditor(view);
  const [busy, setBusy] = useState<GitBusy>(null);
  const [commitOpen, setCommitOpen] = useState(false);
  const [prOpen, setPrOpen] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

  if (!isRepo || !status) return null;

  const listing = prs.result;
  const github = isGithubListing(listing);
  const openPr = openPrFrom(listing);
  const primary = primaryAction(status, listing);
  const onBranch = Boolean(status.current_branch) && !status.detached_head;
  const hasRemote = (status.remotes?.length ?? 0) > 0;

  const push = async () => {
    if (busy) return;
    setBusy("push");
    try {
      const result = await pushThreadGit(threadId);
      toast({
        title: result.set_upstream ? `已推送并设置上游 ${result.upstream || result.remote || ""}`.trim() : "已推送",
        description: result.branch,
        tone: "success",
      });
    } catch (err) {
      toastError("推送失败", err);
    } finally {
      setBusy(null);
    }
  };

  const pull = async (rebase = false) => {
    if (busy) return;
    setBusy("pull");
    try {
      const result = await pullThreadGit(threadId, { rebase });
      toast({ title: rebase ? "已拉取（rebase）" : "已拉取", description: result.output || result.upstream || undefined, tone: "success" });
    } catch (err) {
      toastError("拉取失败", err);
    } finally {
      setBusy(null);
    }
  };

  const viewPr = () => chatPanel.open(threadId, "pull-request");

  const runPrimary = () => {
    if (primary === "commit") setCommitOpen(true);
    else if (primary === "push") void push();
    else if (primary === "pull") void pull(false);
    else if (primary === "create-pr") setPrOpen(true);
    else if (primary === "view-pr") viewPr();
  };

  const primaryBusy = (primary === "push" && busy === "push") || (primary === "pull" && busy === "pull");
  const hint = syncHint(status);
  const primaryCopy = primary ? PRIMARY_COPY[primary] : null;
  const primaryLabel = primary === "push" && status.ahead ? `推送 ${status.ahead}` : primary === "pull" && status.behind ? `拉取 ${status.behind}` : primaryCopy?.label;

  const menu = (
    <Menu
      open={menuOpen}
      onOpenChange={setMenuOpen}
      placement="bottom-end"
      ariaLabel="Git 操作"
      className="min-w-[230px]"
      trigger={(
        <IconButton
          icon="gitBranch"
          loading={Boolean(busy) && !primaryBusy}
          label={hint ? `Git 操作 · ${hint}` : "Git 操作"}
          data-testid="thread-git-menu"
        />
      )}
    >
      {hint ? <div className="px-2.5 pb-1 pt-1.5 text-[11.5px] text-cx-fg-4">{hint}</div> : null}
      <MenuItem icon="gitCommit" disabled={!status.dirty || !onBranch || Boolean(busy)} onSelect={() => setCommitOpen(true)}>
        提交…
      </MenuItem>
      <MenuItem
        icon="upload"
        hint={status.ahead ? String(status.ahead) : undefined}
        disabled={!onBranch || !hasRemote || Boolean(busy)}
        onSelect={() => void push()}
      >
        推送
      </MenuItem>
      <MenuItem
        icon="download"
        hint={status.behind ? String(status.behind) : undefined}
        disabled={!onBranch || !status.upstream || Boolean(busy)}
        onSelect={() => void pull(false)}
      >
        拉取
      </MenuItem>
      <MenuItem icon="download" disabled={!onBranch || !status.upstream || Boolean(busy)} onSelect={() => void pull(true)}>
        拉取（rebase）
      </MenuItem>
      <MenuSeparator />
      {openPr ? (
        <MenuItem icon="gitPullRequest" hint={`#${openPr.number}`} onSelect={viewPr}>查看 PR</MenuItem>
      ) : null}
      <MenuItem
        icon="gitPullRequest"
        disabled={!github || !onBranch || Boolean(openPr) || Boolean(busy)}
        description={!listing ? undefined : !github ? "远端不是 GitHub" : openPr ? "当前分支已有开放的 PR" : undefined}
        onSelect={() => setPrOpen(true)}
      >
        创建 PR…
      </MenuItem>
      <MenuSeparator />
      {status.current_branch ? (
        <MenuItem icon="copy" onSelect={() => copyText(status.current_branch || "", "分支名")}>复制分支名</MenuItem>
      ) : null}
      <OpenInEditorMenuItems editor={editor} />
      <MenuItem
        icon="refresh"
        onSelect={() => {
          void git.refresh();
          void prs.refresh();
        }}
      >
        刷新 Git 状态
      </MenuItem>
    </Menu>
  );
  const dialogs = (
    <>
      <CommitDialog open={commitOpen} onOpenChange={setCommitOpen} threadId={threadId} status={status} />
      <CreatePullRequestDialog open={prOpen} onOpenChange={setPrOpen} threadId={threadId} status={status} />
    </>
  );
  if (variant === "details") {
    return (
      <div className={cn("flex items-center gap-1.5 px-2 py-1", className)} data-testid="thread-details-git">
        {primaryCopy ? (
          <Button
            size="sm"
            variant="secondary"
            icon={primaryCopy.icon}
            loading={primaryBusy}
            disabled={Boolean(busy)}
            tooltip={hint || undefined}
            onClick={runPrimary}
            className="min-w-0 flex-1 justify-center"
            data-testid="thread-details-git-primary"
          >
            {primaryLabel}
          </Button>
        ) : (
          <span className="min-w-0 flex-1 truncate px-1 text-[12px] text-cx-fg-4">{hint || (status.detached_head ? "分离 HEAD" : !hasRemote ? "未配置远端" : "已与远端同步")}</span>
        )}
        {menu}
        {dialogs}
      </div>
    );
  }

  return (
    <div className={cn("flex flex-none items-center", className)} data-testid="thread-git-actions">
      {primaryCopy ? (
        <Button
          size="sm"
          variant="ghost"
          icon={primaryCopy.icon}
          loading={primaryBusy}
          disabled={Boolean(busy)}
          tooltip={hint || undefined}
          onClick={runPrimary}
          className="hidden text-cx-fg-2 @[620px]/header:inline-flex"
          data-testid="thread-git-primary"
        >
          {primaryLabel}
        </Button>
      ) : null}
      {menu}
      {dialogs}
    </div>
  );
}
