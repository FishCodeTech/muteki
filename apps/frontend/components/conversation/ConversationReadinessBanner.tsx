"use client";

import { useState } from "react";
import { cn } from "@/lib/cn";
import { copyToClipboard } from "@/lib/clipboard";
import type { ConversationReadiness, ReadinessBlocker } from "@/lib/conversationReadiness";
import { Icon, type IconName } from "@muteki/ui";
import { Button, Spinner } from "@muteki/ui";
import { ConversationRouteLink } from "./ConversationNavigation";

export interface ConversationReadinessBannerProps {
  readiness: ConversationReadiness;
  probing?: boolean;
  onRetry: () => void;
  onProbe: () => void;
  onDismissGuide: () => void;
  onPickDirectory?: () => void;
  onEnterPath?: () => void;
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
    case "version_unsupported":
      return "版本过低";
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
    case "version_unsupported":
      return "shield";
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
      {blocker.recovery === "open_agents" || blocker.kind === "not_installed" || blocker.kind === "version_unsupported" || blocker.kind === "not_logged_in" || blocker.kind === "model_unavailable" ? (
        <ConversationRouteLink
          className="cx-press inline-flex h-6 items-center gap-1 rounded-md px-2 text-cx-xs font-medium text-cx-accent no-underline hover:bg-cx-accent-soft"
          href={blocker.kind === "version_unsupported" ? "/settings/agents#setting-agents-version-policy" : "/settings/agents"}
        >
          {blocker.kind === "model_unavailable" ? "配置模型" : blocker.kind === "version_unsupported" ? "版本支持设置" : "前往 Agents"}
          <Icon name="arrowUpRight" size={12} />
        </ConversationRouteLink>
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
          {copied ? "已复制" : blocker.kind === "version_unsupported" ? "复制升级命令" : "复制登录命令"}
        </Button>
      ) : null}
    </div>
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
  className = "",
}: ConversationReadinessBannerProps) {
  const [dismissed, setDismissed] = useState<ReadonlySet<string>>(() => new Set());
  if (readiness.guide === "hidden" && !readiness.blockers.length) return null;

  const blockerKey = (row: ReadinessBlocker) => `${row.kind}:${row.source || ""}:${row.message}`;
  const sendBlockers = readiness.blockers.filter((row) => (
    (row.kind !== "connecting" || row.recovery === "probe" || row.message.includes("尚未验证"))
    && (row.kind === "model_unavailable" || !dismissed.has(blockerKey(row)))
  ));
  const connecting = readiness.blockers.some((row) => row.kind === "connecting" && row.message === "正在连接…");
  const showProbe = readiness.guide === "first_run" && readiness.envReady
    && !readiness.agentVerified && !readiness.blockers.length;
  if (!connecting && !sendBlockers.length && !showProbe) return null;

  return (
    <div
      className={cn("cx-animate-in mb-2 flex w-full flex-col gap-2", className)}
      data-testid="conversation-readiness-banner"
      data-guide={readiness.guide}
      data-can-send={readiness.canSend ? "true" : "false"}
    >
      {connecting ? (
        <div className="is-connecting flex items-center gap-2 px-1 text-cx-sm text-cx-fg-3" data-kind="connecting">
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
              "flex flex-col gap-2 rounded-xl px-3 py-2.5 sm:flex-row sm:items-start",
              `is-${blocker.kind}`,
              soft ? "border border-cx-border bg-cx-bg-subtle" : "ring-1 ring-cx-ring bg-cx-elevated shadow-cx-xs",
            )}
            data-kind={blocker.kind}
            role={blocker.kind === "model_unavailable" ? "status" : undefined}
          >
            <div className="flex min-w-0 flex-1 items-start gap-2.5">
              <Icon name={kindIcon(blocker.kind)} size={15} className={cn("mt-[2px] shrink-0", soft ? "text-cx-fg-3" : "text-cx-warning")} />
              <div className="min-w-0">
                <p className="text-cx-sm leading-5 text-cx-fg">
                  <strong className="mr-1.5 font-semibold">{kindLabel(blocker.kind)}</strong>
                  <span className="text-cx-fg-2">{blocker.message}</span>
                </p>
                {blocker.loginCommand ? (
                  <code className="mt-1.5 inline-block max-w-full break-all rounded-md bg-cx-code px-2 py-0.5 font-cx-mono text-cx-xs text-cx-fg">
                    {blocker.loginCommand}
                  </code>
                ) : null}
                {blocker.loginNote ? <small className="mt-1 block text-cx-xs text-cx-fg-3">{blocker.loginNote}</small> : null}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-1 sm:pt-px">
              <BlockerActions
                blocker={blocker}
                probing={probing}
                onRetry={onRetry}
                onProbe={onProbe}
                onPickDirectory={onPickDirectory}
                onEnterPath={onEnterPath}
              />
              {blocker.kind !== "model_unavailable" ? <button
                type="button"
                aria-label="关闭提示"
                title="关闭提示"
                onClick={() => setDismissed((current) => new Set(current).add(blockerKey(blocker)))}
                className="cx-press grid size-6 shrink-0 place-items-center rounded-lg text-cx-fg-3 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
              >
                <Icon name="x" size={13} />
              </button> : null}
            </div>
          </div>
        );
      })}

      {showProbe ? (
        <div className="flex flex-wrap items-center gap-2 px-1 text-cx-sm text-cx-fg-3" data-testid="conversation-readiness-guide">
          <span className="mr-auto">Agent 接入环境待验证</span>
          <Button size="xs" variant="secondary" loading={probing} onClick={onProbe}>
            {probing ? "验证中…" : "验证 Agent"}
          </Button>
          <Button size="xs" variant="ghost" onClick={onDismissGuide} data-testid="conversation-readiness-skip">跳过</Button>
        </div>
      ) : null}
    </div>
  );
}
