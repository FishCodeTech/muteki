"use client";

import { useId, type ReactNode } from "react";
import { Badge, Switch } from "@muteki/ui";
import { SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { cn } from "@/lib/cn";
import type { SubagentPolicy } from "@/lib/controlGrants";

export const DEFAULT_SUBAGENT_POLICY: SubagentPolicy = {
  approval_mode: "always_allow",
  child_worktrees: false,
};

const APPROVAL_MODES = [
  {
    value: "always_allow",
    label: "完全访问（全部允许）",
    description: "子 Agent 以完全访问启动；有允许选项的权限请求自动放行。",
  },
  {
    value: "auto",
    label: "引擎推荐",
    description: "使用各引擎的推荐权限；未自动放行的请求仍交主 Agent 审批。",
  },
  {
    value: "orchestrator",
    label: "主 Agent 审批",
    description: "使用引擎默认权限，执行审批和业务提问由主 Agent 处理。",
  },
  {
    value: "manual",
    label: "我手动审批",
    description: "使用引擎默认权限，在子对话中由你回答审批和业务提问。",
  },
] as const;

export function SubagentPolicySettings({
  policy,
  disabled,
  onChange,
  anchor,
  children,
}: {
  policy: SubagentPolicy;
  disabled: boolean;
  onChange: (policy: SubagentPolicy) => void;
  anchor: string;
  children?: ReactNode;
}) {
  const id = useId();
  return (
    <SettingsSection
      anchor={anchor}
      title="子 Agent 执行权限与审批"
      description="统一用于 Muteki 委派的子 Agent；引擎原生子代理仍使用各自的设置。"
    >
      <fieldset aria-label="子 Agent 执行权限与审批" disabled={disabled} className="min-w-0 border-0 p-0">
        {APPROVAL_MODES.map((mode) => {
          const selected = policy.approval_mode === mode.value;
          const descriptionId = `${id}-${mode.value}-description`;
          return (
            <label
              key={mode.value}
              className={cn(
                "flex items-start gap-3 border-b border-cx-border-subtle px-5 py-3 last:border-b-0",
                disabled ? "cursor-default opacity-60" : "cursor-pointer hover:bg-cx-hover",
                selected && "bg-cx-hover",
              )}
            >
              <input
                type="radio"
                name={id}
                value={mode.value}
                checked={selected}
                aria-describedby={descriptionId}
                onChange={() => onChange({ ...policy, approval_mode: mode.value })}
                className="mt-1 size-4 shrink-0 accent-[var(--cx-accent)]"
              />
              <span className="min-w-0 flex-1">
                <span className="flex flex-wrap items-center gap-2 text-cx-sm font-medium leading-5 text-cx-fg">
                  {mode.label}
                  {mode.value === "always_allow" ? <Badge tone="neutral">默认</Badge> : null}
                </span>
                <span id={descriptionId} className="mt-0.5 block text-cx-xs leading-5 text-cx-fg-3">
                  {mode.description}
                </span>
              </span>
            </label>
          );
        })}
      </fieldset>
      <div className="border-y border-cx-border-subtle px-5 py-3 text-cx-sm leading-5 text-cx-fg-3">
        新建子 Agent 按所选权限启动。已有子 Agent 的执行权限不变，后续审批按新策略处理；已发出的审批仍由原处理方完成。
        业务提问仍按所选模式交给主 Agent 或你回答。
      </div>
      <SettingsRow
        title="为子 Agent 创建独立 worktree"
        description="仅当它的目录是仓库根目录时。分支名为 tcode/〈线程 ID〉；委派时可单独指定。"
        control={
          <Switch
            ariaLabel="为子 Agent 创建独立 worktree"
            checked={policy.child_worktrees}
            disabled={disabled}
            onCheckedChange={(child_worktrees) => onChange({ ...policy, child_worktrees })}
          />
        }
      />
      {children}
    </SettingsSection>
  );
}
