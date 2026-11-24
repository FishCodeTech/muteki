"use client";

import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import { Icon, type IconName } from "@muteki/ui";
import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import { CreatePullRequestDialog } from "@/components/conversation/ThreadGitActions";
import {
  Badge,
  Button,
  Callout,
  DiffStat,
  EmptyState,
  IconButton,
  Input,
  Menu,
  MenuItem,
  MenuSeparator,
  ScrollArea,
  Skeleton,
  Spinner,
  Switch,
  type Tone,
} from "@muteki/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import {
  controlErrorCodeText,
  controlErrorText,
  fetchThreadPullRequestsWithState,
  pullRequestLabel,
  setPullRequestWatchesPaused,
  threadPullRequestAction,
  type PullRequestLinkOp,
  type PullRequestSnapshot,
  type PullRequestSyncError,
  type ThreadPullRequestLink,
} from "@/lib/controlGrants";
import { PullRequestDetailView } from "./PullRequestDetailView";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { useThreadPullRequests, type ThreadPullRequestListing, type ThreadPullRequestRecord } from "@/lib/threadGitActions";
import { SectionHeader, SurfaceToolbar, relativeTime, useNow } from "./shared";

type Chip = { label: string; tone: Tone; icon?: IconName };

const CHECKS: Record<string, Chip> = {
  passing: { label: "检查通过", tone: "success", icon: "checkCircle" },
  failing: { label: "检查失败", tone: "danger", icon: "circleAlert" },
  pending: { label: "检查进行中", tone: "running", icon: "clock" },
};
const REVIEWS: Record<string, Chip> = {
  approved: { label: "已批准", tone: "success" },
  changes_requested: { label: "需要修改", tone: "warning" },
  required: { label: "等待审阅", tone: "neutral" },
};
const MERGEABILITY: Record<string, Chip> = {
  clean: { label: "可以合并", tone: "success" },
  conflicting: { label: "有合并冲突", tone: "danger" },
  pending: { label: "正在计算能否合并", tone: "neutral" },
};
const LISTED_STATE: Record<string, Chip> = {
  open: { label: "开放", tone: "success" },
  draft: { label: "草稿", tone: "neutral" },
  merged: { label: "已合并", tone: "accent" },
  closed: { label: "已关闭", tone: "danger" },
};
const SETTINGS_HREF = "/settings/source-control";
const SETTINGS_FAILURES = new Set(["pull_request_no_credential", "pull_request_host_disabled", "pull_request_unknown_host"]);

type ReadFailure = { code: string; text: string };

function stateChip(snapshot: PullRequestSnapshot): Chip {
  if (snapshot.state === "open" && snapshot.is_draft) return LISTED_STATE.draft;
  return LISTED_STATE[snapshot.state] ?? { label: snapshot.state, tone: "neutral" };
}

function syncErrorText(error: PullRequestSyncError, detail: string): string {
  switch (error.reason) {
    case "no_credential":
      return "没有可用的访问令牌，或令牌已失效。";
    case "host_disabled":
      return "这个代码平台已在设置中停用。";
    case "not_found":
      return "代码平台上找不到这个拉取请求，或当前令牌无权访问。";
    case "rate_limited": {
      const at = new Date(error.retry_at * 1000);
      const time = Number.isFinite(at.getTime()) ? at.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) : "";
      return time ? `代码平台暂时限制了请求频率，${time} 之后可以再试。` : "代码平台暂时限制了请求频率，请稍后再试。";
    }
    default:
      return detail || "上次从代码平台读取状态失败。";
  }
}

function isBranchPull(record: ThreadPullRequestRecord, listing: ThreadPullRequestListing, links: ThreadPullRequestLink[]): boolean {
  const repository = `${listing.owner}/${listing.repo}`.toLowerCase();
  const url = record.html_url.toLowerCase();
  return !links.some((link) => link.url.toLowerCase() === url || (link.key.repository === repository && link.key.number === record.number));
}

function StatusChips({ snapshot }: { snapshot: PullRequestSnapshot }) {
  const chips: Chip[] = [];
  if (snapshot.checks_state && CHECKS[snapshot.checks_state]) chips.push(CHECKS[snapshot.checks_state]);
  if (snapshot.review_decision && REVIEWS[snapshot.review_decision]) chips.push(REVIEWS[snapshot.review_decision]);
  if (snapshot.state === "open" && MERGEABILITY[snapshot.mergeability]) chips.push(MERGEABILITY[snapshot.mergeability]);
  if (!chips.length) return null;

  return (
    <div className="flex flex-wrap gap-1.5">
      {chips.map((chip) => (
        <Badge key={chip.label} tone={chip.tone} icon={chip.icon} dot={!chip.icon}>
          {chip.label}
        </Badge>
      ))}
    </div>
  );
}

function LinkedPullRequest({
  link,
  busy,
  refreshing,
  failure,
  now,
  onAction,
  onOpenDetail,
}: {
  link: ThreadPullRequestLink;
  busy: string;
  refreshing: boolean;
  failure?: ReadFailure;
  now: number;
  onAction: (op: PullRequestLinkOp, link: ThreadPullRequestLink) => void;
  onOpenDetail: (link: ThreadPullRequestLink) => void;
}) {
  const label = pullRequestLabel(link.key);
  const snapshot = link.snapshot;
  const state = snapshot ? stateChip(snapshot) : null;
  const working = busy !== "";
  const needsSettings = link.sync_error?.reason === "no_credential" || link.sync_error?.reason === "host_disabled"
    || Boolean(link.sync_error && failure && SETTINGS_FAILURES.has(failure.code));
  const synced = snapshot ? relativeTime(snapshot.synced_at * 1000, now) : "";

  return (
    <li className="flex flex-col gap-2.5 rounded-xl border border-cx-border-subtle bg-cx-elevated p-3" data-pull-request={label}>
      <div className="flex min-w-0 items-start gap-2">
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1.5">
            {state ? <Badge tone={state.tone} dot>{state.label}</Badge> : null}
            <span className="cx-tabular shrink-0 font-cx-mono text-cx-xs text-cx-fg-3">#{link.key.number}</span>
            <span className="min-w-0 truncate text-cx-xs text-cx-fg-4" title={`${link.key.host}/${link.key.repository}`}>
              {link.key.repository}
            </span>
          </div>
          {snapshot?.title ? (
            <a
              href={link.url}
              target="_blank"
              rel="noopener noreferrer"
              className="mt-1 block break-words text-cx-sm font-medium leading-5 text-cx-fg hover:text-cx-accent hover:underline"
            >
              {snapshot.title}
            </a>
          ) : (
            <a
              href={link.url}
              target="_blank"
              rel="noopener noreferrer"
              className="mt-1 block break-all font-cx-mono text-cx-xs leading-5 text-cx-fg-3 hover:text-cx-accent hover:underline"
            >
              {link.url.replace(/^https?:\/\//, "")}
            </a>
          )}
        </div>
        <Button size="xs" variant="ghost" onClick={() => onOpenDetail(link)}>详情</Button>
        <Menu
          trigger={<IconButton size="sm" icon="more" label={`#${link.key.number} 更多操作`} disabled={working} />}
          placement="bottom-end"
          ariaLabel={`#${link.key.number} 操作`}
        >
          <MenuItem icon="externalLink" onSelect={() => window.open(link.url, "_blank", "noopener,noreferrer")}>在代码平台打开</MenuItem>
          <MenuItem icon="refresh" onSelect={() => onAction("refresh", link)}>重新读取状态</MenuItem>
          <MenuSeparator />
          <MenuItem icon="x" danger onSelect={() => onAction("unlink", link)}>取消关联</MenuItem>
        </Menu>
      </div>

      {snapshot ? (
        <div className="flex flex-wrap items-center gap-x-1.5 gap-y-1 text-cx-xs text-cx-fg-4">
          {[
            snapshot.head_branch ? <span key="branch" className="min-w-0 break-all font-cx-mono">{snapshot.head_branch} → {snapshot.base_branch}</span> : null,
            snapshot.author?.login ? <span key="author">{snapshot.author.login}</span> : null,
            typeof snapshot.additions === "number" && typeof snapshot.deletions === "number"
              ? <DiffStat key="diff" additions={snapshot.additions} deletions={snapshot.deletions} />
              : null,
            typeof snapshot.changed_files === "number" ? <span key="files">{snapshot.changed_files} 个文件</span> : null,
          ].filter(Boolean).flatMap((item, index) => (index ? [<span key={`dot-${index}`} aria-hidden>·</span>, item] : [item]))}
        </div>
      ) : null}

      {snapshot ? <StatusChips snapshot={snapshot} /> : null}

      {!snapshot && !link.sync_error ? (
        <div className="flex items-center gap-2 text-cx-xs text-cx-fg-3">
          <Spinner size={12} />
          正在从代码平台读取状态…
        </div>
      ) : null}

      {link.sync_error ? (
        <Callout
          tone={link.sync_error.reason === "rate_limited" ? "warning" : "danger"}
          role="status"
          className="px-2.5 py-2 text-cx-xs"
        >
          <span className="block">{syncErrorText(link.sync_error, failure?.text ?? "")}</span>
          {needsSettings ? (
            <ConversationRouteLink href={SETTINGS_HREF} className="mt-0.5 inline-flex items-center gap-1 font-medium text-cx-accent hover:underline">
              前往 设置 → 代码平台
              <Icon name="arrowUpRight" size={12} />
            </ConversationRouteLink>
          ) : null}
        </Callout>
      ) : null}

      <div className="flex flex-wrap items-center justify-between gap-2 border-t border-cx-border-subtle pt-2.5">
        <Switch
          size="sm"
          checked={Boolean(link.watch)}
          disabled={working}
          onCheckedChange={(on) => onAction(on ? "watch" : "unwatch", link)}
          label={<span className="text-cx-xs text-cx-fg-2">监控状态变化</span>}
          ariaLabel={`监控 ${link.key.repository} #${link.key.number}`}
        />
        <span className="flex items-center gap-1.5 text-cx-xs text-cx-fg-4">
          {refreshing ? <><Spinner size={11} />正在读取</> : synced ? `${synced}同步` : null}
          <IconButton
            size="xs"
            icon="refresh"
            label="重新读取状态"
            disabled={working || refreshing}
            onClick={() => onAction("refresh", link)}
          />
        </span>
      </div>
    </li>
  );
}

function BranchPullRequest({
  record,
  busy,
  onLink,
}: {
  record: ThreadPullRequestRecord;
  busy: boolean;
  onLink: (url: string) => void;
}) {
  const state = LISTED_STATE[record.state] ?? { label: record.state, tone: "neutral" as Tone };

  return (
    <li className="flex items-center gap-2 rounded-lg border border-dashed border-cx-border px-2.5 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5">
          <Badge tone={state.tone} dot>{state.label}</Badge>
          <span className="cx-tabular font-cx-mono text-cx-xs text-cx-fg-3">#{record.number}</span>
        </div>
        <a
          href={record.html_url}
          target="_blank"
          rel="noopener noreferrer"
          className="mt-0.5 block truncate text-cx-sm text-cx-fg hover:text-cx-accent hover:underline"
          title={record.title}
        >
          {record.title}
        </a>
      </div>
      <Button size="xs" variant="secondary" icon="link" loading={busy} disabled={busy} onClick={() => onLink(record.html_url)}>
        关联
      </Button>
    </li>
  );
}

export function PullRequestSurface({ threadId, view, events, active, hasWorkspace }: SurfaceProps) {
  const [links, setLinks] = useState<ThreadPullRequestLink[] | null>(null);
  const [watchesPaused, setWatchesPaused] = useState(false);
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [reference, setReference] = useState("");
  const [inputError, setInputError] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState("");
  const [refreshing, setRefreshing] = useState<ReadonlyMap<string, number | undefined>>(() => new Map());
  const [createOpen, setCreateOpen] = useState(false);
  const now = useNow(active);

  const projectId = view.thread.project_id || view.workspace?.project_id || undefined;
  const git = useSharedGitStatus(threadId, projectId);
  const status = git.status;
  const canListBranch = Boolean(hasWorkspace && status?.is_repo && status.read_only !== true && status.pull_requests_supported !== false);
  const branch = useThreadPullRequests(threadId, active && canListBranch);
  const listing = canListBranch ? branch.result : null;

  const load = useCallback(async () => {
    try {
      const state = await fetchThreadPullRequestsWithState(threadId);
      setLinks(state.pull_requests);
      setWatchesPaused(state.pull_request_watches_paused);
    } catch (cause) {
      setError(controlErrorText(cause, "读取拉取请求失败"));
      setLinks((current) => current ?? []);
    }
  }, [threadId]);

  const updates = useMemo(
    () => events.filter((event) => event.event_type === "core.pull_request.updated"),
    [events],
  );
  const revision = events.findLast((event) => event.event_type === "core.pull_request.updated" || event.event_type === "core.control.updated")?.seq;
  useEffect(() => {
    if (active) void load();
  }, [active, load, revision]);

  const failures = useMemo(() => {
    const latest = new Map<string, ReadFailure>();
    for (const event of updates) {
      const key = typeof event.payload.key === "string" ? event.payload.key : "";
      if (!key) continue;
      const failure = event.payload.error as { code?: string; message?: string } | undefined;
      if (failure?.message) {
        const code = failure.code ?? "";
        latest.set(key, { code, text: controlErrorCodeText(code) ?? `从代码平台读取状态失败：${failure.message}` });
      } else latest.delete(key);
    }
    return latest;
  }, [updates]);

  const run = async (op: PullRequestLinkOp, target: { reference: string } | { key: string }, busyKey: string) => {
    setBusy(busyKey);
    setError("");
    try {
      await threadPullRequestAction(threadId, op, target);
      await load();
      return true;
    } catch (cause) {
      setError(controlErrorText(cause, "更新拉取请求失败"));
      return false;
    } finally {
      setBusy("");
    }
  };

  const onLinkedAction = (op: PullRequestLinkOp, link: ThreadPullRequestLink) => {
    const label = pullRequestLabel(link.key);
    const id = link.url || label;
    if (op === "refresh") setRefreshing((current) => new Map(current).set(id, revision));
    void run(op, { key: id }, `${op}:${label}`).then((ok) => {
      if (!ok && op === "refresh") setRefreshing((current) => {
        const next = new Map(current);
        next.delete(id);
        return next;
      });
    });
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const value = reference.trim();
    if (!value) return;
    if (!/^https?:\/\//i.test(value) && !/^#?\d+$/.test(value)) {
      setInputError("请粘贴完整的拉取请求地址，或只填编号（例如 42）。");
      return;
    }
    setInputError("");
    if (await run("link", { reference: value }, "link")) setReference("");
  };

  const linkBranch = (url: string) => {
    void run("link", { reference: url }, `branch:${url}`);
  };

  const branchPulls = listing && links ? listing.pull_requests.filter((record) => isBranchPull(record, listing, links)) : [];
  const onBranch = Boolean(status?.current_branch) && !status?.detached_head;
  const branchHasOpenPull = Boolean(listing?.pull_requests.some((record) => record.state === "open" || record.state === "draft"));
  const canCreate = Boolean(listing && listing.state !== "not_github" && listing.state !== "no_remote" && listing.state !== "no_git" && listing.state !== "no_workspace" && onBranch && !branchHasOpenPull);
  const linkedCount = links?.length ?? 0;
  const refreshAll = () => {
    void load();
    if (canListBranch) void branch.refresh();
  };

  const selectedLink = useMemo(
    () => (selectedKey && links ? links.find((l) => pullRequestLabel(l.key) === selectedKey) ?? null : null),
    [selectedKey, links],
  );
  if (selectedLink) {
    return (
      <PullRequestDetailView
        threadId={threadId}
        link={selectedLink}
        onBack={() => setSelectedKey(null)}
        onUpdated={() => void load()}
      />
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="pull-request-surface">
      <SurfaceToolbar className="gap-1.5 pl-5 pr-3">
        <Icon name="gitPullRequest" size={14} className="shrink-0 text-cx-fg-3" />
        <span className="min-w-0 flex-1 truncate text-cx-xs text-cx-fg-2">
          {linkedCount ? `已关联 ${linkedCount} 个拉取请求` : "拉取请求"}
        </span>
        {canCreate ? (
          <Button size="sm" variant="ghost" icon="plus" onClick={() => setCreateOpen(true)}>创建 PR…</Button>
        ) : null}
        <IconButton icon="refresh" label="刷新" loading={links === null || branch.loading} onClick={refreshAll} />
      </SurfaceToolbar>
      <ScrollArea className="flex-1">
        <div className="flex flex-col gap-4 p-3">
          <form className="flex flex-col gap-1.5" onSubmit={(event) => void submit(event)}>
            <div className="flex gap-2">
              <Input
                className="min-w-0 flex-1"
                size="sm"
                icon="link"
                aria-label="拉取请求地址或编号"
                placeholder="粘贴拉取请求地址或填写编号"
                value={reference}
                invalid={Boolean(inputError)}
                disabled={busy === "link"}
                onChange={(event) => {
                  setReference(event.target.value);
                  if (inputError) setInputError("");
                }}
              />
              <Button type="submit" size="sm" variant="secondary" loading={busy === "link"} disabled={busy !== "" || !reference.trim()}>
                关联
              </Button>
            </div>
            <p className={inputError ? "text-cx-xs text-cx-danger" : "text-cx-xs leading-4 text-cx-fg-4"} role={inputError ? "alert" : undefined}>
              {inputError || "支持 GitHub、GitLab、Forgejo、Gitea、Bitbucket 和 Azure DevOps。只填编号时，使用当前项目远程仓库所在的平台。"}
            </p>
          </form>

          {watchesPaused ? (
            <Callout tone="warning" title="拉取请求监听已暂停">
              <div className="flex items-center justify-between gap-2">
                <span>对话被中断后自动暂停了监听，发送新消息或手动恢复后会继续检查更新。</span>
                <Button
                  size="xs"
                  variant="secondary"
                  loading={busy === "resume-watches"}
                  onClick={() => {
                    setBusy("resume-watches");
                    void setPullRequestWatchesPaused(threadId, false)
                      .then((paused) => setWatchesPaused(paused))
                      .catch((cause) => setError(controlErrorText(cause, "恢复监听失败")))
                      .finally(() => setBusy(""));
                  }}
                >
                  恢复监听
                </Button>
              </div>
            </Callout>
          ) : null}

          {error ? (
            <Callout tone="danger" role="alert" title="操作没有完成" onDismiss={() => setError("")}>
              <span className="whitespace-pre-wrap break-words">{error}</span>
            </Callout>
          ) : null}

          {links === null ? (
            <div className="flex flex-col gap-2" aria-busy>
              {[0, 1].map((index) => (
                <div key={index} className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle p-3">
                  <Skeleton className="h-3 w-24" />
                  <Skeleton className="h-3.5 w-[80%]" />
                  <Skeleton className="h-2.5 w-[50%]" />
                </div>
              ))}
            </div>
          ) : null}

          {links && links.length > 0 ? (
            <section aria-label="已关联的拉取请求" className="flex flex-col gap-1.5">
              <SectionHeader title="已关联" count={links.length} />
              <ul className="m-0 flex list-none flex-col gap-2 p-0">
                {links.map((link) => {
                  const label = pullRequestLabel(link.key);
                  const id = link.url || label;
                  return (
                    <LinkedPullRequest
                      key={id}
                      link={link}
                      busy={busy}
                      refreshing={refreshing.has(id) && refreshing.get(id) === revision}
                      failure={failures.get(label)}
                      now={now}
                      onAction={onLinkedAction}
                      onOpenDetail={(item) => setSelectedKey(pullRequestLabel(item.key))}
                    />
                  );
                })}
              </ul>
            </section>
          ) : null}

          {branchPulls.length > 0 ? (
            <section aria-label="当前分支的拉取请求" className="flex flex-col gap-1.5">
              <SectionHeader title={listing?.branch ? `分支 ${listing.branch} 上的拉取请求` : "当前分支的拉取请求"} count={branchPulls.length} />
              <p className="px-1 text-cx-xs leading-4 text-cx-fg-4">还没有关联到这个线程。关联后可以查看检查、审阅状态，并开启监控。</p>
              <ul className="m-0 flex list-none flex-col gap-1.5 p-0">
                {branchPulls.map((record) => (
                  <BranchPullRequest key={record.html_url} record={record} busy={busy === `branch:${record.html_url}`} onLink={linkBranch} />
                ))}
              </ul>
            </section>
          ) : null}

          {links && links.length === 0 && branchPulls.length === 0 ? (
            <EmptyState
              compact
              icon="gitPullRequest"
              title="还没有关联拉取请求"
              description="关联后，这里会显示检查、审阅和能否合并。开启监控后，检查失败、出现新评论或合并冲突时，线程里的 Agent 会收到通知。"
              action={canCreate ? (
                <Button size="sm" variant="secondary" icon="gitPullRequest" onClick={() => setCreateOpen(true)}>创建 PR…</Button>
              ) : undefined}
            />
          ) : null}

          <p className="px-1 text-cx-xs leading-4 text-cx-fg-4">
            读取状态需要代码平台的访问令牌，可以在{" "}
            <ConversationRouteLink href={SETTINGS_HREF} className="text-cx-fg-3 underline-offset-2 hover:text-cx-fg hover:underline">设置 → 代码平台</ConversationRouteLink>
            {" "}中连接。
          </p>
        </div>
      </ScrollArea>
      {canListBranch ? <CreatePullRequestDialog open={createOpen} onOpenChange={setCreateOpen} threadId={threadId} status={status} /> : null}
    </div>
  );
}
