"use client";

import { useEffect, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Badge, Button, EmptyState, IconButton, ScrollArea, Skeleton, type Tone } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { useThreadPullRequests, type ThreadPullRequestListing } from "@/lib/threadGitActions";
import { CreatePullRequestDialog } from "@/components/conversation/ThreadGitActions";
import { SurfaceToolbar, relativeTime } from "./shared";

type PrResult = ThreadPullRequestListing;

const STATE_LABEL: Record<string, string> = { open: "开放", closed: "已关闭", draft: "草稿", merged: "已合并" };
const STATE_TONE: Record<string, Tone> = { open: "success", closed: "danger", draft: "neutral", merged: "accent" };

const EMPTY_COPY: Record<PrResult["state"], { title: string; detail: string }> = {
  no_workspace: { title: "未绑定工作区", detail: "当前会话未绑定工作目录。" },
  no_git: { title: "非 Git 仓库", detail: "工作目录不是 Git 仓库。" },
  no_remote: { title: "没有远端", detail: "仓库未配置远端 origin，无法检测 PR。" },
  not_github: { title: "非 GitHub 远端", detail: "远端不是 GitHub，暂不支持 PR 检测。" },
  error: { title: "检测失败", detail: "GitHub API 请求失败。" },
  no_pr: { title: "暂无关联 PR", detail: "当前分支没有关联的 Pull request。" },
  ok: { title: "", detail: "" },
};

export function PullRequestSurface({ threadId, view, active }: SurfaceProps) {
  const prs = useThreadPullRequests(threadId, false);
  const { result, loading, error } = prs;
  const projectId = view?.thread.project_id || view?.workspace?.project_id || undefined;
  const git = useSharedGitStatus(threadId, projectId);
  const [createOpen, setCreateOpen] = useState(false);
  const load = prs.refresh;

  useEffect(() => {
    if (active && !result && !error && !loading) void load();
  }, [active, result, error, loading, load]);

  const header = result && (result.owner || result.branch) ? (
    <SurfaceToolbar className="px-3">
      <Icon name="gitPullRequest" size={14} className="text-cx-fg-3" />
      <span className="min-w-0 truncate text-[13px] font-medium text-cx-fg">{result.owner}/{result.repo}</span>
      {result.branch ? <span className="min-w-0 truncate font-cx-mono text-[12px] text-cx-fg-4">· {result.branch}</span> : null}
      <span className="flex-1" />
      <IconButton icon="refresh" label="刷新" loading={loading} onClick={() => void load()} />
    </SurfaceToolbar>
  ) : null;

  if (loading && !result) {
    return (
      <div className="flex flex-col gap-3 p-4" aria-busy>
        {[0, 1, 2].map((index) => (
          <div key={index} className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle p-3">
            <Skeleton className="h-3 w-24" />
            <Skeleton className="h-3.5 w-[80%]" />
            <Skeleton className="h-2.5 w-[50%]" />
          </div>
        ))}
      </div>
    );
  }
  if (error) {
    return (
      <EmptyState
        icon="circleAlert"
        title="加载失败"
        description={error}
        action={<Button size="sm" variant="secondary" icon="refresh" onClick={() => void load()}>重试</Button>}
      />
    );
  }
  if (!result || result.state !== "ok") {
    const state = result?.state ?? "no_workspace";
    const copy = EMPTY_COPY[state];
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <EmptyState
          icon="gitPullRequest"
          title={copy.title}
          description={result?.error || copy.detail}
          action={state === "no_pr" && git.status?.current_branch ? (
            <div className="flex items-center gap-2">
              <Button size="sm" variant="secondary" icon="gitPullRequest" onClick={() => setCreateOpen(true)}>创建 PR…</Button>
              <Button size="sm" variant="ghost" icon="refresh" onClick={() => void load()}>重新检测</Button>
            </div>
          ) : state !== "no_workspace" ? <Button size="sm" variant="ghost" icon="refresh" onClick={() => void load()}>重新检测</Button> : undefined}
        />
        <CreatePullRequestDialog open={createOpen} onOpenChange={setCreateOpen} threadId={threadId} status={git.status} />
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {header}
      <ScrollArea className="flex-1">
        <ul className="m-0 flex list-none flex-col gap-2 p-3">
          {result.pull_requests.map((pr) => (
            <li key={pr.number}>
              <a
                href={pr.html_url}
                target="_blank"
                rel="noopener noreferrer"
                className="cx-press group flex flex-col gap-1.5 rounded-xl border border-cx-border-subtle bg-cx-elevated p-3 transition-colors hover:border-cx-border-strong"
              >
                <span className="flex items-center gap-2">
                  <span className="cx-tabular font-cx-mono text-[12px] text-cx-fg-3">#{pr.number}</span>
                  <Badge tone={STATE_TONE[pr.state] ?? "neutral"} dot>{STATE_LABEL[pr.state] ?? pr.state}</Badge>
                  <Icon name="externalLink" size={12} className="ml-auto text-cx-fg-4 opacity-0 transition-opacity group-hover:opacity-100" />
                </span>
                <span className="text-[14px] font-medium leading-5 text-cx-fg">{pr.title}</span>
                <span className={cn("flex flex-wrap items-center gap-x-2 text-[12px] text-cx-fg-4")}>
                  <span>{pr.user_login}</span>
                  {pr.created_at ? <span>{relativeTime(pr.created_at)}</span> : null}
                  {pr.base_ref ? <span className="font-cx-mono">{pr.head_ref} → {pr.base_ref}</span> : null}
                </span>
              </a>
            </li>
          ))}
        </ul>
      </ScrollArea>
    </div>
  );
}
