"use client";

/**
 * CompetitionBoard：比赛题目表（设计 14.2 右栏主表，COMP-10）。
 *
 * 列：类别、分值、远端解出（平台未提供时显示 —）、revision、远端状态、
 * 本地状态、优先级、队列状态、Run（链接 /run/[id]）、Worker（绑定状态 +
 * 执行代）、实例 TTL、提交状态。支持类别 / 状态筛选、关键字搜索、
 * 排序与批量选择（批量排队 / 批量跳过），逐题操作按状态机给出可用按钮。
 * 手动提交答案经 competition.submission.submit（带可选 witness 由后端
 * Gate 复核）。
 */

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import Link from "next/link";
import { Button, Checkbox, Input, ListBox, ListBoxItem, Modal, Select, TextField } from "@heroui/react";
import type {
  ChallengeSnapshotRow,
  CompetitionSnapshotView,
} from "@/lib/competition-events";

// ---- 样式（与 settings/agents 页同一 CSS 变量体系） ---------------------------

const th: CSSProperties = {
  textAlign: "left",
  fontSize: 12,
  fontWeight: 800,
  letterSpacing: "0.08em",
  textTransform: "uppercase",
  color: "var(--muted)",
  padding: "12px",
  borderBottom: "1px solid var(--line)",
  whiteSpace: "nowrap",
  position: "sticky",
  top: 0,
  background: "var(--panel)",
  zIndex: 1,
};
const td: CSSProperties = {
  padding: "14px 12px",
  borderBottom: "1px solid var(--line)",
  fontSize: 14,
  verticalAlign: "top",
};
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };
const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };
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
  whiteSpace: "nowrap",
};
const input: CSSProperties = {
  height: 26,
  padding: "0 8px",
  border: "1px solid var(--line2)",
  borderRadius: 7,
  background: "var(--panel2)",
  color: "var(--bright)",
  fontSize: 12,
  minWidth: 0,
};

const STATE_LABELS: Record<string, { label: string; color: string }> = {
  discovered: { label: "已发现", color: "var(--muted)" },
  selected: { label: "已选中", color: "var(--blue)" },
  queued: { label: "排队中", color: "var(--blue)" },
  provisioning: { label: "准备实例", color: "var(--amber)" },
  dispatching: { label: "派发中", color: "var(--amber)" },
  running: { label: "处理中", color: "var(--blue)" },
  solving: { label: "求解中", color: "var(--green)" },
  terminal_phase_waiting: { label: "最终阶段等待", color: "var(--blue)" },
  revisit_waiting: { label: "待复访", color: "var(--amber)" },
  candidate_found: { label: "已有候选", color: "var(--green)" },
  submitting: { label: "提交中", color: "var(--amber)" },
  solved: { label: "已解出", color: "var(--green)" },
  paused: { label: "已暂停", color: "var(--amber)" },
  skipped: { label: "已跳过", color: "var(--muted)" },
  exhausted: { label: "已耗尽", color: "var(--muted)" },
  failed: { label: "失败", color: "var(--red)" },
  retired: { label: "已退役", color: "var(--muted)" },
};

const REMOTE_LABELS: Record<string, string> = {
  open: "开放",
  closed: "关闭",
  solved_remote: "远端已解",
  hidden: "隐藏",
};

const SUBMISSION_LABELS: Record<string, { label: string; color: string }> = {
  queued: { label: "提交排队", color: "var(--blue)" },
  submitting: { label: "提交中", color: "var(--amber)" },
  correct: { label: "判对", color: "var(--green)" },
  wrong: { label: "判错", color: "var(--red)" },
  duplicate_or_solved: { label: "重复/已解", color: "var(--muted)" },
  rate_limited: { label: "限流冷却", color: "var(--amber)" },
  transient_failure: { label: "暂时失败", color: "var(--amber)" },
  auth_required: { label: "需要认证", color: "var(--red)" },
  unknown: { label: "结果未知", color: "var(--amber)" },
  cancelled: { label: "已取消", color: "var(--muted)" },
};

function stateBadge(state: string) {
  const meta = STATE_LABELS[state] ?? { label: state, color: "var(--muted)" };
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        height: 18,
        padding: "0 7px",
        borderRadius: 999,
        border: `1px solid color-mix(in srgb, ${meta.color} 34%, var(--line))`,
        background: `color-mix(in srgb, ${meta.color} 9%, transparent)`,
        color: meta.color,
        fontSize: 10.5,
        fontWeight: 700,
        whiteSpace: "nowrap",
      }}
    >
      {meta.label}
    </span>
  );
}

function displayChallengeState(row: ChallengeSnapshotRow): string {
  if (row.challenge.state !== "running") return row.challenge.state;
  if (row.queue_entry?.admission_decision?.reason === "terminal_phase_wait") {
    return "terminal_phase_waiting";
  }
  const bindingState = row.active_binding?.state;
  if (bindingState === "active") return "solving";
  if (["planned", "creating", "starting", "resolving"].includes(bindingState || "")) {
    return "dispatching";
  }
  if (bindingState === "paused") {
    return ["queued", "held"].includes(row.queue_entry?.state || "")
      ? "revisit_waiting"
      : "paused";
  }
  if (["queued", "held"].includes(row.queue_entry?.state || "")) {
    return "revisit_waiting";
  }
  return "running";
}

function leaseTtl(row: ChallengeSnapshotRow): string {
  const lease = row.active_lease;
  if (!lease) return "—";
  if (!lease.expires_at) return `${lease.ttl_seconds || "—"}s`;
  const left = Math.max(
    0,
    Math.floor((Date.parse(lease.expires_at) - Date.now()) / 1000),
  );
  const m = Math.floor(left / 60);
  const s = left % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

function submissionStatus(row: ChallengeSnapshotRow) {
  const newest = (items: typeof row.submissions) => [...items].sort((a, b) => {
    const timeOrder = Date.parse(b.updated_at || b.created_at || "")
      - Date.parse(a.updated_at || a.created_at || "");
    if (Number.isFinite(timeOrder) && timeOrder) return timeOrder;
    return b.attempt - a.attempt;
  })[0];
  const terminalSuccess = newest(row.submissions.filter(
    (item) => item.state === "correct" || item.state === "duplicate_or_solved",
  ));
  // 已解题目的最终平台结论是权威状态。历史判错仍保留在提交记录中，
  // 但不能覆盖之后的判对/已解回执；不同候选的 attempt 都可能从 1 开始，
  // 因此未解题目按更新时间选择当前结果。
  const latest = row.challenge.state === "solved" && terminalSuccess
    ? terminalSuccess
    : newest(row.submissions);
  if (!latest) {
    const waiting = row.candidates.some((c) => c.state === "awaiting_approval");
    return waiting
      ? { label: "待确认", color: "var(--amber)" }
      : { label: "—", color: "var(--muted)" };
  }
  return (
    SUBMISSION_LABELS[latest.state] ?? {
      label: latest.state,
      color: "var(--muted)",
    }
  );
}

type SortKey = "points" | "priority" | "state" | "name" | "category";

const BINDING_LABELS: Record<string, string> = {
  active: "活动中", paused: "已暂停", finished: "已结束", failed: "失败", cancelled: "已取消", stale: "已失效",
};
const QUEUE_LABELS: Record<string, string> = {
  queued: "排队中", dispatching: "派发中", held: "已暂缓", done: "已出队", dropped: "已移除",
};
function answerProgress(row: ChallengeSnapshotRow): string {
  const expected = row.current_revision?.expected_flags;
  if (!expected) return "—";
  const confirmed = new Set(row.submissions.filter((item) => item.state === "correct").map((item) => item.digest)).size;
  return `${confirmed} / ${expected}`;
}

export function CompetitionBoard({
  snapshot,
  busy,
  onQueue,
  onSelect,
  onSkip,
  onEnsureInstance,
  onPauseBinding,
  onResolveBinding,
  onManualOverride,
  onPauseMany,
  onRetryMany,
}: {
  snapshot: CompetitionSnapshotView | null;
  busy: boolean;
  onQueue: (ids: string[], priority?: number) => void;
  onSelect: (id: string) => void;
  onSkip: (id: string) => void;
  onEnsureInstance: (id: string) => void;
  onPauseBinding: (id: string) => void;
  onResolveBinding: (id: string) => void;
  onManualOverride: (id: string, answer: string, confirmation: string) => void;
  onPauseMany: (ids: string[]) => void;
  onRetryMany: (submissionIds: string[]) => void;
}) {
  const [category, setCategory] = useState("");
  const [stateFilter, setStateFilter] = useState("");
  const [query, setQuery] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("priority");
  const [sortAsc, setSortAsc] = useState(false);
  const [checked, setChecked] = useState<Record<string, boolean>>({});
  const [answerFor, setAnswerFor] = useState("");
  const [answerText, setAnswerText] = useState("");
  const [confirmationText, setConfirmationText] = useState("");
  const [overrideAcknowledged, setOverrideAcknowledged] = useState(false);
  const [bulkPriority, setBulkPriority] = useState(0);
  const preferencesReady = useRef(false);

  const preferenceKey = snapshot?.competition?.competition_id
    ? `muteki.competition.board.v1:${snapshot.competition.competition_id}`
    : "";

  useEffect(() => {
    preferencesReady.current = false;
    if (!preferenceKey) return;
    try {
      const saved = JSON.parse(localStorage.getItem(preferenceKey) || "{}") as {
        category?: string;
        stateFilter?: string;
        query?: string;
        sortKey?: SortKey;
        sortAsc?: boolean;
        bulkPriority?: number;
      };
      setCategory(saved.category || "");
      setStateFilter(saved.stateFilter || "");
      setQuery(saved.query || "");
      setSortKey(saved.sortKey || "priority");
      setSortAsc(Boolean(saved.sortAsc));
      setBulkPriority(Number(saved.bulkPriority || 0));
    } catch {
      localStorage.removeItem(preferenceKey);
    }
    preferencesReady.current = true;
  }, [preferenceKey]);

  useEffect(() => {
    if (!preferenceKey || !preferencesReady.current) return;
    localStorage.setItem(preferenceKey, JSON.stringify({
      category,
      stateFilter,
      query,
      sortKey,
      sortAsc,
      bulkPriority,
    }));
  }, [bulkPriority, category, preferenceKey, query, sortAsc, sortKey, stateFilter]);

  const rows = useMemo(() => snapshot?.challenges ?? [], [snapshot]);

  const categories = useMemo(() => {
    const set = new Set<string>();
    for (const r of rows) {
      const c = r.current_revision?.category || r.challenge.category;
      if (c) set.add(c);
    }
    return [...set].sort();
  }, [rows]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const list = rows.filter((r) => {
      const name = r.current_revision?.name || r.challenge.name;
      const cat = r.current_revision?.category || r.challenge.category;
      if (category && cat !== category) return false;
      if (stateFilter && displayChallengeState(r) !== stateFilter) return false;
      if (
        q &&
        !name.toLowerCase().includes(q) &&
        !r.challenge.external_challenge_id.toLowerCase().includes(q)
      ) {
        return false;
      }
      return true;
    });
    const value = (r: ChallengeSnapshotRow): string | number => {
      switch (sortKey) {
        case "points":
          return r.current_revision?.points ?? 0;
        case "priority":
          return r.queue_entry?.priority ?? 0;
        case "state":
          return displayChallengeState(r);
        case "category":
          return r.current_revision?.category || r.challenge.category;
        default:
          return r.current_revision?.name || r.challenge.name;
      }
    };
    return [...list].sort((a, b) => {
      const va = value(a);
      const vb = value(b);
      const cmp =
        typeof va === "number" && typeof vb === "number"
          ? va - vb
          : String(va).localeCompare(String(vb));
      return sortAsc ? cmp : -cmp;
    });
  }, [rows, category, stateFilter, query, sortKey, sortAsc]);

  const checkedIds = filtered
    .map((r) => r.challenge.challenge_id)
    .filter((id) => checked[id]);
  const checkedRows = rows.filter((row) => checked[row.challenge.challenge_id]);
  const pausableIds = checkedRows
    .filter((row) => ["running", "candidate_found"].includes(row.challenge.state))
    .map((row) => row.challenge.challenge_id);
  const retryableSubmissionIds = checkedRows.flatMap((row) => row.submissions
    .filter((submission) => ["rate_limited", "transient_failure", "auth_required"].includes(submission.state))
    .map((submission) => submission.submission_id));
  const selectedPoints = checkedRows.reduce(
    (sum, row) => sum + (row.current_revision?.points ?? 0),
    0,
  );
  const activeBindings = checkedRows.filter((row) => row.active_binding).length;
  const activeLeases = checkedRows.filter((row) => row.active_lease).length;
  const pendingSubmissions = checkedRows.reduce(
    (count, row) => count + row.submissions.filter(
      (submission) => ["queued", "submitting", "unknown"].includes(submission.state),
    ).length,
    0,
  );
  const overrideRow = rows.find(
    (row) => row.challenge.challenge_id === answerFor,
  );

  const toggleAll = (on: boolean) => {
    const next = { ...checked };
    for (const r of filtered) next[r.challenge.challenge_id] = on;
    setChecked(next);
  };

  const sortHeader = (key: SortKey, label: string) => (
    <th style={{ ...th, cursor: "pointer" }}
      onClick={() => {
        if (sortKey === key) setSortAsc(!sortAsc);
        else {
          setSortKey(key);
          setSortAsc(false);
        }
      }}
      data-tooltip="点击切换排序"
    >
      {label}
      {sortKey === key ? (sortAsc ? " ↑" : " ↓") : ""}
    </th>
  );

  return (
    <div className="competition-board">
      {/* 筛选 / 排序 / 批量操作 */}
      <div className="competition-board-caption"><div><h2>题目总览</h2><span>{filtered.length} / {rows.length} 道题目</span></div><span>题目、任务绑定与队列分别显示状态</span></div>
      <div className="competition-board-filters">
        <TextField aria-label="搜索题目" className="competition-search">
        <Input
          id="competition-challenge-search"
          style={{ ...input, width: "100%" }}
          placeholder="搜索题目…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        </TextField>
        <Select
          id="competition-category-filter"
          aria-label="题目类别"
          selectedKey={category}
          onSelectionChange={(key) => setCategory(String(key ?? ""))}
        >
          <Select.Trigger><Select.Value /></Select.Trigger>
          <Select.Popover><ListBox><ListBoxItem id="" textValue="全部类别">全部类别</ListBoxItem>{categories.map((c) => <ListBoxItem key={c} id={c} textValue={c}>{c}</ListBoxItem>)}</ListBox></Select.Popover>
        </Select>
        <Select
          id="competition-state-filter"
          aria-label="题目状态"
          selectedKey={stateFilter}
          onSelectionChange={(key) => setStateFilter(String(key ?? ""))}
        >
          <Select.Trigger><Select.Value /></Select.Trigger>
          <Select.Popover><ListBox><ListBoxItem id="" textValue="全部状态">全部状态</ListBoxItem>{Object.entries(STATE_LABELS).map(([k, v]) => <ListBoxItem key={k} id={k} textValue={v.label.trim()}>{v.label.trim()}</ListBoxItem>)}</ListBox></Select.Popover>
        </Select>
        {(query || category || stateFilter) ? <Button variant="ghost" onPress={() => { setQuery(""); setCategory(""); setStateFilter(""); }}>清除筛选</Button> : null}
      </div>
      {checkedIds.length > 0 ? <div className="competition-bulk-actions">
        <strong>已选 {checkedIds.length} 项</strong>
        <Button
          style={btn}
          isDisabled={busy || !checkedIds.length}
          onClick={() => onQueue(checkedIds, bulkPriority)}
        >
          批量排队（{checkedIds.length}）
        </Button>
        <label className="competition-inline-field" htmlFor="competition-bulk-priority">
          <span>批量优先级</span>
          <Input id="competition-bulk-priority" style={{ ...input, width: 76 }} type="number" value={bulkPriority} onChange={(event) => setBulkPriority(Number(event.target.value || 0))} />
        </label>
        <Button style={btn} isDisabled={busy || !pausableIds.length} onClick={() => onPauseMany(pausableIds)}>
          批量暂停（{pausableIds.length}）
        </Button>
        <Button style={btn} isDisabled={busy || !retryableSubmissionIds.length} onClick={() => onRetryMany(retryableSubmissionIds)}>
          批量重试提交（{retryableSubmissionIds.length}）
        </Button>
        <Button
          style={btn}
          isDisabled={busy || !checkedIds.length}
          onClick={() => {
            for (const id of checkedIds) onSkip(id);
          }}
        >
          批量跳过
        </Button>
        <Button variant="ghost" onPress={() => setChecked({})}>取消选择</Button>
      </div> : null}

      {checkedIds.length ? (
        <section className="competition-bulk-impact" aria-live="polite">
          <strong>批量操作影响预览</strong>
          <span>题目 {checkedIds.length} · 分值 {selectedPoints} · 活动 Run {activeBindings} · 活动实例 {activeLeases}</span>
          <span>待裁定提交 {pendingSubmissions} · 可重试提交 {retryableSubmissionIds.length} · 排队将发送 {checkedIds.length} 条独立命令</span>
          <span>暂停会停止对应 Run 的继续执行；重试会再次进入平台提交队列，并继续受平台限流约束。</span>
        </section>
      ) : null}

      <div className="competition-table-scroll" tabIndex={0} aria-label="题目表，可横向滚动">
        <table className="competition-board-table" style={{ borderCollapse: "collapse", width: "100%", minWidth: 900 }}>
          <thead>
            <tr>
              <th style={th}>
                <Checkbox aria-label="选择当前全部题目" isSelected={filtered.length > 0 && filtered.every((r) => checked[r.challenge.challenge_id])} isIndeterminate={filtered.some((r) => checked[r.challenge.challenge_id]) && !filtered.every((r) => checked[r.challenge.challenge_id])} onChange={toggleAll}><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control></Checkbox.Content></Checkbox>
              </th>
              {sortHeader("name", "题目")}
              {sortHeader("category", "类别")}
              {sortHeader("points", "分值")}
              <th style={th} title="按本地正确提交记录去重统计，非远端解题人数">本地确认答案</th>
              <th style={th}>远端状态</th>
              {sortHeader("state", "题目状态")}
              {sortHeader("priority", "队列 / 优先级")}
              <th style={th}>关联任务</th>
              <th style={th}>实例剩余时间</th>
              <th style={th}>提交</th>
              <th style={th}>操作</th>
            </tr>
          </thead>
          <tbody>
            {filtered.map((row) => {
              const id = row.challenge.challenge_id;
              const name =
                row.current_revision?.name ||
                row.challenge.name ||
                row.challenge.external_challenge_id;
              const state = row.challenge.state;
              const displayedState = displayChallengeState(row);
              const sub = submissionStatus(row);
              const binding = row.active_binding;
              const entry = row.queue_entry;
              return (
                  <tr key={id}>
                    <td style={td}>
                      <Checkbox aria-label={`选择 ${name}`} isSelected={Boolean(checked[id])} onChange={(selected) => setChecked({ ...checked, [id]: selected })}><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control></Checkbox.Content></Checkbox>
                    </td>
                    <td style={{ ...td, maxWidth: 220 }}>
                      <div style={{ fontWeight: 650, color: "var(--bright)" }}>
                        {name}
                      </div>
                      <div style={{ ...mono, color: "var(--muted)" }}>
                        {name !== row.challenge.external_challenge_id ? `${row.challenge.external_challenge_id} · ` : ""}
                        {row.current_revision ? `版本 ${row.current_revision.revision_seq}` : ""}
                        {row.challenge.tombstoned ? "（远端已删除）" : ""}
                      </div>
                    </td>
                    <td style={td}>
                      {row.current_revision?.category ||
                        row.challenge.category ||
                        "—"}
                    </td>
                    <td style={{ ...td, ...mono }}>
                      {row.current_revision?.points ?? "—"}
                    </td>
                    <td style={{ ...td, ...mono }} title="本地正确提交的不同答案数量 / 预期答案数量">{answerProgress(row)}</td>
                    <td style={td}>
                      {REMOTE_LABELS[row.challenge.remote_state] ??
                        row.challenge.remote_state}
                    </td>
                    <td style={td}>{stateBadge(displayedState)}</td>
                    <td style={td}>
                      {entry ? <><span>{QUEUE_LABELS[entry.state] || entry.state}</span><small className="competition-cell-note">优先级 {entry.priority}</small></> : "—"}
                    </td>
                    <td style={td}>
                      {binding ? (
                        <div style={{ display: "grid", gap: 2 }}>
                          <Link
                            href={`/run/${encodeURIComponent(binding.run_id)}`}
                            style={{ ...mono, color: "var(--blue)" }}
                          >
                            {binding.run_id.slice(0, 18)}…
                          </Link>
                          <span style={muted}>
                            {BINDING_LABELS[binding.state] || binding.state} · 第 {binding.execution_generation} 代
                          </span>
                          <small className="competition-cell-note">绑定状态{binding.updated_at ? ` · ${new Date(binding.updated_at).toLocaleTimeString()}` : ""}</small>
                        </div>
                      ) : (
                        <span style={muted}>—</span>
                      )}
                    </td>
                    <td style={{ ...td, ...mono }}>{leaseTtl(row)}</td>
                    <td style={td}>
                      <span style={{ color: sub.color, fontSize: 11, fontWeight: 650 }}>
                        {sub.label}
                      </span>
                    </td>
                    <td style={td}>
                      <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                        {(state === "discovered" || state === "selected") && (
                          <Button
                            style={btn}
                            isDisabled={busy}
                            onClick={() => onQueue([id])}
                          >
                            排队
                          </Button>
                        )}
                        {state === "discovered" && (
                          <Button
                            style={btn}
                            isDisabled={busy}
                            onClick={() => onSelect(id)}
                          >
                            选中
                          </Button>
                        )}
                        {(state === "queued" || state === "selected") && (
                          <Button
                            style={btn}
                            isDisabled={busy}
                            onClick={() => onEnsureInstance(id)}
                            aria-label="申请动态实例（instance.ensure）"
                          >
                            实例
                          </Button>
                        )}
                        {(state === "running" || state === "candidate_found") && (
                          <Button
                            style={btn}
                            isDisabled={busy}
                            onClick={() => onPauseBinding(id)}
                          >
                            暂停
                          </Button>
                        )}
                        {state === "paused" && binding && (
                          <Button
                            style={btn}
                            isDisabled={busy}
                            onClick={() => onResolveBinding(id)}
                          >
                            恢复
                          </Button>
                        )}
                        {state !== "solved" &&
                          state !== "retired" &&
                          !row.challenge.tombstoned && (
                            <>
                              <Button
                                style={btn}
                                isDisabled={busy}
                                onClick={() => {
                                  setAnswerFor(id);
                                  setAnswerText("");
                                  setConfirmationText("");
                                  setOverrideAcknowledged(false);
                                }}
                              >
                                人工覆盖
                              </Button>
                              {(state === "discovered" ||
                                state === "selected" ||
                                state === "queued" ||
                                state === "paused") && (
                                <Button
                                  style={btn}
                                  isDisabled={busy}
                                  onClick={() => onSkip(id)}
                                >
                                  跳过
                                </Button>
                              )}
                            </>
                          )}
                      </div>
                    </td>
                  </tr>
              );
            })}
            {!filtered.length && (
              <tr>
                <td colSpan={12} style={{ ...td, ...muted, textAlign: "center" }}>
                  {rows.length ? "没有符合筛选的题目" : "尚未同步到题目"}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="competition-mobile-cards">
        {filtered.map((row) => {
          const id = row.challenge.challenge_id;
          const state = row.challenge.state;
          const displayedState = displayChallengeState(row);
          const name = row.current_revision?.name || row.challenge.name || row.challenge.external_challenge_id;
          const submission = submissionStatus(row);
          return (
            <article key={id} className="competition-challenge-card">
              <header>
                <Checkbox aria-label={`选择 ${name}`} isSelected={Boolean(checked[id])} onChange={(selected) => setChecked({ ...checked, [id]: selected })}><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control></Checkbox.Content></Checkbox>
                <div><strong>{name}</strong><small>{row.current_revision?.category || row.challenge.category || "未分类"} · {row.current_revision?.points ?? "—"} 分</small></div>
                {stateBadge(displayedState)}
              </header>
              <dl>
                <dt>队列</dt><dd>{row.queue_entry ? `${QUEUE_LABELS[row.queue_entry.state] || row.queue_entry.state} · 优先级 ${row.queue_entry.priority}` : "未排队"}</dd>
                <dt>关联任务</dt><dd>{row.active_binding ? <Link href={`/run/${encodeURIComponent(row.active_binding.run_id)}`}>{row.active_binding.run_id.slice(0, 14)}… · 第 {row.active_binding.execution_generation} 代</Link> : "未绑定"}</dd>
                <dt>绑定状态</dt><dd>{row.active_binding ? BINDING_LABELS[row.active_binding.state] || row.active_binding.state : "—"}</dd>
                <dt>本地答案</dt><dd>{answerProgress(row)}</dd>
                <dt>实例</dt><dd>{leaseTtl(row)}</dd>
                <dt>提交</dt><dd style={{ color: submission.color }}>{submission.label}</dd>
              </dl>
              <div className="competition-card-actions">
                {(state === "discovered" || state === "selected") ? <Button type="button" isDisabled={busy} onClick={() => onQueue([id])}>排队</Button> : null}
                {state === "discovered" ? <Button type="button" isDisabled={busy} onClick={() => onSelect(id)}>选中</Button> : null}
                {(state === "queued" || state === "selected") ? <Button type="button" isDisabled={busy} onClick={() => onEnsureInstance(id)}>申请实例</Button> : null}
                {(state === "running" || state === "candidate_found") ? <Button type="button" isDisabled={busy} onClick={() => onPauseBinding(id)}>暂停</Button> : null}
                {state === "paused" && row.active_binding ? <Button type="button" isDisabled={busy} onClick={() => onResolveBinding(id)}>恢复</Button> : null}
                {state !== "solved" && state !== "retired" && !row.challenge.tombstoned ? (
                  <Button type="button" isDisabled={busy} onClick={() => {
                    setAnswerFor(id);
                    setAnswerText("");
                    setConfirmationText("");
                    setOverrideAcknowledged(false);
                  }}>人工覆盖</Button>
                ) : null}
                {["discovered", "selected", "queued", "paused"].includes(state) ? <Button type="button" isDisabled={busy} onClick={() => onSkip(id)}>跳过</Button> : null}
              </div>
            </article>
          );
        })}
      </div>
      {overrideRow ? <Modal isOpen onOpenChange={(open) => { if (!open) setAnswerFor(""); }}>
        <Modal.Backdrop className="competition-override-backdrop">
          <Modal.Container>
          <Modal.Dialog className="competition-override-dialog">
          <form
            noValidate
            aria-labelledby="competition-override-title"
            onSubmit={(event) => {
              event.preventDefault();
              if (
                !answerText.trim()
                || confirmationText !== answerText
                || !overrideAcknowledged
              ) return;
              onManualOverride(
                overrideRow.challenge.challenge_id,
                answerText.trim(),
                confirmationText,
              );
              setAnswerFor("");
            }}
          >
            <header>
              <div>
                <span>高影响操作</span>
                <h2 id="competition-override-title">人工覆盖提交</h2>
              </div>
              <Button type="button" onClick={() => setAnswerFor("")} aria-label="关闭人工覆盖">×</Button>
            </header>
            <p>
              题目：<strong>{overrideRow.current_revision?.name || overrideRow.challenge.name}</strong>。该入口会绕过 Run Gate 来源校验并直接创建远端提交，审计事件会记录操作人和摘要。
            </p>
            <label htmlFor="competition-override-answer">答案</label>
            <TextField><Input id="competition-override-answer" value={answerText} onChange={(event) => setAnswerText(event.target.value)} placeholder="flag{…}" autoFocus /></TextField>
            <label htmlFor="competition-override-confirm">再次输入答案</label>
            <TextField isInvalid={Boolean(confirmationText && confirmationText !== answerText)}><Input id="competition-override-confirm" value={confirmationText} onChange={(event) => setConfirmationText(event.target.value)} /></TextField>
            {confirmationText && confirmationText !== answerText ? <span className="field-error">两次输入不一致</span> : null}
            <Checkbox className="competition-override-ack" isSelected={overrideAcknowledged} onChange={setOverrideAcknowledged}><Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control>我确认该答案没有可核验的 Run witness，并了解它会产生真实平台副作用。</Checkbox.Content></Checkbox>
            <div>
              <Button type="button" onClick={() => setAnswerFor("")}>取消</Button>
              <Button className="danger" isDisabled={busy || !answerText.trim() || confirmationText !== answerText || !overrideAcknowledged}>确认人工提交</Button>
            </div>
          </form>
          </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal> : null}
    </div>
  );
}
