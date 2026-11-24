"use client";

import React from "react";
import { Badge, IconButton, type Tone } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { compactNumber } from "../chat/composer/format";

export interface ThreadGoal {
  objective?: string;
  status?: string;
  tokens_used?: number;
  token_budget?: number;
  checks?: number;
  last_check?: string;
}

const STATUS_LABEL: Record<string, string> = {
  active: "进行中",
  paused: "已暂停",
  blocked: "停滞",
  usage_limited: "已达用量上限",
  budget_limited: "已达 token 预算",
  complete: "已完成",
};

const STATUS_TONE: Record<string, Tone> = {
  active: "running",
  paused: "warning",
  blocked: "danger",
  usage_limited: "warning",
  budget_limited: "warning",
  complete: "success",
};

function usageLine(goal: ThreadGoal): string {
  const parts: string[] = [];
  const used = typeof goal.tokens_used === "number" ? goal.tokens_used : null;
  const budget = typeof goal.token_budget === "number" ? goal.token_budget : null;
  if (used != null && budget != null) parts.push(`${compactNumber(used)} / ${compactNumber(budget)} token`);
  else if (used != null) parts.push(`${compactNumber(used)} token`);
  if (typeof goal.checks === "number") parts.push(`已检查 ${goal.checks} 次`);
  if (goal.last_check) parts.push(`最近一次检查：${goal.last_check}`);
  return parts.join(" · ");
}

export function ComposerGoalStrip({
  goal,
  busy = false,
  onCommand,
  onEdit,
  className = "",
}: {
  goal: ThreadGoal;
  busy?: boolean;
  onCommand: (text: string) => void;
  onEdit: () => void;
  className?: string;
}) {
  const status = String(goal.status || "active");
  const label = STATUS_LABEL[status] || status;
  const usage = usageLine(goal);
  return (
    <section
      className={cn(
        "mx-2 mb-[-1px] flex items-center gap-2 overflow-hidden rounded-t-2xl border border-b-0 border-cx-border bg-cx-bg-subtle px-3 py-1.5",
        className,
      )}
      aria-label={`目标 · ${label}`}
      data-testid="composer-goal-strip"
    >
      <Icon name="target" size={13} className="shrink-0 text-cx-accent" />
      <Badge tone={STATUS_TONE[status] || "neutral"}>{label}</Badge>
      <div className="min-w-0 flex-1">
        <p className="truncate text-cx-sm leading-5 text-cx-fg-2" title={goal.objective}>{goal.objective || "目标"}</p>
        {usage ? <p className="truncate text-cx-xs leading-4 text-cx-fg-4">{usage}</p> : null}
      </div>
      {status === "active" ? (
        <IconButton size="xs" icon="pause" label="暂停目标" disabled={busy} onClick={() => onCommand("/goal pause")} />
      ) : status === "paused" ? (
        <IconButton size="xs" icon="play" label="恢复目标" disabled={busy} onClick={() => onCommand("/goal resume")} />
      ) : null}
      <IconButton size="xs" icon="pencil" label="编辑目标" disabled={busy} onClick={onEdit} />
      <IconButton size="xs" icon="x" label="清除目标" disabled={busy} className="hover:text-cx-danger" onClick={() => onCommand("/goal clear")} />
    </section>
  );
}
