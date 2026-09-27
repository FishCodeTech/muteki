"use client";

/**
 * CompetitionWorkspace：比赛工作区主界面（任务书 COMP-10，设计 14.2）。
 *
 * 布局：顶部指标 + 操作条；主体为全宽数据面板（题目 / 时间线 / 队列 /
 * 资源 / 实例 / 提交）。平台连接与调度操作收进顶栏，避免三栏挤占题目表。
 *
 * 数据全部来自 useCompetition（COMP-09 SSE + snapshot），无固定假数据。
 * 子 Run 详情只展示链接（/run/[id]），其 SSE 由子 Run 页面自己订阅——
 * 比赛页只维护一条比赛 SSE。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Button, ListBox, ListBoxItem, Select, Tabs } from "@heroui/react";
import { useCompetition } from "@/lib/useCompetition";
import { toplineStats } from "@/lib/competition-events";
import { CompetitionBoard } from "./CompetitionBoard";
import { CompetitionQueue } from "./CompetitionQueue";
import { CompetitionResources } from "./CompetitionResources";
import { InstanceLeasePanel } from "./InstanceLeasePanel";
import { UsageDashboard } from "./UsageDashboard";
import { SubmissionLedger } from "./SubmissionLedger";

const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };

type RightTab = "usage" | "board" | "timeline" | "queue" | "resources" | "instances" | "submissions";

const RIGHT_TABS: { key: RightTab; label: string }[] = [
  { key: "usage", label: "用量" },
  { key: "board", label: "题目" },
  { key: "timeline", label: "时间线" },
  { key: "queue", label: "队列" },
  { key: "resources", label: "资源" },
  { key: "instances", label: "实例" },
  { key: "submissions", label: "提交" },
];

function TopStat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return <div className="competition-metric" title={hint ?? label}>
    <span>{label}</span><strong>{value}</strong>{hint ? <small>{hint}</small> : null}
  </div>;
}

function formatRemaining(seconds: number | null): string {
  if (seconds === null) return "--:--:--";
  const safe = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(safe / 3600);
  const minutes = Math.floor((safe % 3600) / 60);
  const rest = safe % 60;
  return [hours, minutes, rest].map((value) => String(value).padStart(2, "0")).join(":");
}

export function CompetitionWorkspace({
  competitionId,
  competitions,
}: {
  competitionId: string;
  competitions: { competition_id: string; title: string; external_competition_id: string }[];
}) {
  const router = useRouter();
  const {
    deck,
    connected,
    historyCards,
    historyLoading,
    historyHasMore,
    historyError,
    loadHistory,
    queueChallenges,
    selectChallenge,
    skipChallenge,
    ensureInstance,
    stopInstance,
    pauseBinding,
    resolveBinding,
    approveCandidate,
    retrySubmission,
    manualOverride,
    updatePolicy,
    schedulerControl,
    syncNow,
  } = useCompetition(competitionId);
  const [tab, setTab] = useState<RightTab>("board");
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState("");
  const [actionNotice, setActionNotice] = useState("");
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [resourceHistory, setResourceHistory] = useState<Array<{
    at: string;
    watermark: number;
    activeRuns: number;
    activeInstances: number;
    budgets: Record<string, { used: number; limit: number }>;
  }>>([]);
  const historyReady = useRef(false);

  useEffect(() => {
    if (tab === "timeline") void loadHistory(true);
  }, [tab, loadHistory]);

  useEffect(() => {
    const stored = localStorage.getItem(`muteki.competition.tab.v1:${competitionId}`);
    if (RIGHT_TABS.some((item) => item.key === stored)) setTab(stored as RightTab);
  }, [competitionId]);

  const selectTab = useCallback((nextTab: RightTab) => {
    localStorage.setItem(`muteki.competition.tab.v1:${competitionId}`, nextTab);
    setTab(nextTab);
  }, [competitionId]);

  useEffect(() => {
    historyReady.current = false;
    try {
      const value = JSON.parse(localStorage.getItem(`muteki.competition.resources.v1:${competitionId}`) || "[]");
      setResourceHistory(Array.isArray(value) ? value.slice(-100) : []);
    } catch {
      setResourceHistory([]);
    }
    historyReady.current = true;
  }, [competitionId]);

  const snapshot = deck.snapshot;
  const stats = useMemo(() => toplineStats(snapshot), [snapshot]);
  const usageChallenges = useMemo(() => snapshot?.challenges.map((row) => ({
    id: row.challenge.challenge_id,
    label: row.current_revision?.name || row.challenge.name || row.challenge.external_challenge_id || row.challenge.challenge_id,
  })) ?? [], [snapshot]);

  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (!snapshot || !historyReady.current) return;
    setResourceHistory((previous) => {
      if (previous.at(-1)?.watermark === snapshot.event_watermark) return previous;
      const next = [...previous, {
        at: snapshot.generated_at || new Date().toISOString(),
        watermark: snapshot.event_watermark,
        activeRuns: stats.runningBindings,
        activeInstances: stats.activeLeases,
        budgets: Object.fromEntries(snapshot.budgets.map((item) => [
          item.kind, { used: item.used, limit: item.limit },
        ])),
      }].slice(-100);
      localStorage.setItem(
        `muteki.competition.resources.v1:${competitionId}`,
        JSON.stringify(next),
      );
      return next;
    });
  }, [competitionId, snapshot, stats.activeLeases, stats.runningBindings]);

  const act = (fn: () => Promise<unknown> | unknown) => {
    setBusy(true);
    setActionError("");
    setActionNotice("");
    void Promise.resolve()
      .then(fn)
      .then((result) => {
        const receipt = result as { command_id?: string; state?: string; error?: { code?: string; message?: string } } | undefined;
        if (receipt?.error) throw new Error(`${receipt.error.code || "操作失败"}：${receipt.error.message || ""}`);
        setActionNotice(receipt?.command_id
          ? `命令 ${receipt.command_id} ${receipt.state === "completed" ? "已完成" : "已受理"}`
          : "操作已受理，状态将通过事件流更新");
      })
      .catch((exc) => setActionError(exc instanceof Error ? exc.message : String(exc)))
      .finally(() => setBusy(false));
  };

  const queueSmart = (ids: string[], priority = 0) =>
    act(async () => {
      const rows = snapshot?.challenges ?? [];
      for (const id of ids) {
        const row = rows.find((r) => r.challenge.challenge_id === id);
        if (row?.challenge.state === "discovered") {
          await selectChallenge(id);
        }
      }
      await queueChallenges(ids, priority);
    });

  const competition = snapshot?.competition;
  const connection = snapshot?.connection;
  const schedulerState = competition?.scheduler_state ?? "stopped";
  const endsAtMs = competition?.ends_at ? Date.parse(competition.ends_at) : Number.NaN;
  const remainingSeconds = Number.isFinite(endsAtMs)
    ? Math.max(0, Math.ceil((endsAtMs - nowMs) / 1000))
    : null;
  const platformStatus = competition?.platform_status ?? {};
  const vpnStatus = (
    typeof platformStatus.vpn === "object" && platformStatus.vpn !== null
      ? platformStatus.vpn as Record<string, unknown>
      : {}
  );
  const vpnState = String(vpnStatus.state || "");
  const budgetsText = snapshot?.budgets?.length
    ? snapshot.budgets
        .map((b) => `${({ submissions: "提交", tokens: "Token", cost: "费用", wallclock: "时长", instances: "实例" } as Record<string, string>)[b.kind] || b.kind} ${b.used}/${b.limit || "∞"}`)
        .join(" · ")
    : "—";

  return (
    <div className="competition-workspace">
      <header className="competition-heading">
        <div className="competition-heading-copy">
          <Link href="/competitions" className="competition-back">← 比赛列表</Link>
          <h1>{competition?.title || competition?.external_competition_id || "比赛"}</h1>
          <div className="competition-heading-status">
            <span className={`competition-live ${connected ? "connected" : ""}`}>
              <i />{connected ? "实时状态已连接" : "连接中断，正在重连"}
            </span>
            <span className="competition-scheduler-state">调度{({ running: "运行中", paused: "已暂停", stopped: "未启动" } as Record<string, string>)[schedulerState] || schedulerState}</span>
            {vpnState ? (
              <span className={`competition-vpn-state ${vpnState === "connected" ? "connected" : "bad"}`}>
                VPN {vpnState === "connected" ? "已连接" : "异常"}
              </span>
            ) : null}
            {snapshot?.generated_at ? <span>数据更新于 {new Date(snapshot.generated_at).toLocaleTimeString()}</span> : null}
          </div>
        </div>
        <div className="competition-heading-rail">
          <div className={`competition-countdown ${remainingSeconds === 0 ? "ended" : ""}`}>
            <span>测评剩余时间</span>
            <strong>{formatRemaining(remainingSeconds)}</strong>
            <small>{competition?.ends_at ? `结束于 ${new Date(competition.ends_at).toLocaleString()}` : "等待平台开始计时"}</small>
          </div>
          <div className="competition-heading-progress">
            <strong>{stats.solved}<span> / {snapshot?.challenges.length ?? 0}</span></strong>
            <span>已解出题目</span>
            <progress aria-label="已解出题目进度" value={stats.solved} max={snapshot?.challenges.length || 1} />
          </div>
        </div>
      </header>
      <section className="competition-metrics" aria-label="比赛概况">
        <TopStat label="得分" value={`${stats.points}`} hint="本地已解出题目合计" />
        <TopStat
          label="处理中题目"
          value={`${stats.active}`}
          hint={`正在求解 ${stats.runningBindings} · 最终阶段等待 ${stats.terminalPhaseWaiting} · 待复访 ${stats.revisitWaiting}`}
        />
        <TopStat label="排队题目" value={`${stats.queued}`} hint={`下一项：${stats.nextQueuedName || "—"}`} />
        <TopStat label="活动绑定" value={`${stats.runningBindings} / ${snapshot?.policy?.max_concurrent_runs ?? "—"}`} hint="绑定数量 / 并发上限" />
        <TopStat label="活动实例" value={`${stats.activeLeases}`} hint="当前持有的租约" />
        <TopStat label="预算使用" value={budgetsText} />
      </section>

      <div className="competition-toolbar">
        <div className="competition-toolbar-actions">
          <Button
            size="sm"
            variant="outline"
            isDisabled={busy || !snapshot}
            onPress={() => act(syncNow)}
          >
            同步比赛
          </Button>
          {schedulerState === "stopped" ? (
            <Button
              size="sm"
              variant="primary"
              isDisabled={busy || !snapshot}
              onPress={() => act(() => schedulerControl("start"))}
            >
              启动调度
            </Button>
          ) : null}
          {schedulerState === "running" ? (
            <Button
              size="sm"
              variant="outline"
              isDisabled={busy || !snapshot}
              onPress={() => act(() => schedulerControl("pause"))}
            >
              暂停调度
            </Button>
          ) : null}
          {schedulerState === "paused" ? (
            <Button
              size="sm"
              variant="primary"
              isDisabled={busy || !snapshot}
              onPress={() => act(() => schedulerControl("resume"))}
            >
              恢复调度
            </Button>
          ) : null}
        </div>

        {competitions.length > 1 ? (
          <label className="competition-toolbar-switch">
            <span>切换比赛</span>
            <Select
              aria-label="切换比赛"
              selectedKey={competitionId}
              onSelectionChange={(key) => {
                const next = String(key || "");
                if (next && next !== competitionId) {
                  router.push(`/competitions/${encodeURIComponent(next)}`);
                }
              }}
            >
              <Select.Trigger><Select.Value /></Select.Trigger>
              <Select.Popover>
                <ListBox>
                  {competitions.map((item) => (
                    <ListBoxItem
                      key={item.competition_id}
                      id={item.competition_id}
                      textValue={item.title || item.external_competition_id}
                    >
                      {item.title || item.external_competition_id}
                    </ListBoxItem>
                  ))}
                </ListBox>
              </Select.Popover>
            </Select>
          </label>
        ) : null}

        {connection ? (
          <div className="competition-toolbar-meta" title={connection.canonical_base_url}>
            <span>
              {connection.platform_kind} · {connection.account_key}
            </span>
            <span>
              {({ active: "连接可用", disabled: "已禁用", auth_required: "需要凭据", error: "连接异常" } as Record<string, string>)[connection.status] || connection.status}
              {connection.has_credential ? " · 已配置凭据" : " · 未配置凭据"}
            </span>
            <details className="competition-connection-detail">
              <summary>连接详情</summary>
              <dl><dt>平台地址</dt><dd>{connection.canonical_base_url}</dd>
                <dt>快照事件序号</dt><dd>{deck.watermark}</dd>
                <dt>已接收快照序号</dt><dd>{deck.lastSeq}</dd></dl>
            </details>
            {connection.last_error ? (
              <span className="bad">错误：{connection.last_error}</span>
            ) : null}
          </div>
        ) : (
          <div className="competition-toolbar-meta muted">等待平台连接…</div>
        )}
      </div>

      {deck.protocolError ? (
        <div
          style={{
            margin: "10px 16px 0",
            border: "1px solid color-mix(in srgb, var(--red) 40%, var(--line))",
            borderRadius: 10,
            background: "color-mix(in srgb, var(--red) 8%, var(--panel))",
            color: "var(--red)",
            padding: "8px 12px",
            fontSize: 12,
          }}
        >
          数据更新异常：{deck.protocolError}
        </div>
      ) : null}

      {actionError ? <div className="competition-action-feedback error" role="alert">{actionError}</div> : null}
      {actionNotice ? <div className="competition-action-feedback success" aria-live="polite">{actionNotice}</div> : null}

      {!snapshot ? (
        <div className="competition-loading" aria-busy="true">
          <span /><span /><span />
          <b>正在恢复比赛题目、回执、租约和提交状态…</b>
        </div>
      ) : (
        <div className="comp-grid">
          <section className="competition-data-panel" aria-label="比赛数据">
            <label className="competition-mobile-tab-select">
              <span>比赛数据视图</span>
              <Select
                aria-label="比赛数据视图"
                selectedKey={tab}
                onSelectionChange={(key) => selectTab(String(key) as RightTab)}
              >
                <Select.Trigger><Select.Value /></Select.Trigger>
                <Select.Popover><ListBox>{RIGHT_TABS.map((item) => (
                  <ListBoxItem key={item.key} id={item.key} textValue={item.label}>{item.label}</ListBoxItem>
                ))}</ListBox></Select.Popover>
              </Select>
            </label>
            <Tabs selectedKey={tab} onSelectionChange={(key) => selectTab(key as RightTab)} aria-label="比赛数据">
              <Tabs.List className="competition-tabs">
                {RIGHT_TABS.map((t) => <Tabs.Tab id={t.key} key={t.key}>
                  {t.label}{t.key === "board" ? <span className="competition-tab-count">{snapshot.challenges.length}</span> : null}
                </Tabs.Tab>)}
              </Tabs.List>
            </Tabs>
            {tab === "usage" && <UsageDashboard competitionId={competitionId} challenges={usageChallenges} />}
            {tab === "board" && (
              <CompetitionBoard
                snapshot={snapshot}
                busy={busy}
                onQueue={queueSmart}
                onSelect={(id) => act(() => selectChallenge(id))}
                onSkip={(id) => act(() => skipChallenge(id))}
                onEnsureInstance={(id) => act(() => ensureInstance(id))}
                onPauseBinding={(id) => act(() => pauseBinding(id))}
                onResolveBinding={(id) => act(() => resolveBinding(id))}
                onManualOverride={(id, answer, confirmation) => act(() => manualOverride(id, answer, confirmation))}
                onPauseMany={(ids) => act(async () => {
                  for (const id of ids) await pauseBinding(id);
                })}
                onRetryMany={(ids) => act(async () => {
                  for (const id of ids) await retrySubmission(id);
                })}
              />
            )}
            {tab === "timeline" && (
              <div className="competition-insights">
                <section>
                  <h2>比赛时间线</h2>
                  <p style={muted}>历史按需加载，不影响看板当前状态。</p>
                  <Button size="sm" variant="outline" isDisabled={historyLoading} onPress={() => void loadHistory(true)}>刷新历史</Button>
                  {historyError ? <p role="alert" className="bad">{historyError}</p> : null}
                  {[...deck.cards].reverse().concat(historyCards).map((item) => <article key={item.key}>
                    <strong>{item.title}</strong>
                    <span>{item.kind} · {item.at ? new Date(item.at).toLocaleString() : `事件 ${item.seq}`}</span>
                    {item.lines.map((line) => <small key={line}>{line}</small>)}
                  </article>)}
                  {!deck.cards.length && !historyCards.length ? <p style={muted}>{historyLoading ? "正在加载历史…" : "当前页没有可展示事件。"}</p> : null}
                  {historyHasMore ? <Button size="sm" variant="outline" isDisabled={historyLoading} onPress={() => void loadHistory()}>{historyLoading ? "加载中…" : "加载更早记录"}</Button> : null}
                </section>
                <section>
                  <h2>题目变更摘要</h2>
                  <dl>
                    <dt>题目</dt><dd>{snapshot.challenges.length || 0}</dd>
                    <dt>题目版本总数</dt><dd>{snapshot.challenges.reduce((sum, row) => sum + (row.current_revision?.revision_seq || 0), 0) || 0}</dd>
                    <dt>远端隐藏或删除</dt><dd>{snapshot.challenges.filter((row) => row.challenge.tombstoned || row.challenge.remote_state === "hidden").length || 0}</dd>
                    <dt>已加载变更事件</dt><dd>{historyCards.filter((item) => ["sync", "challenge"].includes(item.kind)).length}</dd>
                  </dl>
                </section>
                <section>
                  <h2>运行资源趋势</h2>
                  <div className="competition-table-scroll" tabIndex={0}><table><thead><tr><th>时间</th><th>事件序号</th><th>任务绑定</th><th>实例</th><th>预算</th></tr></thead><tbody>{resourceHistory.slice(-12).reverse().map((point) => <tr key={`${point.watermark}:${point.at}`}><td>{new Date(point.at).toLocaleTimeString()}</td><td>{point.watermark}</td><td>{point.activeRuns}</td><td>{point.activeInstances}</td><td>{Object.entries(point.budgets).map(([kind, value]) => `${kind} ${value.used}/${value.limit || "∞"}`).join(" · ") || "—"}</td></tr>)}</tbody></table></div>
                </section>
                <section>
                  <h2>提交结果统计</h2>
                  <dl>{Object.entries(snapshot.challenges.flatMap((row) => row.submissions).reduce<Record<string, number>>((counts, submission) => ({ ...counts, [submission.state]: (counts[submission.state] || 0) + 1 }), {})).map(([state, count]) => <div key={state}><dt>{state}</dt><dd>{count}</dd></div>)}</dl>
                  {!snapshot.challenges.some((row) => row.submissions.length) ? <p style={muted}>当前没有提交记录。</p> : null}
                </section>
              </div>
            )}
            {tab === "queue" && <CompetitionQueue snapshot={snapshot} />}
            {tab === "resources" && (
              <CompetitionResources
                snapshot={snapshot}
                busy={busy}
                onUpdatePolicy={(changes) => act(() => updatePolicy(changes))}
                onScheduler={(a) => act(() => schedulerControl(a))}
              />
            )}
            {tab === "instances" && (
              <InstanceLeasePanel
                snapshot={snapshot}
                busy={busy}
                onStop={(leaseId) => act(() => stopInstance(leaseId))}
              />
            )}
            {tab === "submissions" && (
              <SubmissionLedger
                snapshot={snapshot}
                busy={busy}
                onApprove={(cid) => act(() => approveCandidate(cid))}
                onRetry={(sid) => act(() => retrySubmission(sid))}
              />
            )}
          </section>
        </div>
      )}
    </div>
  );
}
