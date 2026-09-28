"use client";

import Link from "next/link";
import { useState } from "react";
import { Button } from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";
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
  if (workspace.id === "pentest") return "用自然语言描述授权目标，由 Coordinator 调度有界测试和证据复核。";
  if (workspace.aggregateType === "thread") {
    return "与外部 Agent 连续协作，查看工具调用、审批、产物和运行记录。";
  }
  if (workspace.aggregateType === "run") {
    return "提交一道 CTF 题目，由标准 Coordinator 调度 Worker 并汇总可核查结果。";
  }
  if (workspace.aggregateType === "competition") {
    return "连接比赛平台，同步题目、调度 Run、审核候选并跟踪远端裁定。";
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
  const ready = !overview.loading && !overview.error && visibleKinds.every((kind) => kind.ready);

  return (
    <main className="workspace-home">
      <section className="workspace-home-hero">
        <div>
          <span className="workspace-home-kicker">工作区</span>
          <h1>{solveOnly ? "开始做题" : "选择要完成的工作"}</h1>
          <p>{solveOnly ? "提交 CTF 题目，由 Coordinator 调度 Worker 并汇总结果。" : "对话、单题和比赛工作区共用 Agent Runtime、权限管理和可追踪的任务回执。"}</p>
        </div>
        <div className={`workspace-home-status${ready ? "" : " pending"}`} role="status">
          <i aria-hidden="true" />
          <span>{overview.loading ? "正在读取控制面" : overview.error ? "控制面读取失败" : ready ? "本地控制面已就绪" : "部分模块正在恢复"}</span>
        </div>
      </section>

      {overview.error ? <div className="workspace-home-error" role="alert"><span>暂时无法刷新工作区。{overview.refreshedAt ? "正在显示上次读取的内容。" : "请检查服务连接后重试。"}</span><Button size="sm" variant="ghost" isPending={refreshing} isDisabled={refreshing} onPress={() => void refresh()}>重试</Button></div> : null}
      <section className="workspace-home-grid" aria-label="工作区列表" aria-busy={overview.loading}>
        {overview.loading && !overview.kinds.length ? Array.from({ length: solveOnly ? 1 : 3 }, (_, index) => (
          <div className="workspace-home-card workspace-home-placeholder" key={index} aria-hidden="true">
            <div className="t-skeleton ux-skeleton-line short" /><div className="t-skeleton ux-skeleton-line" />
            <div className="t-skeleton ux-skeleton-line" /><div className="t-skeleton ux-skeleton-line short" />
          </div>
        )) : null}
        {visibleKinds.map((workspace, index) => {
          const activity = overview.activity[workspace.id];
          const points = [
            `${activity?.total ?? 0} 个工作区`,
            `${activity?.running ?? 0} 个运行中`,
            `${(activity?.unread ?? 0) + (activity?.approvals ?? 0)} 个待处理`,
          ];
          return (
            <Link href={workspace.createEntry} className={`workspace-home-card${workspace.ready ? "" : " pending"}`} key={workspace.id}>
              <div className="workspace-home-card-head"><span className="workspace-home-card-index">{String(index + 1).padStart(2, "0")}</span><Icon name={iconOf(workspace)} size={20} /></div>
              <h2>{workspace.title}</h2>
              <p>{descriptionOf(workspace)}</p>
              <div className="workspace-home-points">{points.map((point) => <span key={point}>{point}</span>)}</div>
              <strong className="workspace-home-action">{actionOf(workspace)}<Icon name="chevronRight" size={15} /></strong>
            </Link>
          );
        })}
      </section>

      <section className="workspace-home-recent" aria-label="最近工作区">
        <div className="workspace-home-recent-head"><h2>继续最近的工作</h2><Button size="sm" variant="ghost" isPending={refreshing} isDisabled={refreshing || overview.loading} onPress={() => void refresh()} aria-label="刷新工作区"><Icon name="refresh" size={14} />刷新</Button></div>
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
        {solveOnly ? <><span>做题设置</span><Link href="/ctf/workers">CTF Worker</Link><Link href="/ctf/workers?section=credentials">Agent 凭据</Link><Link href="/settings/appearance">显示模式</Link></>
          : <><span>运行环境与扩展</span><Link href="/settings/agents">Agent Runtime</Link><Link href="/ctf/workers">CTF Worker</Link><Link href="/settings/extensions">扩展</Link><Link href="/settings/operations">运行诊断</Link></>}
      </footer>
    </main>
  );
}

export default function Page() {
  return <HomeContent />;
}
