"use client";

import { useState } from "react";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import type { ConversationReadiness, ReadinessBlocker } from "@/lib/conversationReadiness";
import { Icon, type IconName } from "../Icon";
import { Button, Spinner } from "@/components/chat/ui";

export interface ConversationReadinessBannerProps {
  readiness: ConversationReadiness;
  probing?: boolean;
  onRetry: () => void;
  onProbe: () => void;
  onDismissGuide: () => void;
  onPickDirectory?: () => void;
  onEnterPath?: () => void;
  directoryControlInComposer?: boolean;
  className?: string;
}

function kindLabel(kind: ReadinessBlocker["kind"]): string {
  switch (kind) {
    case "connecting":
      return "连接中";
    case "config_read_failed":
      return "配置读取失败";
    case "not_installed":
      return "未安装";
    case "discovery_unavailable":
      return "宿主检测已禁用";
    case "not_logged_in":
      return "未登录";
    case "model_unavailable":
      return "模型不可用";
    case "directory_inaccessible":
      return "目录不可访问";
    case "runtime_unhealthy":
      return "接入异常";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

function kindIcon(kind: ReadinessBlocker["kind"]): IconName {
  switch (kind) {
    case "connecting":
      return "loader";
    case "config_read_failed":
      return "circleAlert";
    case "not_installed":
      return "package";
    case "discovery_unavailable":
      return "circleAlert";
    case "not_logged_in":
      return "lock";
    case "model_unavailable":
      return "cpu";
    case "directory_inaccessible":
      return "folder";
    case "runtime_unhealthy":
      return "plug";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

function BlockerActions({
  blocker,
  probing,
  onRetry,
  onProbe,
  onPickDirectory,
  onEnterPath,
}: {
  blocker: ReadinessBlocker;
  probing?: boolean;
  onRetry: () => void;
  onProbe: () => void;
  onPickDirectory?: () => void;
  onEnterPath?: () => void;
}) {
  const [copied, setCopied] = useState(false);
  const copyCommand = async () => {
    if (!blocker.loginCommand) return;
    const ok = await copyToClipboard(blocker.loginCommand);
    if (ok) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1600);
    }
  };

  return (
    <div className="flex flex-wrap items-center gap-1.5">
      {blocker.recovery === "retry" ? (
        <Button size="xs" variant="secondary" icon="retry" data-testid="conversation-readiness-retry" onClick={onRetry}>
          重试
        </Button>
      ) : null}
      {blocker.recovery === "probe" || blocker.kind === "runtime_unhealthy" ? (
        <Button size="xs" variant="secondary" icon="refresh" loading={probing} data-testid="conversation-readiness-probe" onClick={onProbe}>
          {probing ? "验证中…" : "验证 Agent"}
        </Button>
      ) : null}
      {blocker.recovery === "open_agents" || blocker.kind === "not_installed" || blocker.kind === "not_logged_in" || blocker.kind === "model_unavailable" ? (
        <a
          className="cx-press inline-flex h-6 items-center gap-1 rounded-md px-2 text-[12px] font-medium text-cx-accent hover:bg-cx-accent-soft"
          href="/settings/agents"
        >
          前往 Agents
          <Icon name="arrowUpRight" size={12} />
        </a>
      ) : null}
      {blocker.recovery === "pick_directory" && onPickDirectory ? (
        <Button size="xs" variant="secondary" icon="folderOpen" onClick={onPickDirectory}>选择目录</Button>
      ) : null}
      {blocker.recovery === "enter_path" && onEnterPath ? (
        <Button size="xs" variant="secondary" data-testid="conversation-readiness-enter-path" onClick={onEnterPath}>
          输入路径
        </Button>
      ) : null}
      {blocker.loginCommand ? (
        <Button size="xs" variant="ghost" icon={copied ? "check" : "copy"} onClick={() => void copyCommand()}>
          {copied ? "已复制" : "复制登录命令"}
        </Button>
      ) : null}
    </div>
  );
}

function GuideStep({
  index,
  done,
  title,
  children,
}: {
  index: number;
  done: boolean;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <li data-done={done ? "true" : "false"} className="flex min-h-9 items-center gap-3">
      <span
        className={cn(
          "grid size-5 shrink-0 place-items-center rounded-full text-[11px] font-semibold transition-colors",
          done ? "bg-cx-success text-white" : "border border-cx-border-strong text-cx-fg-3",
        )}
      >
        {done ? <Icon name="check" size={11} /> : index}
      </span>
      <span className={cn("flex-1 text-[13px]", done ? "text-cx-fg-3" : "font-medium text-cx-fg")}>{title}</span>
      <span className="flex items-center gap-1.5 text-[12px] text-cx-fg-4">{children}</span>
    </li>
  );
}

export function ConversationReadinessBanner({
  readiness,
  probing = false,
  onRetry,
  onProbe,
  onDismissGuide,
  onPickDirectory,
  onEnterPath,
  directoryControlInComposer = false,
  className = "",
}: ConversationReadinessBannerProps) {
  if (readiness.guide === "hidden" && !readiness.blockers.length) return null;

  const sendBlockers = readiness.blockers.filter((row) => (
    row.kind !== "connecting" || row.recovery === "probe" || row.message.includes("尚未验证")
  ));
  const connecting = readiness.blockers.some((row) => row.kind === "connecting" && row.message === "正在连接…");
  const showChecklist = readiness.guide === "first_run";
  if (!connecting && !sendBlockers.length && !showChecklist) return null;

  return (
    <div
      className={cn("cx-animate-in mb-2 flex w-full flex-col gap-2", className)}
      data-testid="conversation-readiness-banner"
      data-guide={readiness.guide}
      data-can-send={readiness.canSend ? "true" : "false"}
    >
      {connecting ? (
        <div className="is-connecting flex items-center gap-2 px-1 text-[12.5px] text-cx-fg-3" data-kind="connecting">
          <Spinner size={12} />
          <span>正在连接 Agent…</span>
        </div>
      ) : null}

      {sendBlockers.map((blocker, index) => {
        const soft = blocker.kind === "connecting";
        return (
          <div
            key={`${blocker.kind}:${blocker.source || ""}:${index}`}
            className={cn(
              "flex flex-col gap-2 rounded-xl border px-3 py-2.5 sm:flex-row sm:items-center",
              `is-${blocker.kind}`,
              soft
                ? "border-cx-border bg-cx-bg-subtle"
                : "border-[color-mix(in_srgb,var(--amber)_32%,transparent)] bg-cx-warning-soft",
            )}
            data-kind={blocker.kind}
          >
            <div className="flex min-w-0 flex-1 items-start gap-2.5">
              <Icon name={kindIcon(blocker.kind)} size={15} className={cn("mt-[2px] shrink-0", soft ? "text-cx-fg-3" : "text-cx-warning")} />
              <div className="min-w-0">
                <p className="text-[13px] leading-5 text-cx-fg">
                  <strong className="mr-1.5 font-semibold">{kindLabel(blocker.kind)}</strong>
                  <span className="text-cx-fg-2">{blocker.message}</span>
                </p>
                {blocker.loginCommand ? (
                  <code className="mt-1.5 block break-all rounded-lg bg-cx-code px-2 py-1 font-cx-mono text-[12px] text-cx-fg">
                    {blocker.loginCommand}
                  </code>
                ) : null}
                {blocker.loginNote ? <small className="mt-1 block text-[12px] text-cx-fg-3">{blocker.loginNote}</small> : null}
              </div>
            </div>
            <BlockerActions
              blocker={blocker}
              probing={probing}
              onRetry={onRetry}
              onProbe={onProbe}
              onPickDirectory={onPickDirectory}
              onEnterPath={onEnterPath}
            />
          </div>
        );
      })}

      {showChecklist ? (
        <div className="rounded-2xl border border-cx-border bg-cx-elevated px-4 pb-2 pt-3 shadow-cx-xs" data-testid="conversation-readiness-guide">
          <div className="mb-1 flex items-center gap-2">
            <Icon name="sparkles" size={14} className="text-cx-accent" />
            <strong className="text-[13px] font-semibold text-cx-fg">开始之前</strong>
            <span className="hidden flex-1 truncate text-[12px] text-cx-fg-4 sm:inline">选择执行环境并验证 Agent。开始后不能补选目录；文件/终端需新建已绑定目录的会话。</span>
            <Button size="xs" variant="ghost" className="ml-auto text-cx-fg-3" onClick={onDismissGuide} data-testid="conversation-readiness-skip">
              跳过引导
            </Button>
          </div>
          <ol className="m-0 flex list-none flex-col p-0">
            <GuideStep index={1} done={readiness.envReady} title="选择执行环境">
              {readiness.envReady ? "已选择" : "使用输入框下方的模型选择器"}
            </GuideStep>
            <GuideStep index={2} done={readiness.agentVerified} title="验证 Agent">
              {readiness.agentVerified ? "接入环境已验证；具体模型是否成功调用见模型选择器。" : (
                <Button size="xs" variant="secondary" loading={probing} onClick={onProbe}>
                  {probing ? "验证中…" : "验证 Agent"}
                </Button>
              )}
            </GuideStep>
            <GuideStep index={3} done={readiness.directorySelected} title="选择工作目录">
              {readiness.directorySelected ? "已选择" : directoryControlInComposer ? "通过输入框下方的工作区入口选择目录或输入服务宿主路径。普通聊天无需目录。" : (
                <>
                  {onPickDirectory ? <Button size="xs" variant="secondary" onClick={onPickDirectory}>选择目录</Button> : null}
                  {onEnterPath ? <Button size="xs" variant="ghost" onClick={onEnterPath}>输入路径</Button> : null}
                  <span className="hidden sm:inline">开始后不可补选</span>
                </>
              )}
            </GuideStep>
          </ol>
        </div>
      ) : null}
    </div>
  );
}
