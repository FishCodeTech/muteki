"use client";

import { useMemo, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Badge, Callout, CopyButton, ScrollArea, Spinner } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { chatPanel } from "@/lib/chatPanelStore";
import { conversationErrorMessage, isBrowserTool } from "@/components/conversation/conversationEventViews";
import {
  SectionHeader,
  artifactPayload,
  formatBytes,
  isDiffArtifact,
  openExecutionLog,
  statusPresentation,
  toolPayload,
  workspaceKindLabel,
  workspaceLabel,
  type ConversationArtifact,
} from "./shared";

function Row({
  icon,
  title,
  subtitle,
  onClick,
  trailing,
  tone = "default",
}: {
  icon: IconName;
  title: ReactNode;
  subtitle?: ReactNode;
  onClick?: () => void;
  trailing?: ReactNode;
  tone?: "default" | "running" | "accent";
}) {
  const body = (
    <>
      <span
        className={cn(
          "grid size-8 shrink-0 place-items-center rounded-lg",
          tone === "running" ? "bg-cx-accent-soft text-cx-accent" : tone === "accent" ? "bg-cx-accent-soft text-cx-accent" : "bg-cx-hover text-cx-fg-3",
        )}
      >
        {tone === "running" ? <Spinner size={14} /> : <Icon name={icon} size={15} />}
      </span>
      <span className="flex min-w-0 flex-1 flex-col text-left">
        <span className="truncate text-[13px] font-medium text-cx-fg">{title}</span>
        {subtitle ? <span className="truncate text-[12px] text-cx-fg-4">{subtitle}</span> : null}
      </span>
      {trailing ?? (onClick ? <Icon name="chevronRight" size={14} className="shrink-0 text-cx-fg-4" /> : null)}
    </>
  );
  if (!onClick) return <div className="flex items-center gap-3 rounded-xl px-2 py-1.5">{body}</div>;
  return (
    <button type="button" onClick={onClick} className="cx-press flex w-full items-center gap-3 rounded-xl px-2 py-1.5 transition-colors hover:bg-cx-hover">
      {body}
    </button>
  );
}

function Card({ children, className }: { children: ReactNode; className?: string }) {
  return <section className={cn("rounded-2xl border border-cx-border-subtle bg-cx-elevated p-1.5", className)}>{children}</section>;
}

export function OverviewSurface({ threadId, view, events, tools, onOpenDetails }: SurfaceProps) {
  const status = statusPresentation(view);
  const errorMessage = conversationErrorMessage(view.turns.at(-1)?.error) || conversationErrorMessage(view.state.last_error);
  const runningTools = useMemo(() => tools.filter((tool) => tool.status === "running"), [tools]);
  const browserTools = useMemo(() => tools.filter((tool) => isBrowserTool(tool) && tool.status === "completed"), [tools]);
  const { outputs, sources, diffs } = useMemo(() => {
    const created = new Set<string>();
    for (const event of events) {
      const sha256 = typeof event.payload.sha256 === "string" ? event.payload.sha256 : "";
      if (!sha256) continue;
      if (event.event_type === "core.artifact.created") created.add(sha256);
    }
    const out: ConversationArtifact[] = [];
    const src: ConversationArtifact[] = [];
    const diff: ConversationArtifact[] = [];
    for (const artifact of view.artifacts || []) {
      if (isDiffArtifact(artifact)) {
        diff.push(artifact);
        continue;
      }
      const isCreated = created.has(artifact.sha256)
        || (Boolean(artifact.kind) && artifact.kind !== "conversation.upload");
      if (isCreated) out.push(artifact);
      else src.push(artifact);
    }
    return { outputs: out, sources: src, diffs: diff };
  }, [events, view.artifacts]);

  const workspacePath = view.workspace?.root_path || "";
  const model = view.runtime.model || view.runtime.adapter_id;

  return (
    <ScrollArea className="flex-1">
      <div className="flex flex-col gap-4 p-4" data-testid="overview-surface">
        <Card className="p-3">
          <div className="flex items-start gap-3">
            <div className="min-w-0 flex-1">
              <p className="text-[12px] font-medium text-cx-fg-4">{view.state.status === "archived" ? "会话已归档" : view.state.status === "active" ? "会话可继续" : "会话生命周期未上报"} · {view.state.running_turn_id ? "当前轮次" : "最近轮次"}执行状态</p>
              <p className="mt-0.5 truncate text-[15px] font-semibold text-cx-fg">{view.thread.title || "未命名对话"}</p>
            </div>
            <Badge tone={status.tone} dot>{status.label}</Badge>
          </div>
          <dl className="mt-3 grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 text-[13px]">
            <dt className="text-cx-fg-4">当前选择模型</dt>
            <dd className="flex min-w-0 items-center gap-1.5 text-cx-fg-2">
              <span className="truncate font-cx-mono text-[12px]">{model}</span>
              <span className={cn("size-1.5 shrink-0 rounded-full", view.runtime_connection?.connected ? "bg-cx-success" : "bg-cx-fg-4")} />
              <span className="shrink-0 text-cx-fg-4">{view.runtime_connection?.connected ? "已连接" : "未连接"}</span>
            </dd>
            <dt className="text-cx-fg-4">服务工作目录</dt>
            <dd className="min-w-0 text-cx-fg-2">
              <span className="block truncate">{workspaceLabel(view)} · {workspaceKindLabel(view)}</span>
              {workspacePath ? (
                <span className="mt-0.5 flex min-w-0 items-center gap-1">
                  <code className="min-w-0 truncate font-cx-mono text-[12px] text-cx-fg-4" title={workspacePath}>{workspacePath}</code>
                  <CopyButton text={workspacePath} label="复制路径" />
                </span>
              ) : null}
            </dd>
            <dt className="text-cx-fg-4">回合</dt>
            <dd className="cx-tabular text-cx-fg-2">{view.turns.length}</dd>
          </dl>
          {errorMessage ? <Callout tone="danger" className="mt-3" role="alert">{errorMessage}</Callout> : null}
        </Card>

        <div>
          <SectionHeader title="变更" count={diffs.length} />
          <Card>
            {diffs.length ? (
              <Row
                icon="gitCompare"
                tone="accent"
                title={`${diffs.length} 个回合产生了文件改动`}
                subtitle="按回合或工作树审查代码变更"
                onClick={() => chatPanel.openDiff(threadId, { kind: "current_turn" })}
              />
            ) : (
              <Row
                icon="gitCompare"
                title="查看工作树改动"
                subtitle={workspacePath ? "暂无回合变更产物" : "当前会话未绑定工作区"}
                onClick={workspacePath ? () => chatPanel.openDiff(threadId, { kind: "worktree" }) : undefined}
              />
            )}
          </Card>
        </div>

        <div>
          <SectionHeader
            title="后台进程"
            count={runningTools.length}
            action={tools.length ? (
              <button type="button" onClick={openExecutionLog} className="text-[12px] font-medium text-cx-accent hover:underline">执行日志</button>
            ) : undefined}
          />
          <Card>
            {runningTools.length ? runningTools.map((tool) => (
              <Row key={tool.id} icon="terminal" tone="running" title={tool.name} subtitle={tool.argsSummary || "正在执行"} onClick={() => onOpenDetails(toolPayload(tool))} />
            )) : (
              <Row icon="checkCheck" title="当前没有后台进程" subtitle={tools.length ? `本会话共执行 ${tools.length} 次工具调用` : undefined} />
            )}
          </Card>
        </div>

        <div>
          <SectionHeader title="交付内容" count={outputs.length} />
          <Card>
            {outputs.length ? outputs.map((artifact) => (
              <Row
                key={artifact.sha256}
                icon="file"
                title={artifact.name || artifact.sha256.slice(0, 12)}
                subtitle={[artifact.kind || "文件", formatBytes(artifact.size)].filter(Boolean).join(" · ")}
                onClick={() => onOpenDetails(artifactPayload(view, artifact))}
              />
            )) : (
              <Row icon="package" title="还没有交付内容" subtitle="Agent 生成的产物会出现在这里" />
            )}
          </Card>
        </div>

        {(sources.length || browserTools.length || workspacePath) ? (
          <div>
            <SectionHeader title="来源" count={sources.length + browserTools.length + (workspacePath ? 1 : 0)} />
            <Card>
              {workspacePath ? (
                <Row icon="folder" title={workspaceLabel(view)} subtitle={workspacePath} onClick={() => chatPanel.open(threadId, "files")} />
              ) : null}
              {sources.map((artifact) => (
                <Row
                  key={artifact.sha256}
                  icon="paperclip"
                  title={artifact.name || artifact.sha256.slice(0, 12)}
                  subtitle={["会话附件", formatBytes(artifact.size)].filter(Boolean).join(" · ")}
                  onClick={() => onOpenDetails(artifactPayload(view, artifact))}
                />
              ))}
              {browserTools.map((tool) => (
                <Row key={tool.id} icon="globe" title={tool.argsSummary || tool.name} subtitle={tool.name} onClick={() => onOpenDetails(toolPayload(tool))} />
              ))}
            </Card>
          </div>
        ) : null}
      </div>
    </ScrollArea>
  );
}
