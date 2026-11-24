"use client";

import { Badge } from "@muteki/ui";
import { NumberField } from "@/components/NumberField";
import { SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import type { WorkerSettings } from "@/lib/useRun";

export const CTF_AUTO_CONCURRENCY = 3;

export type SchedulingPatch = Partial<{
  maxTotal: number; wallClock: number; costBudget: number;
}>;

const toInt = (next: string, min: number) => Math.max(min, Math.floor(Number(next)) || min);

export function SchedulingSection({
  config, mode = "ctf", maxTotal, wallClock, costBudget, maxWorkers, onChange,
}: {
  config: WorkerSettings;
  mode?: "ctf" | "pentest";
  maxTotal: number;
  wallClock: number;
  costBudget: number;
  maxWorkers: number;
  onChange: (patch: SchedulingPatch) => void;
}) {
  const pentest = mode === "pentest";
  const rosterCap = Math.max(0, maxWorkers);
  const concurrent = pentest ? rosterCap : Math.min(rosterCap, CTF_AUTO_CONCURRENCY);
  const overrides = Object.keys(config.overrides || {}).length;

  return (
    <div className="flex flex-col gap-8">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-1 rounded-lg bg-cx-bg-subtle px-3 py-2 text-cx-sm text-cx-fg-2" aria-live="polite">
        {rosterCap ? (
          <>
            <span>单个任务最多同时运行 <strong className="font-semibold text-cx-fg tabular-nums">{concurrent}</strong> 个 Worker</span>
            <span className="text-cx-fg-3">· {pentest ? "等于启用 Worker 的并发上限之和" : `取阵容容量 ${rosterCap} 与自动调度上限 ${CTF_AUTO_CONCURRENCY} 的较小值`}</span>
            <span className="basis-full text-cx-fg-3">
              所有任务还共用执行节点的并发上限，节点满载时新 Worker 会排队等待；在
              <a href="?section=nodes" className="mx-0.5 text-cx-accent underline-offset-2 hover:underline">执行节点</a>
              中查看或调整。
            </span>
          </>
        ) : (
          <span>
            暂无可运行的 Worker，请先在
            <a href="?section=roster" className="mx-0.5 text-cx-accent underline-offset-2 hover:underline">出战配置</a>
            中启用 Worker
          </span>
        )}
      </div>

      <SettingsSection title="运行预算" actions={<Badge>0 = 不限</Badge>}>
        <SettingsRow
          title="累计启动上限"
          description="到限后等待已启动 Worker 完成"
          control={<NumberField ariaLabel="累计启动上限" min={0} value={maxTotal} onChange={(next) => onChange({ maxTotal: toInt(next, 0) })} />}
        />
        <SettingsRow
          title="最长运行时间"
          description="到限后结束剩余任务"
          control={<NumberField ariaLabel="最长运行时间" min={0} suffix="秒" value={wallClock} onChange={(next) => onChange({ wallClock: toInt(next, 0) })} />}
        />
        <SettingsRow
          title="成本预算"
          description="按已知金额；到限仅限制后续启动"
          control={<NumberField ariaLabel="成本预算" min={0} step={0.1} suffix="USD" value={costBudget} onChange={(next) => onChange({ costBudget: Math.max(0, Number(next) || 0) })} />}
        />
      </SettingsSection>

      {overrides ? <SettingsNote>已保留 {overrides} 个题型专属配置</SettingsNote> : null}
    </div>
  );
}
