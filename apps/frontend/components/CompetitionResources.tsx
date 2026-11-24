"use client";

import { Button, Input } from "@muteki/ui";


import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import type { CompetitionSnapshotView } from "@/lib/competition-events";

const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };

const MODE_LABELS: Record<string, { label: string; hint: string }> = {
  observe: { label: "仅观察", hint: "自动同步，不调度和派发，不提交" },
  assisted: { label: "辅助（默认）", hint: "自动调度，远端提交等待确认" },
  autonomous: { label: "全自动", hint: "来源 / 预算 / 冷却满足后自动提交" },
};

const BUDGET_LABELS: Record<string, string> = {
  tokens: "Token",
  cost: "费用",
  wallclock: "墙钟",
  submissions: "平台提交",
  instances: "动态实例",
};

const SCHEDULER_LABELS: Record<string, { label: string; color: string }> = {
  stopped: { label: "已停止", color: "var(--muted)" },
  running: { label: "运行中", color: "var(--green)" },
  paused: { label: "已暂停", color: "var(--amber)" },
};

export function CompetitionResources({
  snapshot,
  busy,
  onUpdatePolicy,
  onScheduler,
}: {
  snapshot: CompetitionSnapshotView | null;
  busy: boolean;
  onUpdatePolicy: (changes: Record<string, unknown>) => void;
  onScheduler: (action: "start" | "pause" | "resume") => void;
}) {
  const policy = snapshot?.policy ?? null;
  const schedulerState = snapshot?.competition?.scheduler_state ?? "stopped";
  const [maxRuns, setMaxRuns] = useState("");
  const [maxInstances, setMaxInstances] = useState("");
  const [cooldown, setCooldown] = useState("");

  useEffect(() => {
    if (!policy) return;
    setMaxRuns(String(policy.max_concurrent_runs));
    setMaxInstances(String(policy.max_instances));
    setCooldown(String(policy.submission_cooldown_seconds));
  }, [policy]);

  const scheduler = SCHEDULER_LABELS[schedulerState] ?? {
    label: schedulerState,
    color: "var(--muted)",
  };

  return (
    <div style={{ display: "grid", gap: 12 }}>
      <section style={{ display: "grid", gap: 6 }}>
        <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
          <strong style={{ fontSize: 12 }}>调度器</strong>
          <span style={{ color: scheduler.color, fontSize: 11, fontWeight: 600 }}>
            {scheduler.label}
          </span>
          <span style={muted}>
            待决 receipt {snapshot?.pending_receipts ?? 0} · outbox{" "}
            {snapshot?.pending_outbox ?? 0}
          </span>
        </div>
        <div style={{ display: "flex", gap: 6 }}>
          {schedulerState === "stopped" && (
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => onScheduler("start")}>
              启动调度
            </Button>
          )}
          {schedulerState === "running" && (
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => onScheduler("pause")}>
              暂停调度
            </Button>
          )}
          {schedulerState === "paused" && (
            <Button size="sm" variant="secondary" disabled={busy} onClick={() => onScheduler("resume")}>
              恢复调度
            </Button>
          )}
        </div>
      </section>

      <section style={{ display: "grid", gap: 6 }}>
        <strong style={{ fontSize: 12 }}>自动化策略</strong>
        <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
          {Object.entries(MODE_LABELS).map(([mode, meta]) => {
            const active = policy?.automation_mode === mode;
            return (
              <Button
                key={mode}
                variant="outline"
                className={
                  active
                    ? "h-auto flex-col items-start gap-0 border-cx-accent-line bg-cx-accent-soft py-1.5 text-left disabled:opacity-100"
                    : "h-auto flex-col items-start gap-0 py-1.5 text-left"
                }
                disabled={busy || active}
                onClick={() => onUpdatePolicy({ automation_mode: mode })}
                data-tooltip={meta.hint}
              >
                <div style={{ fontWeight: 600 }}>{meta.label}</div>
                <div style={{ ...muted, fontWeight: 400 }}>{meta.hint}</div>
              </Button>
            );
          })}
        </div>
        <div style={{ display: "flex", gap: 10, flexWrap: "wrap", alignItems: "center" }}>
          <label style={muted}>
            并发 Run{" "}
            <Input
              size="sm"
              className="w-[72px]"
              inputMode="numeric"
              value={maxRuns}
              onChange={(e) => setMaxRuns(e.target.value)}
            />
          </label>
          <label style={muted}>
            实例上限{" "}
            <Input
              size="sm"
              className="w-[72px]"
              inputMode="numeric"
              value={maxInstances}
              onChange={(e) => setMaxInstances(e.target.value)}
            />
          </label>
          <label style={muted}>
            提交冷却（秒）{" "}
            <Input
              size="sm"
              className="w-[72px]"
              inputMode="numeric"
              value={cooldown}
              onChange={(e) => setCooldown(e.target.value)}
            />
          </label>
          <Button
            size="sm"
            variant="secondary"
            disabled={busy || !policy}
            onClick={() =>
              onUpdatePolicy({
                max_concurrent_runs: Number(maxRuns) || 0,
                max_instances: Number(maxInstances) || 0,
                submission_cooldown_seconds: Number(cooldown) || 0,
              })
            }
          >
            应用
          </Button>
        </div>
      </section>

      <section style={{ display: "grid", gap: 6 }}>
        <strong style={{ fontSize: 12 }}>预算</strong>
        {!snapshot?.budgets?.length ? (
          <div style={muted}>尚无预算桶（由 Scheduler 汇总用量后写回）。</div>
        ) : (
          snapshot.budgets.map((b) => {
            const pct = b.limit > 0 ? Math.min(100, (b.used / b.limit) * 100) : 0;
            const over = b.limit > 0 && b.used >= b.limit;
            return (
              <div key={b.kind} style={{ display: "grid", gap: 3 }}>
                <div style={{ display: "flex", gap: 8, alignItems: "baseline" }}>
                  <span style={{ fontSize: 11, fontWeight: 600 }}>
                    {BUDGET_LABELS[b.kind] ?? b.kind}
                  </span>
                  <span style={{ ...mono, color: over ? "var(--red)" : "var(--muted)" }}>
                    {b.used} / {b.limit || "∞"}（{b.window}）
                  </span>
                  {b.resets_at ? <span style={muted}>重置 {b.resets_at}</span> : null}
                </div>
                <div
                  style={{
                    height: 5,
                    borderRadius: 3,
                    background: "var(--panel3)",
                    overflow: "hidden",
                  }}
                >
                  <div
                    style={{
                      width: `${pct}%`,
                      height: "100%",
                      background: over ? "var(--red)" : "var(--green)",
                    }}
                  />
                </div>
              </div>
            );
          })
        )}
      </section>
    </div>
  );
}
