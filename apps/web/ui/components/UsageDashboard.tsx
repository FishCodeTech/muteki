"use client";

import { useEffect, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import styles from "./UsageDashboard.module.css";

type Totals = { input_tokens: number | null; output_tokens: number | null; total_tokens: number | null; cache_read_tokens: number | null; cache_write_tokens: number | null; reasoning_tokens: number | null; records: number; quality: Record<string, number>; observed?: Record<string, number>; token_coverage?: string; reported_cost: number | null; estimated_cost: number | null; unpriced: number; unverified_estimates?: number };
type UsageRow = Partial<Totals> & { id: string; at: number; model?: string; role?: string; engine?: string; run_id?: string; thread_id?: string; challenge_id?: string; worker_id?: string; generation?: number; quality: string; status: string; cost_status?: string };
type Snapshot = { revision: number; as_of: number; totals: Totals; groups: Record<string, (Totals & { name: string })[]>; series: (Totals & { at: number })[]; records: UsageRow[]; count: number };
type ChallengeOption = { id: string; label: string };

type QuotaEntry = {
  credential_id: string;
  account_id: string;
  engine: string;
  label: string;
  quota_type: "subscription" | "api_key";
  status: "unknown" | "not_supported" | "ok" | string;
  remaining: number | null;
  total: number | null;
  window_seconds: number | null;
  reset_at: number | null;
  updated_at: number | null;
  unknown_reason: string | null;
};
type QuotaSnapshot = { quota: QuotaEntry[]; as_of: number };

const num = (n: number | null | undefined) => n == null ? "—" : n.toLocaleString();
const money = (n: number | null | undefined) => n == null ? "未定价" : `$${n.toFixed(5)}`;
const rowMoney = (row: UsageRow) => row.cost_status === "legacy_pi_estimate_unverified"
  ? "未定价"
  : row.cost_status === "legacy_estimate_unverified"
  ? (row.reported_cost != null && row.reported_cost > 0 ? money(row.reported_cost) : "未定价")
  : money(row.reported_cost ?? row.estimated_cost);
const rowMoneySource = (row: UsageRow) => row.cost_status === "legacy_pi_estimate_unverified"
  ? "历史 Pi 估算口径不明"
  : row.cost_status === "legacy_estimate_unverified"
  ? (row.reported_cost != null && row.reported_cost > 0 ? "上报 · 历史估算口径不明" : "历史估算口径不明")
  : row.reported_cost != null ? "上报" : row.estimated_cost != null ? "估算" : "未定价";
const quality: Record<string, string> = { reported: "已上报", estimated: "估算", partial: "部分缺失", missing: "未上报" };
const statusName: Record<string, string> = { observed: "已记录", failed: "失败", timeout: "超时", cancelled: "已取消", historical_quality_unknown: "历史口径不完整", counter_reset_unknown: "计数器重置", counter_reset_rebased: "计数器重置（已从零计）" };
const roleName: Record<string, string> = { worker: "Worker", assistant: "对话 Agent", reason: "Reason", titler: "标题 / 摘要", dispatch: "任务列表标题" };

function formatWindow(seconds: number | null | undefined): string {
  if (seconds == null) return "—";
  if (seconds < 120) return `${seconds} 秒`;
  if (seconds < 7200) return `${Math.round(seconds / 60)} 分钟`;
  if (seconds < 172800) return `${Math.round(seconds / 3600)} 小时`;
  return `${Math.round(seconds / 86400)} 天`;
}

function SubscriptionQuotaPanel({ stateRoot }: { stateRoot?: string }) {
  const [data, setData] = useState<QuotaSnapshot | null>(null);
  const [error, setError] = useState("");
  const [refreshing, setRefreshing] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  const load = async () => {
    try {
      const res = await apiFetch(`/api/usage/quota`);
      if (!res.ok) throw new Error(`额度读取失败（${res.status}）`);
      setData(await res.json() as QuotaSnapshot);
      setError("");
    } catch (e) {
      setError(e instanceof Error ? e.message : "额度读取失败");
    }
  };

  useEffect(() => { void load(); }, []);

  const refresh = async (credentialId: string) => {
    setRefreshing(credentialId);
    try {
      const res = await apiFetch(`/api/usage/quota/${encodeURIComponent(credentialId)}/refresh`, { method: "POST" });
      if (!res.ok) throw new Error(`刷新失败（${res.status}）`);
      await load();
    } catch {
    } finally {
      setRefreshing(null);
    }
  };

  const entries = data?.quota ?? [];
  const subscriptionEntries = entries.filter(e => e.quota_type === "subscription");
  const hasKnown = subscriptionEntries.some(e => e.status === "ok" && e.remaining != null);

  return (
    <div className={styles.quotaPanel}>
      <button
        className={styles.quotaToggle}
        onClick={() => setOpen(v => !v)}
        aria-expanded={open}
      >
        <span className={styles.eyebrow}>SUBSCRIPTION QUOTA</span>
        <span className={styles.quotaToggleLabel}>订阅额度{hasKnown ? " · 已知" : " · 未知"}</span>
        <span className={styles.quotaChevron}>{open ? "▲" : "▼"}</span>
      </button>
      {open && (
        <div className={styles.quotaBody}>
          <p className={styles.quotaNote}>
            订阅额度与 Token 用量分开统计。CLI 引擎不主动上报额度窗口，默认显示未知；引擎在会话中通过响应头上报后将自动更新。刷新只重读缓存，不启动 Agent 或消耗额度信用。
          </p>
          {error && <div role="alert" className={styles.error}>{error}</div>}
          {!data && !error && <p role="status" className={styles.quotaNote}>正在读取额度信息…</p>}
          {entries.length === 0 && data && (
            <p className={styles.quotaNote}>当前没有已配置的凭据账号。</p>
          )}
          <div className={styles.quotaTable}>
            {entries.map(entry => {
              const isRefreshing = refreshing === entry.credential_id;
              const statusLabel =
                entry.status === "ok" ? "已知" :
                entry.status === "not_supported" ? "不支持" :
                "未知";
              const statusData =
                entry.status === "ok" ? "ok" :
                entry.status === "not_supported" ? "unsupported" :
                "unknown";
              return (
                <div key={entry.credential_id} className={styles.quotaRow}>
                  <div className={styles.quotaRowHeader}>
                    <span className={styles.quotaLabel}>{entry.label}</span>
                    <span className={styles.quotaEngine}>{entry.engine}</span>
                    <span className={styles.quotaStatus} data-status={statusData}>{statusLabel}</span>
                    {entry.quota_type === "subscription" && (
                      <button
                        className={styles.quotaRefreshBtn}
                        disabled={isRefreshing}
                        onClick={() => void refresh(entry.credential_id)}
                        title="刷新额度（只重读缓存，不启动 Agent）"
                      >
                        {isRefreshing ? "…" : "↻"}
                      </button>
                    )}
                  </div>
                  {entry.status === "ok" ? (
                    <div className={styles.quotaDetails}>
                      <span>剩余 <b>{num(entry.remaining)}</b></span>
                      {entry.total != null && <span>总量 <b>{num(entry.total)}</b></span>}
                      {entry.window_seconds != null && <span>窗口 <b>{formatWindow(entry.window_seconds)}</b></span>}
                      {entry.reset_at != null && (
                        <span>重置时间 <b>{new Date(entry.reset_at * 1000).toLocaleString()}</b></span>
                      )}
                      {entry.updated_at != null && (
                        <span className={styles.quotaUpdated}>更新于 {new Date(entry.updated_at * 1000).toLocaleTimeString()}</span>
                      )}
                    </div>
                  ) : (
                    <div className={styles.quotaUnknown}>
                      {entry.unknown_reason || (entry.quota_type === "api_key" ? "API 密钥账号无订阅窗口" : "未知")}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

export function UsageDashboard({ runId, competitionId, threadId, challenges = [] }: { runId?: string; competitionId?: string; threadId?: string; challenges?: ChallengeOption[] }) {
  const scoped = Boolean(runId || competitionId || threadId);
  const scope = competitionId ? "competition" : runId ? "run" : threadId ? "thread" : "global";
  const [days, setDays] = useState(scoped ? "all" : "1");
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [generation, setGeneration] = useState("");
  const [model, setModel] = useState("");
  const [role, setRole] = useState("");
  const [challengeId, setChallengeId] = useState("");
  const [group, setGroup] = useState(competitionId ? "challenge_id" : "role");
  const [offset, setOffset] = useState(0);
  const [data, setData] = useState<Snapshot | null>(null);
  const [error, setError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const [importing, setImporting] = useState(false);
  const [importNote, setImportNote] = useState("");
  const importHistory = async () => {
    setImporting(true);
    try {
      const response = await apiFetch(`/api/usage/import-history`, { method: "POST" });
      if (!response.ok) throw new Error(`历史补录失败（${response.status}）`);
      const result = await response.json();
      setImportNote(`补录 ${result.imported} 条旧任务快照，跳过 ${result.skipped} 条异常记录。历史数据精度单独标记；重复补录不会重复计数。`);
      setRefresh(v => v + 1);
    } catch (e) { setImportNote(e instanceof Error ? e.message : "历史补录失败"); }
    finally { setImporting(false); }
  };
  useEffect(() => { setOffset(0); }, [days, startDate, endDate, workspace, generation, model, role, challengeId, runId, competitionId, threadId]);
  useEffect(() => {
    const controller = new AbortController();
    let busy = false;
    setData(null);
    const load = async () => {
      if (busy || controller.signal.aborted) return;
      busy = true;
      try {
        const query = new URLSearchParams({ limit: "50", offset: String(offset) });
        if (days === "custom") {
          if (startDate) query.set("start", String(new Date(`${startDate}T00:00:00`).getTime() / 1000));
          if (endDate) query.set("end", String(new Date(`${endDate}T23:59:59.999`).getTime() / 1000));
        } else if (days !== "all") {
          const start = new Date(); start.setHours(0, 0, 0, 0); start.setDate(start.getDate() - Number(days) + 1);
          query.set("start", String(start.getTime() / 1000));
        }
        for (const [key, value] of Object.entries({ run_id: runId, thread_id: threadId, competition_id: competitionId, challenge_id: challengeId, workspace_kind: workspace, generation, model, role })) if (value) query.set(key, value);
        const res = await apiFetch(`/api/usage?${query}`, { signal: controller.signal });
        if (!res.ok) throw new Error(`用量读取失败（${res.status}）`);
        const next = await res.json() as Snapshot;
        if (!controller.signal.aborted) { setData(next); setError(""); }
      } catch (e) { if (!controller.signal.aborted) setError(e instanceof Error ? e.message : "用量读取失败"); }
      finally { busy = false; }
    };
    void load();
    const timer = window.setInterval(() => { if (!document.hidden) void load(); }, 3000);
    return () => { controller.abort(); window.clearInterval(timer); };
  }, [days, startDate, endDate, workspace, generation, model, role, challengeId, runId, competitionId, threadId, offset, refresh]);
  const totals = data?.totals;
  const max = Math.max(1, ...(data?.series.map(row => row.total_tokens ?? 0) ?? []));
  const challengeNames = Object.fromEntries(challenges.map((item) => [item.id, item.label]));
  const labelGroup = (name: string) => group === "challenge_id" ? (challengeNames[name] || name) : (roleName[name] || name);
  return <section className={styles.root} data-scope={scope} aria-label="Token 用量统计">
    <header className={styles.header}><div><span className={styles.eyebrow}>USAGE</span><h1>{competitionId ? "比赛 Worker 用量" : runId ? "任务用量" : threadId ? "对话用量" : "全局用量"}</h1><p>{competitionId ? "仅本比赛 Worker，包含失败、中断和重试；不含 Reason、标题及摘要调用。" : "输入与输出构成总量，缓存和推理为其中明细。辅助模型调用同样计入。"}</p></div><button onClick={() => setRefresh(v => v + 1)}>刷新</button></header>
    {!scoped && <div className={styles.health}><button disabled={importing} onClick={() => void importHistory()}>{importing ? "正在补录…" : "补录旧任务用量"}</button><small>{importNote || "仅补录账本启用前的任务快照；对话历史不在此补录范围。"}</small></div>}
    {!scoped && <SubscriptionQuotaPanel />}
    <div className={styles.filters}>
      <label>时间<select value={days} onChange={e => setDays(e.target.value)}><option value="1">今日</option><option value="7">近 7 天</option><option value="30">近 30 天</option><option value="all">全部历史</option><option value="custom">自定义</option></select></label>
      {days === "custom" && <><label>开始日期<input type="date" value={startDate} onChange={e => setStartDate(e.target.value)} /></label><label>结束日期<input type="date" value={endDate} onChange={e => setEndDate(e.target.value)} /></label></>}
      {!scoped && <label>工作区<select value={workspace} onChange={e => setWorkspace(e.target.value)}><option value="">全部</option><option value="conversation">对话</option><option value="single-security-task">单任务</option><option value="competition">比赛</option></select></label>}
      {competitionId && <label>题目<select value={challengeId} onChange={e => setChallengeId(e.target.value)}><option value="">全部题目</option>{challenges.map((item) => <option key={item.id} value={item.id}>{item.label}</option>)}</select></label>}
      <label>模型<input value={model} placeholder="全部模型（精确匹配）" onChange={e => setModel(e.target.value)} /></label>
      <label>角色<select value={role} onChange={e => setRole(e.target.value)}><option value="">全部角色</option>{Object.entries(roleName).map(([key, title]) => <option key={key} value={key}>{title}</option>)}</select></label>
      {runId && <label>执行批次<input type="number" min="0" value={generation} placeholder="全部执行" onChange={e => setGeneration(e.target.value)} /></label>}
    </div>
    {error && <div role="alert" className={styles.error}>{error}。下方如有数据，为上次成功读取的结果。</div>}
    {!data && !error && <p role="status">正在读取用量账本…</p>}
    {totals && <>
      <div className={styles.cards}>{[
        ["已知总 Token", num(totals.total_tokens), `${num(totals.records)} 条调用 / 消息记录`],
        ["输入", num(totals.input_tokens), `缓存读取 ${num(totals.cache_read_tokens)} · 写入 ${num(totals.cache_write_tokens)}`],
        ["输出", num(totals.output_tokens), `其中推理 ${num(totals.reasoning_tokens)}（已知部分）`],
        ["上报金额", money(totals.reported_cost), "CLI / 服务返回值，不代表实际扣款"],
        ["估算金额", money(totals.estimated_cost), `${num(totals.unpriced)} 条未定价${totals.unverified_estimates ? `（${num(totals.unverified_estimates)} 条历史估算口径不明）` : ""} · 与上报金额分开统计`],
      ].map(([label, value, note]) => <div key={label} className={styles.card}><span>{label}</span><strong>{value}</strong><small>{note}</small></div>)}</div>
      <div className={styles.health}><div>{Object.entries(totals.quality).map(([key, value]) => <span key={key} data-warning={key !== "reported" && value > 0}>{quality[key]} <b>{value}</b></span>)}</div><small>每 3 秒刷新 · {new Date(data!.as_of * 1000).toLocaleTimeString()} · 缺失不是零</small></div>
      {totals.records === 0 ? <div className={styles.empty}><strong>这个范围还没有用量记录</strong><p>新调用上报后会自动显示。历史未记录的标题用量无法补回；运行中的 CLI 可能在调用结束后才上报。</p></div> : <>
        <div className={styles.panel}><h2>消耗趋势 <small>按小时汇总 · 本地时间</small></h2><div className={styles.chart} role="img" aria-label="每小时已知 Token 消耗柱状图">{data!.series.map(row => <div key={row.at} className={styles.barSlot} title={`${new Date(row.at * 1000).toLocaleString()}：${num(row.total_tokens)} Token`}><div className={styles.bar} style={{ height: `${Math.max(2, (row.total_tokens ?? 0) / max * 100)}%` }} /></div>)}</div><div className={styles.chartLabels}><span>{new Date(data!.series[0].at * 1000).toLocaleString()}</span><span>{new Date(data!.series[data!.series.length - 1].at * 1000).toLocaleString()}</span></div></div>
        <div className={`${styles.panel} ${styles.distributionPanel}`}><div className={styles.header}><h2>用量分布</h2><select aria-label="分组方式" value={group} onChange={e => setGroup(e.target.value)}>{[...(competitionId ? [["challenge_id","题目"]] : []),["role","角色"],["model","模型"],["engine","引擎"],["workspace_kind","工作区"],["worker_id","Worker"]].map(([v,l]) => <option key={v} value={v}>{l}</option>)}</select></div><div className={styles.tableScroll}><table><thead><tr><th>分组</th><th>输入</th><th>输出</th><th>总 Token</th><th>记录数</th></tr></thead><tbody>{data!.groups[group]?.map(row => <tr key={row.name}><td>{labelGroup(row.name)}</td><td>{num(row.input_tokens)}</td><td>{num(row.output_tokens)}</td><td>{num(row.total_tokens)}</td><td>{num(row.records)}</td></tr>)}</tbody></table></div></div>
        <div className={styles.panel}><h2>用量明细 <small>每条可观测调用 / 消息，不等同于供应商请求数</small></h2><div className={styles.tableScroll}><table><thead><tr><th>{competitionId ? "题目 / 时间" : "时间 / 归属"}</th><th>角色 / 模型</th><th>输入</th><th>输出</th><th>缓存读 / 写</th><th>推理</th><th>质量 / 状态</th><th>金额</th></tr></thead><tbody>{data!.records.map(row => <tr key={row.id}><td>{competitionId && row.challenge_id ? <strong className={styles.challengeName}>{challengeNames[row.challenge_id] || row.challenge_id}</strong> : null}{new Date(row.at * 1000).toLocaleString()}<small>{row.run_id ? <a href={`/run/${encodeURIComponent(row.run_id)}?view=usage`}>{row.run_id}</a> : row.thread_id || "前置调用"}{row.generation != null && ` · 第 ${row.generation} 代`}</small></td><td>{roleName[row.role || ""] || row.role}<small>{row.model || "模型未上报"}{row.worker_id ? ` · ${row.worker_id}` : ""}</small></td><td>{num(row.input_tokens)}</td><td>{num(row.output_tokens)}</td><td>{num(row.cache_read_tokens)} / {num(row.cache_write_tokens)}</td><td>{num(row.reasoning_tokens)}</td><td>{quality[row.quality]}<small>{statusName[row.status] || row.status}</small></td><td>{rowMoney(row)}<small>{rowMoneySource(row)}</small></td></tr>)}</tbody></table></div><div className={styles.pagination}><span>{data!.count} 条记录</span><button disabled={offset === 0} onClick={() => setOffset(v => Math.max(0,v-50))}>上一页</button><button disabled={offset + 50 >= data!.count} onClick={() => setOffset(v => v+50)}>下一页</button></div></div>
      </>}
      <p className={styles.footnote}>仅统计此 Muteki 实例归属明确的消费。历史补录未执行的记录不在此范围内；没有细分上报的字段用“—”显示。订阅和模型标价折算不代表实际账单。</p>
    </>}
  </section>;
}
