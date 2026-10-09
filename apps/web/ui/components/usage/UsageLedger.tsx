"use client";

import { useEffect, useState } from "react";
import { Button } from "@/components/chat/ui/Button";
import { Callout } from "@/components/chat/ui/Feedback";
import { apiFetch } from "@/lib/serviceAuth";
import { countValue, exactMoney, finiteValue, moneyValue } from "@/lib/providerUsage";
import styles from "../UsageDashboard.module.css";

type LedgerTotals = { historical_scope_unknown?: number | null; input_tokens: number | null; output_tokens: number | null; total_tokens: number | null; cache_read_tokens: number | null; cache_write_tokens: number | null; reasoning_tokens: number | null; records: number; reported_cost: number | null; estimated_cost: number | null };
type LedgerRow = Partial<LedgerTotals> & { id: string; at: number; model?: string; role?: string; engine?: string; run_id?: string; thread_id?: string; challenge_id?: string; worker_id?: string; generation?: number; quality: string; status: string; cost_status?: string; measurement_scope?: string; normalization_repair?: string; historical_observation?: Record<string, unknown> };
type LedgerSnapshot = { as_of: number; totals?: LedgerTotals; records: LedgerRow[]; count: number; groups: Record<string, (LedgerTotals & { name: string })[]> };
const QUALITY: Record<string, string> = { reported: "已上报", estimated: "估算", partial: "部分缺失", missing: "未上报" };
const STATUS: Record<string, string> = { observed: "已记录", failed: "失败", timeout: "超时", cancelled: "已取消", historical_quality_unknown: "历史口径不完整", counter_reset_unknown: "计数器重置", counter_reset_rebased: "计数器重置（已从零计）" };
export const USAGE_ROLES: Record<string, string> = { worker: "Worker", assistant: "对话 Agent", reason: "Reason", titler: "标题 / 摘要", dispatch: "任务列表标题" };

function historicalScopeUnknown(row: LedgerRow) {
  return row.measurement_scope === "historical_scope_unknown" || row.normalization_repair === "droid_historical_scope_unknown";
}

function HistoricalObservation({ row }: { row: LedgerRow }) {
  const observation = row.historical_observation;
  if (!observation || typeof observation !== "object" || Array.isArray(observation)) return null;
  const fields = [
    ["input_tokens", "原始输入"], ["output_tokens", "原始输出"], ["total_tokens", "原始总量"],
    ["cache_read_tokens", "原始缓存读取"], ["cache_write_tokens", "原始缓存写入"], ["reasoning_tokens", "原始推理"],
    ["reported_cost", "原始上报金额"], ["estimated_cost", "原始估算金额"],
  ];
  const observed = fields.filter(([key]) => finiteValue(observation[key]) !== null);
  if (!observed.length) return null;
  return <details className={styles.historicalObservation}><summary>查看原始观测</summary><div><p>旧记录的累计或增量口径无法确认。以下仅供追溯，不参与 Token 和费用合计。</p><dl>{observed.map(([key, label]) => <div key={key}><dt>{label}</dt><dd>{key.endsWith("cost") ? exactMoney(observation[key]) : countValue(observation[key])}</dd></div>)}</dl></div></details>;
}

function rowCost(row: LedgerRow) {
  if (historicalScopeUnknown(row)) return { value: null, label: "历史口径未知" };
  if (row.cost_status === "legacy_pi_estimate_unverified") return { value: null, label: "历史 Pi 估算口径不明" };
  if (row.cost_status === "legacy_estimate_unverified") return { value: finiteValue(row.reported_cost) !== null && row.reported_cost! > 0 ? row.reported_cost : null, label: "历史估算口径不明" };
  return { value: finiteValue(row.reported_cost) ?? finiteValue(row.estimated_cost), label: finiteValue(row.reported_cost) !== null ? "上报" : finiteValue(row.estimated_cost) !== null ? "估算" : "未定价" };
}

export function UsageLedger({ query, refresh, competitionId, challenges, allowImport, onImport }: {
  query: string; refresh: number; competitionId?: string; challenges: { id: string; label: string }[]; allowImport: boolean; onImport: () => void;
}) {
  const [offset, setOffset] = useState(0);
  const [group, setGroup] = useState(competitionId ? "challenge_id" : "role");
  const [data, setData] = useState<LedgerSnapshot | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [importing, setImporting] = useState(false);
  const [importNote, setImportNote] = useState("");
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError("");
    const params = new URLSearchParams(query);
    ["scope", "days", "tz", "refresh"].forEach(key => params.delete(key));
    params.set("limit", "50"); params.set("offset", String(offset));
    void (async () => {
      try {
        const response = await apiFetch(`/api/usage?${params}`, { signal: controller.signal });
        if (!response.ok) throw new Error(`明细读取失败（${response.status}）`);
        const result = await response.json() as LedgerSnapshot;
        if (!Array.isArray(result.records) || finiteValue(result.count) === null || !result.groups) throw new Error("用量明细返回的数据无效");
        if (!controller.signal.aborted) setData(result);
      } catch (cause) { if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : "明细读取失败"); }
      finally { if (!controller.signal.aborted) setLoading(false); }
    })();
    return () => controller.abort();
  }, [query, offset, refresh]);
  const importHistory = async () => {
    setImporting(true);
    setImportNote("");
    try {
      const response = await apiFetch("/api/usage/import-history", { method: "POST" });
      if (!response.ok) throw new Error(`历史补录失败（${response.status}）`);
      const result = await response.json();
      setImportNote(`补录 ${countValue(result.imported)} 条旧任务快照，跳过 ${countValue(result.skipped)} 条异常记录。重复补录不会重复计数。`);
      onImport();
    } catch (cause) { setImportNote(cause instanceof Error ? cause.message : "历史补录失败"); }
    finally { setImporting(false); }
  };
  const challengeNames = Object.fromEntries(challenges.map(item => [item.id, item.label]));
  const pageHistoricalCount = data?.records.filter(historicalScopeUnknown).length ?? 0;
  const historicalCount = finiteValue(data?.totals?.historical_scope_unknown);
  const groupName = (name: string) => group === "challenge_id" ? challengeNames[name] || name : USAGE_ROLES[name] || name;
  return <div className={styles.ledger}>
    {allowImport ? <div className={styles.importHistory}><Button size="sm" variant="outline" loading={importing} onClick={() => void importHistory()}>补录旧任务用量</Button><span>{importNote || "仅补录账本启用前的任务快照，不含对话和外部 CLI 历史。"}</span></div> : null}
    {error ? <Callout tone="danger" role="alert">{error}</Callout> : null}
    {loading ? <p className={styles.loading} role="status">正在读取用量明细…</p> : null}
    {data && !loading && !error ? <>
      {(historicalCount ?? pageHistoricalCount) > 0 ? <Callout tone="warning" role="status">{historicalCount !== null ? `当前范围有 ${countValue(historicalCount)} 条` : `本页有 ${pageHistoricalCount} 条`}记录历史口径未知，已排除其 Token 和费用数值。原始观测可在对应行展开查看。</Callout> : null}
      <div className={styles.sectionHeading}><h3>归属分布</h3><label className={styles.inlineField}><span className={styles.srOnly}>明细分组</span><select value={group} onChange={event => setGroup(event.target.value)}>{[...(competitionId ? [["challenge_id", "题目"]] : []), ["role", "角色"], ["model", "模型"], ["engine", "引擎"], ["workspace_kind", "工作区"], ["worker_id", "Worker"]].map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label></div>
      <div className={styles.tableScroll}><table><thead><tr><th>分组</th><th>输入</th><th>输出</th><th>总 Token</th><th>记录数</th></tr></thead><tbody>{data.groups[group]?.map(row => <tr key={row.name}><th scope="row">{groupName(row.name)}{finiteValue(row.historical_scope_unknown) !== null && row.historical_scope_unknown! > 0 ? <small className={styles.historicalGroupNote}>含 {countValue(row.historical_scope_unknown)} 条历史口径未知</small> : null}</th><td>{countValue(row.input_tokens)}</td><td>{countValue(row.output_tokens)}</td><td>{countValue(row.total_tokens)}</td><td>{countValue(row.records)}</td></tr>)}</tbody></table></div>
      <div className={styles.sectionHeading}><h3>调用明细</h3><span>每条可观测调用 / 消息，不等同于供应商请求数</span></div>
      {data.records.length ? <div className={styles.tableScroll}><table><thead><tr><th>{competitionId ? "题目 / 时间" : "时间 / 归属"}</th><th>角色 / 模型</th><th>输入</th><th>输出</th><th>缓存读 / 写</th><th>推理</th><th>质量 / 状态</th><th>金额</th></tr></thead><tbody>{data.records.map(row => {
        const historical = historicalScopeUnknown(row);
        const cost = rowCost(row);
        return <tr key={row.id}><td>{competitionId && row.challenge_id ? <strong>{challengeNames[row.challenge_id] || row.challenge_id}</strong> : null}{finiteValue(row.at) === null ? "时间未知" : new Date(row.at * 1000).toLocaleString()}<small>{row.run_id ? <a href={`/run/${encodeURIComponent(row.run_id)}?view=usage`}>{row.run_id}</a> : row.thread_id || "前置调用"}{finiteValue(row.generation) !== null ? ` · 第 ${row.generation} 代` : ""}</small></td><td>{USAGE_ROLES[row.role || ""] || row.role}<small>{row.model || "模型未上报"}{row.worker_id ? ` · ${row.worker_id}` : ""}</small></td><td>{countValue(historical ? null : row.input_tokens)}</td><td>{countValue(historical ? null : row.output_tokens)}</td><td>{countValue(historical ? null : row.cache_read_tokens)} / {countValue(historical ? null : row.cache_write_tokens)}</td><td>{countValue(historical ? null : row.reasoning_tokens)}</td><td>{historical ? <span className={styles.historicalStatus}>历史口径未知</span> : QUALITY[row.quality] || row.quality}<small>{STATUS[row.status] || row.status}</small>{historical ? <HistoricalObservation row={row} /> : null}</td><td title={exactMoney(cost.value)}>{moneyValue(cost.value)}<small>{cost.label}</small></td></tr>;
      })}</tbody></table></div> : <p className={styles.loading}>当前筛选条件下没有明细。</p>}
      <div className={styles.pagination}><span>{countValue(data.count)} 条记录{data.count ? ` · ${offset + 1}–${Math.min(offset + 50, data.count)}` : ""}</span><Button variant="outline" size="sm" disabled={offset === 0} onClick={() => setOffset(value => Math.max(0, value - 50))}>上一页</Button><Button variant="outline" size="sm" disabled={offset + 50 >= data.count} onClick={() => setOffset(value => value + 50)}>下一页</Button></div>
    </> : null}
  </div>;
}
