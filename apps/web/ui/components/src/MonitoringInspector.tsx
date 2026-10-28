"use client";

import { useCallback, useEffect, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { assetMonitoringApi } from "./assetMonitoringApi";
import type { AssetMonitoringApi, ChangeEvent, MonitorCycle } from "./assetMonitoringTypes";
import { Empty, ErrorMessage, Field, JsonDetails, Loading, Notice, Timestamp } from "./AssetMonitoringShared";
import styles from "./AssetMonitoring.module.css";

function text(value: unknown, fallback = ""): string { return typeof value === "string" && value ? value : fallback; }
function number(value: unknown, fallback = 0): number { return typeof value === "number" && Number.isFinite(value) ? value : fallback; }

export function MonitoringInspector({ taskId, monitorId, api = assetMonitoringApi, onSelectMonitor }: {
  taskId: string;
  monitorId?: string;
  api?: AssetMonitoringApi;
  onSelectMonitor?: (monitorId: string) => void;
}) {
  const [monitors, setMonitors] = useState<Awaited<ReturnType<AssetMonitoringApi["listMonitors"]>>>([]);
  const [selected, setSelected] = useState(monitorId || "");
  const [details, setDetails] = useState<Awaited<ReturnType<AssetMonitoringApi["getMonitor"]>> | null>(null);
  const [creating, setCreating] = useState(false);
  const [editingRevision, setEditingRevision] = useState<number | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [changeFilter, setChangeFilter] = useState<"all" | "incomparable" | "failed">("all");
  const [form, setForm] = useState({
    name: "", chain_type: "asset", query_version: "", source_ref: "", source_query: "", plan_ref: "", authorization_ref: "",
    interval_seconds: 3600, timezone: "Asia/Shanghai", total_budget: 1, per_cycle_budget: 1, call_limit: 10,
    notification: true, merge_window_seconds: 300,
  });

  const loadMonitors = useCallback(async () => {
    setLoading(true); setError("");
    try {
      const rows = await api.listMonitors(taskId); setMonitors(rows);
      if (!selected && monitorId) setSelected(monitorId);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setLoading(false); }
  }, [api, monitorId, selected, taskId]);
  const loadMonitor = useCallback(async () => {
    if (!selected) { setDetails(null); return; }
    setLoading(true); setError("");
    try { setDetails(await api.getMonitor(taskId, selected)); }
    catch (cause) { setDetails(null); setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setLoading(false); }
  }, [api, selected, taskId]);
  useEffect(() => { void loadMonitors(); }, [loadMonitors]);
  useEffect(() => { void loadMonitor(); }, [loadMonitor]);
  useEffect(() => { if (monitorId) setSelected(monitorId); }, [monitorId]);

  const control = async (action: "pause" | "resume" | "cancel" | "run") => {
    setBusy(true); setError(""); setNotice("");
    try {
      await api.controlMonitor(taskId, selected, action);
      setNotice(action === "run" ? "已请求一轮受预算约束的采集；完成、失败和费用以实际轮次为准。" : action === "pause" ? "监控已暂停；已提交资料保留，不再启动新轮次。" : action === "resume" ? "监控已恢复；慢轮不会重叠补跑。" : "监控已取消；历史轮次与基线仍可读。");
      await Promise.all([loadMonitors(), loadMonitor()]);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBusy(false); }
  };
  const create = async () => {
    setBusy(true); setError(""); setNotice("");
    try {
      const monitor = await api.createMonitor(taskId, {
        monitor_id: editingRevision !== null ? selected : undefined, base_revision: editingRevision ?? undefined,
        name: form.name.trim(), chain_type: form.chain_type, plan_ref: form.plan_ref.trim(), source_query: form.source_query.trim(),
        query_version: form.query_version.trim(), source_refs: [{ source_id: form.source_ref.trim() }],
        authorization_ref: form.authorization_ref.trim(), interval_seconds: form.interval_seconds,
        timezone: form.timezone,
        budget: { total: form.total_budget, per_cycle: form.per_cycle_budget, currency: "USD", call_limit: form.call_limit },
        notification: { enabled: form.notification, merge_window_seconds: form.merge_window_seconds },
      });
      setSelected(monitor.monitor_id); setCreating(false); setEditingRevision(null);
      setNotice(editingRevision !== null ? "监控定义已创建新版本；历史轮次仍引用原定义版本。" : "监控已按明确请求创建；保存查询本身不会启动周期采集。");
      await loadMonitors();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBusy(false); }
  };
  const monitor = details?.monitor;
  const cycles = details?.cycles || [];
  const changes = (details?.changes || []).filter(change => changeFilter === "all" || (changeFilter === "incomparable" ? !change.comparable : change.completeness !== "complete"));
  const lastSuccess = cycles.filter(cycle => cycle.status === "completed" && cycle.completeness === "complete").at(-1);
  return <div className={styles.root}>
    <div className={styles.header}><div><h2>持续监控</h2><p>规则与资产变化分别比较；只有明确创建的监控才会周期调用来源。</p></div><div className={styles.actions}><Button size="sm" variant="ghost" onPress={() => void Promise.all([loadMonitors(), loadMonitor()])}><Icon name="refresh" size={14} />刷新</Button><Button size="sm" variant="primary" onPress={() => setCreating(value => !value)}>{creating ? "关闭创建" : "创建监控"}</Button></div></div>
    {error ? <ErrorMessage message={error} /> : null}{notice ? <Notice>{notice}</Notice> : null}
    {creating ? <section className={styles.section}><div className={styles.sectionHeader}><h3>明确创建持续监控</h3></div><Notice>定义固定查询、来源、授权与预算。首个完整成功轮次才建立基线；不会自动创建 Run 或 Finding。</Notice><div className={styles.gridTwo}><label className={styles.field}><span>名称</span><input className={styles.input} value={form.name} onChange={event => setForm({ ...form, name: event.target.value })} /></label><label className={styles.field}><span>变化链</span><select className={styles.select} value={form.chain_type} onChange={event => setForm({ ...form, chain_type: event.target.value })}><option value="asset">资产 / 服务 / 接口</option><option value="rule">计划规则 / 范围</option></select></label><label className={styles.field}><span>固定查询 / 目标版本</span><input className={styles.input} required value={form.query_version} onChange={event => setForm({ ...form, query_version: event.target.value })} placeholder="query-id:v3" /></label><label className={styles.field}><span>来源引用</span><input className={styles.input} required value={form.source_ref} onChange={event => setForm({ ...form, source_ref: event.target.value })} placeholder="connector-id:config-version" /></label><label className={styles.field}><span>来源查询语句</span><input className={styles.input} required value={form.source_query} onChange={event => setForm({ ...form, source_query: event.target.value })} /></label><label className={styles.field}><span>计划版本</span><input className={styles.input} required value={form.plan_ref} onChange={event => setForm({ ...form, plan_ref: event.target.value })} placeholder="plan-id:v1" /></label><label className={styles.field}><span>执行策略版本</span><input className={styles.input} required value={form.authorization_ref} onChange={event => setForm({ ...form, authorization_ref: event.target.value })} /></label><label className={styles.field}><span>时区</span><input className={styles.input} value={form.timezone} onChange={event => setForm({ ...form, timezone: event.target.value })} /></label><label className={styles.field}><span>周期（秒）</span><input className={styles.input} type="number" min={30} value={form.interval_seconds} onChange={event => setForm({ ...form, interval_seconds: number(event.target.valueAsNumber, 3600) })} /></label><label className={styles.field}><span>单轮调用上限</span><input className={styles.input} type="number" min={1} value={form.call_limit} onChange={event => setForm({ ...form, call_limit: number(event.target.valueAsNumber, 10) })} /></label><label className={styles.field}><span>总预算（USD）</span><input className={styles.input} type="number" min={0} step="any" value={form.total_budget} onChange={event => setForm({ ...form, total_budget: number(event.target.valueAsNumber, 0) })} /></label><label className={styles.field}><span>单轮预算（USD）</span><input className={styles.input} type="number" min={0} step="any" value={form.per_cycle_budget} onChange={event => setForm({ ...form, per_cycle_budget: number(event.target.valueAsNumber, 0) })} /></label><label className={styles.field}><span>通知合并窗口（秒）</span><input className={styles.input} type="number" min={0} value={form.merge_window_seconds} onChange={event => setForm({ ...form, merge_window_seconds: number(event.target.valueAsNumber, 300) })} /></label></div><label className={styles.checkbox}><input type="checkbox" checked={form.notification} onChange={event => setForm({ ...form, notification: event.target.checked })} />有意义变化、完成、失败或需人处理时通知；未变化静默</label><Button size="sm" variant="primary" isDisabled={busy || !form.name.trim() || !form.query_version.trim() || !form.source_ref.trim() || !form.source_query.trim() || !form.plan_ref.trim() || !form.authorization_ref.trim()} onPress={() => void create()}>{editingRevision !== null ? "保存定义新版本" : "确认创建监控"}</Button></section> : null}
    <section className={styles.section}><div className={styles.formRow}><select className={styles.select} aria-label="选择监控" value={selected} onChange={event => { setSelected(event.target.value); onSelectMonitor?.(event.target.value); }}><option value="">选择监控定义</option>{monitors.map(item => <option key={item.monitor_id} value={item.monitor_id}>{item.name} · v{item.version} · {item.status}</option>)}</select>{monitor ? <><Button size="sm" variant="ghost" onPress={() => { setEditingRevision(details?.revision ?? monitor.revision); setCreating(true); setForm({ ...form, name: monitor.name, chain_type: monitor.chain_type, query_version: monitor.query_version, source_ref: `${String((monitor.source as Record<string, unknown> | undefined)?.connector_id || "")}:${String((monitor.source as Record<string, unknown> | undefined)?.config_version || "")}`, source_query: String((monitor.source as Record<string, unknown> | undefined)?.query || ""), plan_ref: String(monitor.plan_ref || ""), authorization_ref: monitor.authorization_ref || "", interval_seconds: monitor.interval_seconds, timezone: monitor.timezone, total_budget: Number(monitor.budget?.total ?? 0), per_cycle_budget: Number(monitor.budget?.per_cycle ?? 0), call_limit: Number(monitor.budget?.call_limit ?? 0), notification: (monitor.notification?.enabled !== false), merge_window_seconds: Number(monitor.notification?.merge_window_seconds ?? 0) }); }}>修订定义</Button><Button size="sm" variant="ghost" isDisabled={busy || monitor.status !== "active"} onPress={() => void control("pause")}>暂停</Button><Button size="sm" variant="ghost" isDisabled={busy || monitor.status !== "paused"} onPress={() => void control("resume")}>恢复</Button><Button size="sm" variant="ghost" isDisabled={busy || monitor.status !== "active"} onPress={() => void control("run")}>执行一轮</Button><Button size="sm" variant="ghost" isDisabled={busy || monitor.status === "cancelled"} onPress={() => void control("cancel")}>取消监控</Button></> : null}</div></section>
    {loading && selected ? <Loading>正在恢复定义、轮次与基线…</Loading> : monitor && details ? <>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>{monitor.name}</h3><span className={styles.badge}>{monitor.status}</span></div><dl className={styles.definition}><Field label="变化链">{monitor.chain_type === "rule" ? "计划规则 / 范围" : "资产 / 服务 / 接口"}</Field><Field label="定义版本">v{monitor.version}</Field><Field label="固定查询版本">{monitor.query_version}</Field><Field label="周期 / 时区">{monitor.interval_seconds} 秒 · {monitor.timezone}</Field><Field label="最近完整成功"><Timestamp value={lastSuccess?.finished_at || monitor.last_success_at} /></Field><Field label="当前基线">{monitor.baseline_id || "尚未建立（需要首个完整成功轮次）"}</Field></dl><JsonDetails title="来源、授权与配置版本" value={{ source_refs: monitor.source_refs, authorization_ref: monitor.authorization_ref }} /><JsonDetails title="预算与通知定义" value={{ budget: monitor.budget, notification: monitor.notification }} /></section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>轮次与费用</h3><span className={styles.badge}>{cycles.length}</span></div>{cycles.length ? <div className={styles.tableScroll}><table className={styles.table}><thead><tr><th>轮次 / 定义</th><th>状态 / 完整性</th><th>起止时间</th><th>基线</th><th>费用 / 结束原因</th></tr></thead><tbody>{cycles.map(cycle => <CycleRow key={cycle.cycle_id} cycle={cycle} />)}</tbody></table></div> : <Empty>尚无轮次；未启动监控不代表“没有变化”。</Empty>}</section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>可比基线</h3><span className={styles.badge}>{details.baselines.length}</span></div>{details.baselines.length ? <div className={styles.list}>{details.baselines.map((baseline, index) => <article className={styles.card} key={text(baseline.baseline_id, String(index))}><div className={styles.cardHeader}><strong>{text(baseline.baseline_id, `基线 ${index + 1}`)}</strong><span className={styles.badge}>{text(baseline.completeness, "完整性未知")}</span></div><div className={styles.meta}><span>建立于 <Timestamp value={text(baseline.created_at) || null} /></span><span>来源轮次 {text(baseline.cycle_id, "未知")}</span></div><JsonDetails title="原始与降噪基线（独立版本）" value={baseline} /></article>)}</div> : <Empty>尚无可比基线。局部失败或不可比轮次不会推进完整基线。</Empty>}</section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>变化与复查候选</h3><select className={styles.select} value={changeFilter} onChange={event => setChangeFilter(event.target.value as typeof changeFilter)} aria-label="变化筛选"><option value="all">全部变化</option><option value="incomparable">不可比</option><option value="failed">不完整 / 失败</option></select></div>{changes.length ? <div className={styles.list}>{changes.map(change => <ChangeCard key={change.change_id} change={change} />)}</div> : <Empty>{changeFilter === "all" ? "没有变化事件。只有完整成功且无变化的轮次才可视为安静；失败会单独显示。" : "当前筛选下没有事件。"}</Empty>}</section>
      {details.notifications?.length ? <section className={styles.section}><div className={styles.sectionHeader}><h3>通知与原事件</h3></div><JsonDetails title="合并通知保留全部原事件入口" value={details.notifications} open /></section> : null}
    </> : !selected ? <Empty>选择已有监控，或明确创建一个新的持续监控。</Empty> : null}
  </div>;
}

function CycleRow({ cycle }: { cycle: MonitorCycle }) {
  const cost = cycle.cost || {};
  const unknown = cost.unknown === true || cost.status === "unknown" || cost.actual == null && cost.confirmed == null;
  return <tr><td>{cycle.cycle_id}<small>定义 v{cycle.definition_version}</small></td><td>{cycle.status}<small>{cycle.completeness || "完整性未知"}</small></td><td><Timestamp value={cycle.started_at || cycle.scheduled_at} /><small>结束：<Timestamp value={cycle.finished_at} /></small></td><td>{cycle.baseline_id || "未推进"}<small>比较：{cycle.compared_baseline_id || "—"}</small></td><td><span className={unknown ? styles.costUnknown : undefined}>{unknown ? "实际费用未知" : <JsonDetails title="查看费用" value={cost} />}</span>{cycle.end_reason ? <small>{cycle.end_reason}</small> : null}<JsonDetails title="来源水位" value={cycle.source_watermarks || {}} /></td></tr>;
}

function ChangeCard({ change }: { change: ChangeEvent }) {
  return <article className={styles.card}><div className={styles.cardHeader}><strong>{change.change_type} · {text(change.subject_ref.asset_id || change.subject_ref.rule_id || change.subject_ref.id, "未知对象")}</strong><span className={change.comparable ? styles.badge : styles.badgeWarning}>{change.comparable ? "可比" : "不可比"}</span></div><div className={styles.meta}><span>{change.chain_type === "rule" ? "规则变化链" : "资产变化链"}</span><span>轮次 {change.cycle_id}</span><span>{change.completeness || "完整性未知"}</span></div>{!change.comparable ? <Notice tone="warning">{change.incomparable_reason || "查询、来源、目标集合、权限或采样发生变化，不能直接比较。"}</Notice> : null}<div className={styles.diff}><JsonDetails title="原始前后与字节差异" value={{ before: change.before_ref, after: change.after_ref, diff: change.raw_diff }} /><JsonDetails title="降噪视图与被忽略规则" value={{ diff: change.denoised_diff, rule: change.denoise_rule }} /></div>{change.candidate ? <JsonDetails title="复查候选（不是 Finding，也不自动创建 Run）" value={change.candidate} /> : null}{change.notification_ids?.length ? <div className={styles.meta}>通知：{change.notification_ids.join("、")}</div> : null}</article>;
}
