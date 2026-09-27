"use client";

import { Button } from "@heroui/react";

/**
 * SubmissionLedger：提交候选与远端提交台账（任务书 10.7，设计 12/14.2）。
 *
 * - 候选：digest 摘要（绝无原文）、Gate 判定、来源 Run、状态；assisted
 *   档的 awaiting_approval 候选显示「确认提交」按钮
 *   （platform_submission.approve，不引入 UI 本地隐藏状态）。
 * - 提交：attempt、状态机投影（含 rate_limited 冷却倒计时、判错持久
 *   否决、unknown 禁止盲目重试）、远端回执摘要与失败分类；可重试状态
 *   （rate_limited / transient_failure / auth_required）显示重试按钮
 *   （platform_submission.retry）。
 */

import { useEffect, useState } from "react";
import type { CSSProperties } from "react";
import Link from "next/link";
import type {
  CandidateView,
  CompetitionSnapshotView,
  SubmissionView,
} from "@/lib/competition-events";

const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };
const btn: CSSProperties = {
  height: 24,
  padding: "0 8px",
  border: "1px solid var(--line2)",
  borderRadius: 7,
  background: "var(--panel2)",
  color: "var(--text)",
  fontSize: 11,
  fontWeight: 650,
  cursor: "pointer",
};
const section: CSSProperties = { display: "grid", gap: 6 };

const CANDIDATE_LABELS: Record<string, { label: string; color: string }> = {
  candidate: { label: "候选", color: "var(--blue)" },
  awaiting_approval: { label: "待确认", color: "var(--amber)" },
  approved: { label: "已批准", color: "var(--green)" },
  submitted: { label: "已提交", color: "var(--green)" },
  rejected: { label: "已否决", color: "var(--red)" },
  superseded: { label: "被取代", color: "var(--muted)" },
  cancelled: { label: "已取消", color: "var(--muted)" },
};

const SUBMISSION_LABELS: Record<string, { label: string; color: string }> = {
  queued: { label: "排队", color: "var(--blue)" },
  submitting: { label: "提交中", color: "var(--amber)" },
  correct: { label: "判对", color: "var(--green)" },
  wrong: { label: "判错（持久否决）", color: "var(--red)" },
  duplicate_or_solved: { label: "重复/已解", color: "var(--muted)" },
  rate_limited: { label: "限流冷却", color: "var(--amber)" },
  transient_failure: { label: "暂时失败", color: "var(--amber)" },
  auth_required: { label: "需要认证", color: "var(--red)" },
  unknown: { label: "结果未知（禁止盲目重试）", color: "var(--amber)" },
  cancelled: { label: "已取消", color: "var(--muted)" },
};

const RETRYABLE = new Set(["rate_limited", "transient_failure", "auth_required"]);

export function SubmissionLedger({
  snapshot,
  busy,
  onApprove,
  onRetry,
}: {
  snapshot: CompetitionSnapshotView | null;
  busy: boolean;
  onApprove: (candidateId: string) => void;
  onRetry: (submissionId: string) => void;
}) {
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, []);

  const nameOf = (challengeId: string) => {
    const row = snapshot?.challenges.find(
      (r) => r.challenge.challenge_id === challengeId,
    );
    return row?.current_revision?.name || row?.challenge.name || challengeId;
  };

  const candidates: CandidateView[] = [];
  const submissions: SubmissionView[] = [];
  for (const row of snapshot?.challenges ?? []) {
    candidates.push(...row.candidates);
    submissions.push(...row.submissions);
  }
  candidates.sort((a, b) => String(b.created_at).localeCompare(String(a.created_at)));
  submissions.sort((a, b) => String(b.created_at).localeCompare(String(a.created_at)));

  return (
    <div style={{ display: "grid", gap: 14 }}>
      <section style={section}>
        <strong style={{ fontSize: 12 }}>提交候选（{candidates.length}）</strong>
        {!candidates.length ? (
          <div style={muted}>尚无候选。候选来自子 Run 经 Gate 确认的真实来源。</div>
        ) : (
          candidates.map((c) => {
            const meta = CANDIDATE_LABELS[c.state] ?? {
              label: c.state,
              color: "var(--muted)",
            };
            const approvable =
              (c.state === "awaiting_approval" || c.state === "candidate")
              && c.source_ref !== "operator_override"
              && Boolean(c.source_run_id && c.witness_digest);
            const verified = c.source_ref !== "operator_override"
              && Boolean(c.source_run_id && c.witness_digest);
            return (
              <div
                key={c.candidate_id}
                style={{
                  border: "1px solid var(--line)",
                  borderRadius: 10,
                  padding: "8px 10px",
                  display: "grid",
                  gap: 3,
                  background: "var(--panel2)",
                }}
              >
                <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
                  <span style={{ fontWeight: 700, fontSize: 12, color: "var(--bright)" }}>
                    {nameOf(c.competition_challenge_id)}
                  </span>
                  <span style={{ color: meta.color, fontSize: 11, fontWeight: 700 }}>
                    {meta.label}
                  </span>
                  <span style={{ color: verified ? "var(--green)" : "var(--amber)", fontSize: 11, fontWeight: 700 }}>
                    {verified ? "Gate 证据已核验" : "人工覆盖 · 未核验"}
                  </span>
                  <span style={{ ...mono, color: "var(--muted)" }}>
                    digest {c.digest.slice(0, 12)}… · 槽位 {c.answer_slot}
                  </span>
                </div>
                <div style={{ display: "flex", gap: 12, flexWrap: "wrap", ...muted }}>
                  <span>Gate：{c.gate_verdict || "—"}</span>
                  {c.witness_digest ? <span style={mono}>witness {c.witness_digest.slice(0, 12)}…</span> : null}
                  {c.source_execution_generation ? <span>执行代 {c.source_execution_generation}</span> : null}
                  {c.source_worker_id ? <span>Worker {c.source_worker_id}</span> : null}
                  {c.source_session_id ? <span>会话 {c.source_session_id}</span> : null}
                  {c.source_run_id ? (
                    <Link
                      href={`/run/${encodeURIComponent(c.source_run_id)}`}
                      style={{ ...mono, color: "var(--blue)" }}
                    >
                      来源 Run {c.source_run_id.slice(0, 16)}…
                    </Link>
                  ) : (
                    <span>{c.source_ref || "手动登记"}</span>
                  )}
                </div>
                {approvable && (
                  <div>
                    <Button
                      style={{
                        ...btn,
                        borderColor:
                          "color-mix(in srgb, var(--green) 50%, var(--line2))",
                        color: "var(--green)",
                      }}
                      isDisabled={busy}
                      onClick={() => onApprove(c.candidate_id)}
                      data-tooltip="platform_submission.approve：批准后立即入队提交"
                    >
                      确认提交
                    </Button>
                  </div>
                )}
                {!verified && c.state !== "submitted" ? (
                  <div style={{ ...muted, color: "var(--amber)" }}>
                    未核验候选没有普通批准按钮；远端提交只能来自已确认的人工覆盖命令。
                  </div>
                ) : null}
              </div>
            );
          })
        )}
      </section>

      <section style={section}>
        <strong style={{ fontSize: 12 }}>远端提交（{submissions.length}）</strong>
        {!submissions.length ? (
          <div style={muted}>尚无远端提交。</div>
        ) : (
          submissions.map((s) => {
            const meta = SUBMISSION_LABELS[s.state] ?? {
              label: s.state,
              color: "var(--muted)",
            };
            const cooldownLeft =
              s.state === "rate_limited" && s.retry_after_at
                ? Math.max(
                    0,
                    Math.floor((Date.parse(s.retry_after_at) - now) / 1000),
                  )
                : 0;
            return (
              <div
                key={s.submission_id}
                style={{
                  border: "1px solid var(--line)",
                  borderRadius: 10,
                  padding: "8px 10px",
                  display: "grid",
                  gap: 3,
                  background: "var(--panel2)",
                }}
              >
                <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
                  <span style={{ fontWeight: 700, fontSize: 12, color: "var(--bright)" }}>
                    {nameOf(s.competition_challenge_id)}
                  </span>
                  <span style={{ color: meta.color, fontSize: 11, fontWeight: 700 }}>
                    {meta.label}
                  </span>
                  <span style={{ ...mono, color: "var(--muted)" }}>
                    第 {s.attempt} 次 · 槽位 {s.answer_slot}
                  </span>
                  {cooldownLeft > 0 ? (
                    <span style={{ ...mono, color: "var(--amber)" }}>
                      冷却 {cooldownLeft}s
                    </span>
                  ) : null}
                </div>
                {s.remote_receipt ? (
                  <div style={{ ...mono, color: "var(--muted)" }}>
                    远端回执：{s.remote_receipt}
                  </div>
                ) : null}
                {s.last_error ? (
                  <div style={{ ...muted, color: "var(--red)" }}>
                    失败分类：{s.last_error}
                  </div>
                ) : null}
                {RETRYABLE.has(s.state) && cooldownLeft === 0 && (
                  <div>
                    <Button
                      style={btn}
                      isDisabled={busy}
                      onClick={() => onRetry(s.submission_id)}
                    >
                      重试提交
                    </Button>
                  </div>
                )}
              </div>
            );
          })
        )}
      </section>
    </div>
  );
}
