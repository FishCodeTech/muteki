"use client";

import Link from "next/link";
import { useState } from "react";
import { Button, InlineStatus } from "@muteki/ui";
import { Icon, type IconName } from "@muteki/ui";
import { recentStateLabel, useSharedWorkspaceOverview } from "@/components/WorkspaceNav";
import type { WorkspaceKindEntry } from "@/lib/workspace-kinds";
import { useSolveOnlyMode } from "@/lib/workspaceMode";

function iconOf(workspace: WorkspaceKindEntry): IconName {
  if (workspace.id === "pentest") return "target";
  if (workspace.icon === "flag") return "crosshair";
  if (workspace.icon === "trophy") return "grid";
  return "terminal";
}

function actionOf(workspace: WorkspaceKindEntry): string {
  if (workspace.id === "pentest") return "开始测试";
  if (workspace.aggregateType === "thread") return "开始对话";
  if (workspace.aggregateType === "run") return "下发任务";
  if (workspace.aggregateType === "competition") return "进入比赛";
  return "打开工作区";
}

function descriptionOf(workspace: WorkspaceKindEntry): string {
  if (workspace.id === "pentest") return "管理授权测试任务与报告。";
  if (workspace.aggregateType === "thread") {
    return "与 Agent 对话并查看审批、产物和记录。";
  }
  if (workspace.aggregateType === "run") {
    return "提交 CTF 题目并查看解题记录。";
  }
  if (workspace.aggregateType === "competition") {
    return "同步比赛题目，跟踪进度与裁定。";
  }
  return workspace.description;
}

function HomeContent() {
  const overview = useSharedWorkspaceOverview();
  const solveOnly = useSolveOnlyMode();
  const visibleKinds = overview.kinds.filter((kind) => !solveOnly || kind.aggregateType === "run");
  const visibleRecent = overview.recent.filter((item) => !solveOnly || item.kindId === "single-security-task");
  const [refreshing, setRefreshing] = useState(false);
  const refresh = async () => {
    setRefreshing(true);
    try { await overview.refresh(); } finally { setRefreshing(false); }
  };
  const recovering = overview.loading || overview.error ? [] : visibleKinds.filter((kind) => !kind.ready);

  return (
    <main className="workspace-home">
      <section className="workspace-home-hero">
        <h1>选择工作区</h1>
        {recovering.length ? (
          <InlineStatus tone="warning" className="workspace-home-status" action={<Link href="/settings/operations">运行诊断</Link>}>
            {recovering.length === 1 ? `${recovering[0].title}正在恢复` : `${recovering.length} 个工作区正在恢复`}
          </InlineStatus>
        ) : null}
      </section>

      {overview.error ? <div className="workspace-home-error" role="alert"><span>暂时无法刷新工作区。{overview.hasRefreshed ? "正在显示上次读取的内容。" : "请检查服务连接后重试。"}</span><Button size="sm" variant="ghost" loading={refreshing} onClick={() => void refresh()}>重试</Button></div> : null}
      <section className="workspace-home-grid" aria-label="工作区列表" aria-busy={overview.loading}>
        {overview.loading && !overview.kinds.length ? Array.from({ length: solveOnly ? 1 : 3 }, (_, index) => (
          <div className="workspace-home-card workspace-home-placeholder" key={index} aria-hidden="true">
            <div className="t-skeleton ux-skeleton-line short" /><div className="t-skeleton ux-skeleton-line" />
            <div className="t-skeleton ux-skeleton-line" /><div className="t-skeleton ux-skeleton-line short" />
          </div>
        )) : null}
        {visibleKinds.map(workspace => {
          const activity = overview.activity[workspace.id];
          const running = activity?.running ?? 0;
          const pending = (activity?.unread ?? 0) + (activity?.approvals ?? 0);
          return (
            <Link href={workspace.createEntry} className={`workspace-home-card${workspace.ready ? "" : " pending"}`} key={workspace.id}>
              <h2><Icon name={iconOf(workspace)} size={20} />{workspace.title}</h2>
              <p>{descriptionOf(workspace)}</p>
              {running > 0 || pending > 0 ? (
                <div className="workspace-home-points">
                  {running > 0 ? <span className="workspace-home-running">{running} 个运行中</span> : null}
                  {pending > 0 ? <span className="workspace-home-attention">{pending} 个待处理</span> : null}
                </div>
              ) : null}
              <strong className="workspace-home-action">{actionOf(workspace)}<Icon name="chevronRight" size={15} /></strong>
            </Link>
          );
        })}
      </section>

      <section className="workspace-home-recent" aria-label="最近工作区">
        <div className="workspace-home-recent-head"><h2>继续最近的工作</h2><Button size="sm" variant="ghost" icon="refresh" loading={refreshing} disabled={overview.loading} onClick={() => void refresh()} aria-label="刷新工作区">刷新</Button></div>
        {visibleRecent.length ? (
          <div className="workspace-home-recent-list">
            {visibleRecent.slice(0, 8).map((item) => (
              <Link href={item.href} key={`${item.kindId}:${item.id}`}>
                <span><strong>{item.title}</strong><small>{recentStateLabel(item.status, item.kindId, item.running)}</small></span>
                <span className="workspace-home-recent-state">
                  {item.running ? <b>运行中</b> : null}{item.unread ? <b>未读</b> : null}{item.approval ? <b>待审批</b> : null}<Icon name="chevronRight" size={14} />
                </span>
              </Link>
            ))}
          </div>
        ) : overview.loading ? <div className="workspace-home-empty" role="status">正在读取最近工作区…</div>
          : !overview.error ? <div className="workspace-home-empty">还没有工作记录。从上方选择一个工作区开始，之后可在这里继续。</div> : null}
      </section>

      <footer className="workspace-home-footer">
        {solveOnly ? <><span>做题设置</span><Link href="/ctf/workers">CTF Worker</Link><Link href="/ctf/workers?section=local_pi">本地 Pi 单会话</Link><Link href="/ctf/workers?section=credentials">Agent 凭据</Link><Link href="/settings/appearance">显示模式</Link></>
          : <><span>运行环境与扩展</span><Link href="/settings/agents">Agent Runtime</Link><Link href="/ctf/workers">CTF Worker</Link><Link href="/ctf/workers?section=local_pi">本地 Pi 单会话</Link><Link href="/settings/extensions">扩展</Link><Link href="/settings/operations">运行诊断</Link></>}
      </footer>
    </main>
  );
}

export default function Page() {
  return <HomeContent />;
}
