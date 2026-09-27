"use client";

/**
 * CompetitionQueue：调度队列与准入原因（设计 14.2 右栏，COMP-10）。
 *
 * 每条队列条目显示状态、Operator 优先级、打分、not_before 与最近一次
 * AdmissionDecision（动作 / 原因 / 逐条规则结果），让"为什么没派发"
 * 可解释。数据来自 snapshot.queue（含 admission_decision 投影缓存）。
 */

import type { CSSProperties } from "react";
import type {
  CompetitionSnapshotView,
  QueueEntryView,
} from "@/lib/competition-events";

const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };
const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };

const ENTRY_STATE_LABELS: Record<string, { label: string; color: string }> = {
  queued: { label: "排队中", color: "var(--blue)" },
  dispatching: { label: "派发中", color: "var(--amber)" },
  held: { label: "已挂起", color: "var(--amber)" },
  done: { label: "已完成", color: "var(--green)" },
  dropped: { label: "已丢弃", color: "var(--muted)" },
};

const ACTION_LABELS: Record<string, string> = {
  admit: "准入",
  hold: "挂起",
  defer: "推迟",
};

interface RuleOutcome {
  rule?: string;
  passed?: boolean;
  reason?: string;
}

function admissionLines(entry: QueueEntryView): string[] {
  const decision = entry.admission_decision;
  if (!decision || typeof decision !== "object" || !Object.keys(decision).length) {
    return ["尚未产生准入决策（等待调度 tick）"];
  }
  const lines: string[] = [];
  const action = String(decision.action ?? "");
  const reason = String(decision.reason ?? "");
  if (action) {
    lines.push(
      `动作：${ACTION_LABELS[action] ?? action}${reason ? `，原因 ${reason}` : ""}`,
    );
  }
  if (decision.score != null) lines.push(`打分：${Number(decision.score)}`);
  const rules = Array.isArray(decision.rules)
    ? (decision.rules as RuleOutcome[])
    : [];
  for (const rule of rules) {
    if (rule && rule.passed === false) {
      lines.push(`✗ ${rule.rule ?? "?"}：${rule.reason ?? "未通过"}`);
    }
  }
  const passed = rules.filter((r) => r && r.passed !== false).length;
  if (rules.length) lines.push(`通过规则 ${passed}/${rules.length}`);
  return lines;
}

export function CompetitionQueue({
  snapshot,
}: {
  snapshot: CompetitionSnapshotView | null;
}) {
  const entries = [...(snapshot?.queue ?? [])].sort(
    (a, b) => b.priority - a.priority || b.score - a.score,
  );
  const nameOf = (challengeId: string) => {
    const row = snapshot?.challenges.find(
      (r) => r.challenge.challenge_id === challengeId,
    );
    return (
      row?.current_revision?.name || row?.challenge.name || challengeId
    );
  };

  if (!entries.length) {
    return <div style={muted}>队列为空。在题目表中选择题目加入队列。</div>;
  }

  return (
    <div style={{ display: "grid", gap: 6 }}>
      {entries.map((entry) => {
        const meta = ENTRY_STATE_LABELS[entry.state] ?? {
          label: entry.state,
          color: "var(--muted)",
        };
        return (
          <div
            key={entry.competition_challenge_id}
            style={{
              border: "1px solid var(--line)",
              borderRadius: 10,
              padding: "8px 10px",
              display: "grid",
              gap: 4,
              background: "var(--panel2)",
            }}
          >
            <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
              <span style={{ fontWeight: 700, fontSize: 12, color: "var(--bright)" }}>
                {nameOf(entry.competition_challenge_id)}
              </span>
              <span style={{ color: meta.color, fontSize: 11, fontWeight: 700 }}>
                {meta.label}
              </span>
              <span style={{ ...mono, color: "var(--muted)" }}>
                优先级 {entry.priority} · 分数 {entry.score}
              </span>
              {entry.not_before ? (
                <span style={muted}>不早于 {entry.not_before}</span>
              ) : null}
            </div>
            <div style={{ display: "grid", gap: 2 }}>
              {admissionLines(entry).map((line, i) => (
                <div key={i} style={{ ...muted, fontFamily: "var(--font-mono)" }}>
                  {line}
                </div>
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}
