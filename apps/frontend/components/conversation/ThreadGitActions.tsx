"use client";

import { useEffect, useRef, useState, type ReactNode } from "react";
import type { ConversationView, ProjectGitStatus } from "@/lib/useConversation";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import {
  ThreadGitError,
  checkoutThreadGit,
  commitThreadGit,
  initThreadGit,
  mergeWorktreeThreadGit,
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
import { fetchWorktreeDiff, type DiffFileMeta } from "@/lib/conversationDiff";
import { controlErrorText, threadPullRequestAction } from "@/lib/controlGrants";
import { readChatPreferences } from "@/lib/chatPreferences";
import { type IconName } from "@muteki/ui";
import {
  Button,
  Callout,
  Checkbox,
  Dialog,
  IconButton,
  Input,
  Menu,
  MenuItem,
  MenuSeparator,
  TextArea,
  TextField,
  toast,
} from "@muteki/ui";

type GitBusy = "commit" | "push" | "pull" | "pr" | "merge" | "init" | null;
type PrimaryKind = "commit" | "push" | "pull" | "create-pr" | "view-pr";

function errorText(error: unknown): { title: string; hint: string } {
  if (error instanceof ThreadGitError) return { title: error.message, hint: error.recoveryHint };
  if (error instanceof Error) return { title: error.message, hint: "" };
  return { title: String(error), hint: "" };
}

function toastError(title: string, error: unknown, retry?: () => void) {
  const { title: message, hint } = errorText(error);
  toast({
    title,
    description: (
      <span className="whitespace-pre-wrap break-words font-cx-mono text-cx-xs">
        {message}
        {hint ? <span className="mt-1 block font-sans text-cx-xs text-cx-fg-3">{hint}</span> : null}
      </span>
    ),
    tone: "danger",
    duration: 9000,
    action: retry ? { label: "重试", onClick: retry } : undefined,
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
        description: <span className="break-all font-cx-mono text-cx-xs">{result.path}</span>,
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
  const [changedFiles, setChangedFiles] = useState<DiffFileMeta[]>([]);
  const [excluded, setExcluded] = useState<ReadonlySet<string>>(() => new Set());
  const [filesNote, setFilesNote] = useState("");
  const [featureBranch, setFeatureBranch] = useState("");
  const messageRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setAmend(false);
    setExcluded(new Set());
    setFilesNote("");
    setFeatureBranch("");
    let cancelled = false;
    fetchWorktreeDiff(threadId)
      .then((data) => { if (!cancelled) setChangedFiles((data.files || []).filter((file) => file.path)); })
      .catch(() => { if (!cancelled) setFilesNote("无法读取改动文件列表，提交将包含全部改动。"); });
    return () => { cancelled = true; };
  }, [open, threadId]);

  const files = status?.changed_file_count ?? 0;
  const canPush = (status?.remotes?.length ?? 0) > 0;
  const empty = !message.trim();
  const defaultBranch = ["main", "master", "trunk", "develop"].find((name) => status?.branches?.includes(name)) || null;
  const onDefaultBranch = Boolean(status?.current_branch && status.current_branch === defaultBranch);
  const includedPaths = changedFiles.filter((file) => !excluded.has(file.path)).map((file) => file.path);

  const run = async (push: boolean, createBranch = false) => {
    if (busy) return;
    if (empty) {
      setError({ title: "请填写提交信息", hint: "" });
      messageRef.current?.focus();
      return;
    }
    if (changedFiles.length && !includedPaths.length) {
      setError({ title: "请至少包含一个改动文件", hint: "" });
      return;
    }
    const paths = changedFiles.length && excluded.size ? includedPaths : undefined;
    setBusy(push ? "commit-push" : "commit");
    setError(null);
    let committed: Awaited<ReturnType<typeof commitThreadGit>> | null = null;
    try {
      if (createBranch) {
        const name = featureBranch.trim();
        if (!name) {
          setError({ title: "请填写特性分支名", hint: "" });
          setBusy(null);
          return;
        }
        await checkoutThreadGit(threadId, { branch: name, create: true });
      }
      committed = await commitThreadGit(threadId, { message, amend, paths });
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
            {changedFiles.length && excluded.size ? `提交 ${includedPaths.length} 个文件` : "提交"}
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
        {changedFiles.length ? (
          <div className="flex flex-col gap-1.5">
            <span className="text-cx-xs text-cx-fg-3">已更改文件（{includedPaths.length}/{changedFiles.length}）</span>
            <div className="cx-scroll max-h-44 overflow-y-auto rounded-lg border border-cx-border-subtle">
              {changedFiles.map((file) => (
                <div key={`${file.staging}:${file.path}`} className="px-2 py-0.5 hover:bg-cx-hover">
                  <Checkbox
                    checked={!excluded.has(file.path)}
                    disabled={Boolean(busy)}
                    onCheckedChange={(on) => setExcluded((prev) => {
                      const next = new Set(prev);
                      if (on) next.delete(file.path);
                      else next.add(file.path);
                      return next;
                    })}
                    label={<span className="truncate font-cx-mono text-cx-xs" title={file.path}>{file.path}</span>}
                  />
                </div>
              ))}
            </div>
          </div>
        ) : filesNote ? (
          <span className="text-cx-xs text-cx-fg-4">{filesNote}</span>
        ) : null}
        {onDefaultBranch ? (
          <Callout tone="warning" title="你正处于默认分支">
            <span className="block text-cx-xs">在此提交会直接写入默认分支。你可以继续在该分支提交，或创建一个特性分支并在其上提交。</span>
            <span className="mt-2 flex items-center gap-2">
              <Input
                value={featureBranch}
                disabled={Boolean(busy)}
                placeholder="feature/新分支名"
                aria-label="特性分支名"
                className="h-8 flex-1"
                onChange={(event) => setFeatureBranch(event.target.value)}
              />
              <Button size="sm" variant="secondary" disabled={Boolean(busy) || !featureBranch.trim()} onClick={() => void run(false, true)}>
                创建特性分支并提交
              </Button>
            </span>
          </Callout>
        ) : null}
        {error ? (
          <Callout tone="danger" role="alert" title="提交失败">
            <span className="whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{error.title}</span>
            {error.hint ? <span className="mt-1 block text-cx-xs text-cx-fg-3">{error.hint}</span> : null}
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
      let linkFailure = "";
      if (created.url) {
        try {
          await threadPullRequestAction(threadId, "link", { reference: created.url });
        } catch (cause) {
          linkFailure = controlErrorText(cause, "关联失败");
        }
      }
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
      if (linkFailure) toast({ title: "PR 已创建，但没有关联到这个对话", description: linkFailure, tone: "warning", duration: 8000 });
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
      description={status?.current_branch ? `从分支 ${status.current_branch} 创建` : undefined}
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
        <TextField label="标题" ref={titleRef} value={title} disabled={busy} onChange={(event) => setTitle(event.target.value)} />
        <TextArea label="描述" value={body} rows={6} disabled={busy} placeholder="可选，支持 Markdown" onChange={(event) => setBody(event.target.value)} />
        <TextField label="目标分支" labelHint="留空使用仓库默认分支" value={base} disabled={busy} placeholder="例如：main" onChange={(event) => setBase(event.target.value)} />
        <Checkbox checked={draft} onCheckedChange={setDraft} disabled={busy} label="创建为草稿" />
        {needsPush ? (
          <Callout tone="neutral" title="分支尚未完全推送">
            {status?.upstream ? `有 ${status.ahead ?? 0} 个提交未推送到 ${status.upstream}` : "分支还没有上游"}，创建前会先推送。
          </Callout>
        ) : null}
        {error ? (
          <Callout tone="danger" role="alert" title="创建失败">
            <span className="whitespace-pre-wrap break-words font-cx-mono text-cx-xs">{error.title}</span>
            {error.hint ? <span className="mt-1 block text-cx-xs text-cx-fg-3">{error.hint}</span> : null}
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

export function ThreadGitActions({
  view,
  className,
  variant = "header",
}: {
  view: ConversationView;
  className?: string;
  variant?: "header" | "details";
}): ReactNode {
  const threadId = view.thread.thread_id;
  const projectId = view.thread.project_id || view.workspace?.project_id || undefined;
  const hasWorkspace = Boolean(view.workspace?.root_path);
  const git = useSharedGitStatus(threadId, projectId);
  const status = git.status;
  const readOnly = status?.read_only === true;
  const supportsPullRequests = status?.pull_requests_supported !== false;
  const isRepo = Boolean(hasWorkspace && status?.is_repo);
  const prs = useThreadPullRequests(threadId, isRepo && !readOnly && supportsPullRequests);
  const editor = useOpenThreadInEditor(view);
  const [busy, setBusy] = useState<GitBusy>(null);
  const [commitOpen, setCommitOpen] = useState(false);
  const [prOpen, setPrOpen] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);

  const initGit = async () => {
    if (busy) return;
    setBusy("init");
    try {
      await initThreadGit(threadId);
      toast({ title: "已初始化 Git 仓库", tone: "success" });
    } catch (err) {
      toastError("初始化 Git 失败", err, () => void initGit());
    } finally {
      setBusy(null);
    }
  };

  if (hasWorkspace && status && !status.is_repo) {
    if (readOnly) return null;
    return (
      <div className={cn("flex flex-none items-center", variant === "details" && "px-2 py-1", className)} data-testid="thread-git-actions">
        <Button
          size="sm"
          variant={variant === "details" ? "secondary" : "ghost"}
          icon="gitBranch"
          loading={busy === "init"}
          disabled={Boolean(busy)}
          className={variant === "details" ? "min-w-0 flex-1 justify-center" : undefined}
          onClick={() => void initGit()}
        >
          初始化 Git
        </Button>
      </div>
    );
  }
  if (!isRepo || !status) return null;

  const listing = prs.result;
  const github = supportsPullRequests && isGithubListing(listing);
  const openPr = openPrFrom(listing);
  const primary = readOnly ? null : primaryAction(status, listing);
  const onBranch = Boolean(status.current_branch) && !status.detached_head;
  const hasRemote = (status.remotes?.length ?? 0) > 0;

  const push = async () => {
    if (busy) return;
    setBusy("push");
    try {
      const result = await pushThreadGit(threadId);
      toast({
        title: result.set_upstream ? "已发布分支" : "已推送",
        description: result.set_upstream ? `${result.branch} → ${result.upstream || result.remote || ""}`.trim() : result.branch,
        tone: "success",
      });
    } catch (err) {
      toastError("推送失败", err, () => void push());
    } finally {
      setBusy(null);
    }
  };

  const mergeWorktree = async () => {
    if (busy) return;
    setBusy("merge");
    try {
      const result = await mergeWorktreeThreadGit(threadId);
      toast({
        title: result.outcome === "fast_forward"
          ? "已通过快进将 worktree 分支合并回项目目录"
          : result.outcome === "merged"
            ? "已通过合并提交将 worktree 分支合并回项目目录"
            : "已将 worktree 分支的更改以 squash 方式暂存到项目目录，尚未提交",
        description: result.source_branch || undefined,
        tone: "success",
        duration: 6000,
      });
    } catch (err) {
      toastError("合并 worktree 失败", err, () => void mergeWorktree());
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
      toastError("拉取失败", err, () => void pull(rebase));
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
  const hint = [readOnly ? "当前仅支持读取 Git 状态和差异" : "", syncHint(status)].filter(Boolean).join(" · ");
  const primaryCopy = primary ? PRIMARY_COPY[primary] : null;
  const publishing = !status.upstream && hasRemote;
  const primaryLabel = primary === "push" && publishing ? "发布分支" : primary === "push" && status.ahead ? `推送 ${status.ahead}` : primary === "pull" && status.behind ? `拉取 ${status.behind}` : primaryCopy?.label;
  const isWorktreeThread = view.lineage?.isolation === "worktree";

  const menu = (
    <Menu
      open={menuOpen}
      onOpenChange={setMenuOpen}
      placement="bottom-end"
      ariaLabel="Git 操作"
      className="min-w-[230px]"
      trigger={(
        <IconButton
          size="md"
          icon="gitBranch"
          loading={Boolean(busy) && !primaryBusy}
          label={hint ? `Git 操作 · ${hint}` : "Git 操作"}
          data-testid="thread-git-menu"
        />
      )}
    >
      {hint ? <div className="px-2.5 pb-1 pt-1.5 text-cx-xs text-cx-fg-4">{hint}</div> : null}
      <MenuItem icon="gitCommit" disabled={readOnly || !status.dirty || !onBranch || Boolean(busy)} onSelect={() => setCommitOpen(true)}>
        提交…
      </MenuItem>
      <MenuItem
        icon="upload"
        hint={status.ahead ? String(status.ahead) : undefined}
        disabled={readOnly || !onBranch || !hasRemote || Boolean(busy)}
        onSelect={() => void push()}
      >
        {status.upstream ? "推送" : "发布分支"}
      </MenuItem>
      <MenuItem
        icon="download"
        hint={status.behind ? String(status.behind) : undefined}
        disabled={readOnly || !onBranch || !status.upstream || Boolean(busy)}
        onSelect={() => void pull(false)}
      >
        拉取
      </MenuItem>
      <MenuItem icon="download" disabled={readOnly || !onBranch || !status.upstream || Boolean(busy)} onSelect={() => void pull(true)}>
        拉取（rebase）
      </MenuItem>
      {isWorktreeThread ? (
        <MenuItem
          icon="gitMerge"
          disabled={readOnly || Boolean(busy)}
          description="将此对话 worktree 分支的更改合并回项目目录"
          onSelect={() => void mergeWorktree()}
        >
          合并回项目目录
        </MenuItem>
      ) : null}
      <MenuSeparator />
      {openPr ? (
        <MenuItem icon="gitPullRequest" hint={`#${openPr.number}`} onSelect={viewPr}>查看 PR</MenuItem>
      ) : null}
      <MenuItem
        icon="gitPullRequest"
        disabled={readOnly || !github || !onBranch || Boolean(openPr) || Boolean(busy)}
        description={!supportsPullRequests ? "当前服务尚未接入 Pull request" : !listing ? undefined : !github ? "远端不是 GitHub" : openPr ? "当前分支已有开放的 PR" : undefined}
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
          if (!readOnly && supportsPullRequests) void prs.refresh();
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
          <span className="min-w-0 flex-1 truncate px-1 text-cx-xs text-cx-fg-4">{hint || (status.detached_head ? "分离 HEAD" : !hasRemote ? "未配置远端" : "已与远端同步")}</span>
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
