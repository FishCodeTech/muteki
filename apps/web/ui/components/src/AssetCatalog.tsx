"use client";

import { useCallback, useEffect, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { assetMonitoringApi } from "./assetMonitoringApi";
import type { ApiRecord, AssetMonitoringApi, ImportBatch } from "./assetMonitoringTypes";
import { Empty, ErrorMessage, JsonDetails, Loading, Notice, Pagination, Timestamp } from "./AssetMonitoringShared";
import styles from "./AssetMonitoring.module.css";

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" && value ? value : fallback;
}

function download(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url; anchor.download = filename; anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function AssetCatalog({ taskId, api = assetMonitoringApi, onSelectAsset, selectedAssetId }: {
  taskId: string;
  api?: AssetMonitoringApi;
  onSelectAsset: (assetId: string) => void;
  selectedAssetId?: string;
}) {
  const [tab, setTab] = useState<"assets" | "imports">("assets");
  const [query, setQuery] = useState("");
  const [activeQuery, setActiveQuery] = useState("");
  const [result, setResult] = useState<Awaited<ReturnType<AssetMonitoringApi["queryAssets"]>> | null>(null);
  const [savedQueries, setSavedQueries] = useState<ApiRecord[]>([]);
  const [selectedQueryId, setSelectedQueryId] = useState("");
  const [queryName, setQueryName] = useState("");
  const [cursors, setCursors] = useState<string[]>([""]);
  const [pageIndex, setPageIndex] = useState(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [exporting, setExporting] = useState(false);
  const [imports, setImports] = useState<ImportBatch[]>([]);
  const [expandedBatch, setExpandedBatch] = useState<ImportBatch | null>(null);
  const [batchLoading, setBatchLoading] = useState(false);
  const [selectedAssets, setSelectedAssets] = useState<ReadonlySet<string>>(new Set());
  const [bulkTag, setBulkTag] = useState("");
  const [bulkBusy, setBulkBusy] = useState(false);

  const loadSavedQueries = useCallback(async () => {
    try { setSavedQueries(await api.listSavedQueries(taskId)); } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  }, [api, taskId]);
  const loadImports = useCallback(async () => {
    try { setImports(await api.listImports(taskId)); } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  }, [api, taskId]);

  useEffect(() => { void loadSavedQueries(); void loadImports(); }, [loadImports, loadSavedQueries]);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setError("");
    void api.queryAssets(taskId, { query: activeQuery, cursor: cursors[pageIndex] || undefined, limit: 25 })
      .then(value => { if (!controller.signal.aborted) setResult(value); })
      .catch(cause => { if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : String(cause)); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [activeQuery, api, cursors, pageIndex, taskId]);

  const search = () => { setActiveQuery(query); setCursors([""]); setPageIndex(0); };
  const selectSavedQuery = (queryId: string) => {
    setSelectedQueryId(queryId);
    const saved = savedQueries.find(item => text(item.query_id || item.record_id) === queryId);
    if (!saved) return;
    const expression = text(saved.expression || saved.query || saved.dsl);
    setQuery(expression); setQueryName(text(saved.name)); setActiveQuery(expression); setCursors([""]); setPageIndex(0);
  };
  const saveQuery = async () => {
    setError(""); setNotice("");
    try {
      const existing = savedQueries.find(item => text(item.query_id || item.record_id) === selectedQueryId);
      const saved = await api.saveQuery(taskId, {
        query_id: selectedQueryId || undefined, name: queryName.trim() || query,
        expression: query, base_revision: existing?.revision,
      });
      const id = text(saved.query_id || saved.record_id);
      setSelectedQueryId(id); setNotice(selectedQueryId ? "保存查询已创建新版本；既有监控定义仍固定其原版本。" : "查询已保存，但不会自动创建监控。");
      await loadSavedQueries();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  };
  const deleteQuery = async () => {
    if (!selectedQueryId) return;
    try {
      await api.deleteQuery(taskId, selectedQueryId); setSelectedQueryId(""); setQueryName("");
      setNotice("保存查询已删除；既有监控固定版本未改变。"); await loadSavedQueries();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  };
  const runExport = async (format: "csv" | "json" | "zip") => {
    setExporting(true); setError(""); setNotice("");
    try {
      const blob = await api.exportAssets(taskId, { query: activeQuery, format });
      download(blob, `src-assets-${taskId}.${format}`);
      setNotice("导出由服务端按固定查询快照和完整结果集生成；manifest 可核对缺项与 hash。");
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setExporting(false); }
  };
  const applyBulkTag = async (association: "attach" | "detach") => {
    if (!result || !selectedAssets.size || !bulkTag.trim()) return;
    setBulkBusy(true); setError(""); setNotice("");
    const selected = result.items.filter(asset => selectedAssets.has(asset.asset_id));
    const outcomes = await Promise.allSettled(selected.map(asset => api.saveAnnotation(taskId, asset.asset_id, {
      kind: "tag", value: bulkTag.trim(), association, scope: "task", base_revision: asset.revision,
    })));
    const failures = outcomes.map((outcome, index) => ({ outcome, asset: selected[index] })).filter(item => item.outcome.status === "rejected");
    setNotice(failures.length ? `${selected.length - failures.length} 项成功，${failures.length} 项失败；逐项修订未覆盖。` : `已对 ${selected.length} 个固定版本资产${association === "attach" ? "关联" : "解除"}标签。`);
    if (failures.length) setError(failures.map(item => `${item.asset.canonical_key}: ${item.outcome.status === "rejected" ? String(item.outcome.reason) : ""}`).join("；"));
    setBulkBusy(false);
  };
  const openBatch = async (batchId: string) => {
    setBatchLoading(true);
    try { setExpandedBatch(await api.getImport(taskId, batchId)); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBatchLoading(false); }
  };
  const retryBatch = async (batchId: string) => {
    setBatchLoading(true); setError("");
    try {
      const batch = await api.retryImport(taskId, batchId); setExpandedBatch(batch);
      setNotice("仅重试未提交或明确可重试项；已成功的观测不会重复登记。"); await loadImports();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setBatchLoading(false); }
  };

  return <div className={styles.root}>
    <div className={styles.header}><div><h2>任务资料</h2><p>按当前问题检索资产、来源与导入记录，不以数量代表研究覆盖。</p></div><div className={styles.actions}><Button size="sm" variant="ghost" isDisabled={exporting} onPress={() => void runExport("csv")}>导出 CSV</Button><Button size="sm" variant="ghost" isDisabled={exporting} onPress={() => void runExport("json")}>导出 JSON</Button><Button size="sm" variant="ghost" isDisabled={exporting} onPress={() => void runExport("zip")}><Icon name="archive" size={14} />完整证据包</Button></div></div>
    <div className={styles.tabs} role="tablist" aria-label="资料类型"><button role="tab" aria-selected={tab === "assets"} onClick={() => setTab("assets")}>资产与查询</button><button role="tab" aria-selected={tab === "imports"} onClick={() => setTab("imports")}>导入批次<span className={styles.badge}>{imports.length}</span></button></div>
    {error ? <ErrorMessage message={error} /> : null}{notice ? <Notice>{notice}</Notice> : null}
    {tab === "assets" ? <>
      <section className={styles.section}>
        <form className={styles.form} onSubmit={event => { event.preventDefault(); search(); }}>
          <div className={styles.query}><input className={styles.input} aria-label="资产查询表达式" value={query} onChange={event => setQuery(event.target.value)} placeholder="例如：type=endpoint AND (method=POST OR host contains example.com)" /><Button type="submit" size="sm" variant="primary" isDisabled={loading}>查询</Button><Button type="button" size="sm" variant="ghost" onPress={() => { setQuery(""); setActiveQuery(""); setCursors([""]); setPageIndex(0); }}>清除</Button></div>
          <div className={styles.formRow}><select className={styles.select} aria-label="已保存查询" value={selectedQueryId} onChange={event => selectSavedQuery(event.target.value)}><option value="">选择已保存查询</option>{savedQueries.map(item => <option key={text(item.query_id || item.record_id)} value={text(item.query_id || item.record_id)}>{text(item.name, text(item.expression || item.query))}</option>)}</select><input className={styles.input} aria-label="查询名称" value={queryName} onChange={event => setQueryName(event.target.value)} placeholder="保存名称" /><Button type="button" size="sm" variant="ghost" onPress={() => void saveQuery()}>{selectedQueryId ? "保存为新版本" : "保存查询"}</Button>{selectedQueryId ? <Button type="button" size="sm" variant="ghost" onPress={() => void deleteQuery()}>删除</Button> : null}</div>
        </form>
        {result?.semantic_version ? <span className={styles.muted}>语义版本 {result.semantic_version}{result.snapshot ? ` · 快照 ${result.snapshot}` : ""}</span> : null}
        {result?.watermarks?.length ? <JsonDetails title="来源水位与索引完整性" value={result.watermarks} /> : null}
      </section>
      {loading ? <Loading>正在按固定语义查询资产…</Loading> : result && !result.items.length ? <Empty>当前条件下没有可读资产；这不代表来源为空或索引已完成。</Empty> : result ? <>
        <div className={styles.formRow}><input className={styles.input} aria-label="批量标签" value={bulkTag} onChange={event => setBulkTag(event.target.value)} placeholder="为所选资产添加或解除标签" /><Button size="sm" variant="ghost" isDisabled={bulkBusy || !selectedAssets.size || !bulkTag.trim()} onPress={() => void applyBulkTag("attach")}>批量关联</Button><Button size="sm" variant="ghost" isDisabled={bulkBusy || !selectedAssets.size || !bulkTag.trim()} onPress={() => void applyBulkTag("detach")}>批量解除</Button><span className={styles.muted}>已选 {selectedAssets.size} 项</span></div>
        <div className={styles.tableScroll}><table className={styles.table}><thead><tr><th><input type="checkbox" aria-label="选择当前页全部资产" checked={result.items.length > 0 && result.items.every(asset => selectedAssets.has(asset.asset_id))} onChange={event => setSelectedAssets(previous => { const next = new Set(previous); result.items.forEach(asset => { if (event.target.checked) next.add(asset.asset_id); else next.delete(asset.asset_id); }); return next; })} /></th><th>资产</th><th>类型 / 版本</th><th>最近观察</th><th>归属 / 状态</th></tr></thead><tbody>{result.items.map(asset => <tr key={asset.asset_id}><td><input type="checkbox" aria-label={`选择 ${asset.canonical_key}`} checked={selectedAssets.has(asset.asset_id)} onChange={event => setSelectedAssets(previous => { const next = new Set(previous); if (event.target.checked) next.add(asset.asset_id); else next.delete(asset.asset_id); return next; })} /></td><td><button className={styles.linkButton} onClick={() => onSelectAsset(asset.asset_id)} aria-current={selectedAssetId === asset.asset_id ? "true" : undefined}><strong>{text(asset.display_name, asset.canonical_key)}</strong></button><small>{asset.canonical_key}</small></td><td>{asset.asset_type}<small>v{asset.revision}</small></td><td><Timestamp value={asset.last_observed_at} /><small>首次：<Timestamp value={asset.first_seen_at} /></small></td><td>{asset.archived_at ? <span className={styles.badgeWarning}>已归档</span> : <span className={styles.badge}>当前</span>}{asset.judgement ? <JsonDetails title="判断依据" value={asset.judgement} /> : null}</td></tr>)}</tbody></table></div>
        <Pagination page={result} loading={loading} onPrevious={pageIndex > 0 ? () => setPageIndex(value => value - 1) : undefined} onNext={result.next_cursor ? () => { const cursor = result.next_cursor; if (cursor) { setCursors(value => [...value.slice(0, pageIndex + 1), cursor]); setPageIndex(value => value + 1); } } : undefined} />
      </> : null}
    </> : <section className={styles.section}>
      <div className={styles.sectionHeader}><h3>导入批次</h3><Button size="sm" variant="ghost" onPress={() => void loadImports()}><Icon name="refresh" size={14} />刷新</Button></div>
      {!imports.length ? <Empty>尚无导入记录。文件、连接器和外部事件会在这里保留原始来源与逐项结果。</Empty> : <div className={styles.split}><div className={styles.list}>{imports.map(batch => <article className={styles.card} key={batch.batch_id}><div className={styles.cardHeader}><button className={styles.linkButton} onClick={() => void openBatch(batch.batch_id)}><strong>{text(batch.source_name, batch.batch_id)}</strong></button><span className={styles.badge}>{batch.status}</span></div><div className={styles.meta}><span>{text(batch.source_kind, "未知来源")}</span><Timestamp value={batch.created_at} /><span>{batch.completeness || "完整性未知"}</span></div></article>)}</div>
        {batchLoading ? <Loading /> : expandedBatch ? <ImportDetails batch={expandedBatch} onRetry={() => void retryBatch(expandedBatch.batch_id)} /> : <Empty>选择一个批次查看原文件、映射、预览和逐项结果。</Empty>}
      </div>}
    </section>}
  </div>;
}

function ImportDetails({ batch, onRetry }: { batch: ImportBatch; onRetry: () => void }) {
  const counts = batch.counts || {};
  return <article className={styles.card}><div className={styles.cardHeader}><strong>{batch.batch_id}</strong><Button size="sm" variant="ghost" onPress={onRetry}>仅重试失败项</Button></div>
    <div className={styles.meta}>{Object.entries(counts).map(([key, value]) => <span key={key}>{key}：{String(value)}</span>)}{batch.upstream_limit !== undefined ? <span>上游上限：{String(batch.upstream_limit ?? "未知")}</span> : null}{batch.next_page ? <span>仍有下一页</span> : null}</div>
    {batch.completeness !== "complete" ? <Notice tone="warning">本批次不是完整取得：{batch.completeness || "未知"}。缺页、上限或失败不能显示为全量成功。</Notice> : null}
    {batch.cost ? <JsonDetails title="费用与上游回执（未知不会写成零）" value={batch.cost} /> : null}
    {batch.artifact ? <JsonDetails title="原始文件 / 响应 Artifact" value={batch.artifact} /> : null}
    {batch.field_mapping ? <JsonDetails title="字段映射与解析版本" value={batch.field_mapping} /> : null}
    {batch.preview?.length ? <JsonDetails title="同一次解析预览" value={batch.preview} /> : null}
    {batch.items?.length ? <div className={styles.tableScroll}><table className={styles.table}><thead><tr><th>输入位置</th><th>结果</th><th>字段 / 错误</th><th>修正方法</th></tr></thead><tbody>{batch.items.map(item => <tr key={`${item.index}:${item.code || item.status}`}><td>{item.index}</td><td>{item.status}{item.asset_id ? <small>{item.asset_id}</small> : null}</td><td>{item.field || "—"}{item.code ? <small>{item.code}：{item.message}</small> : null}</td><td>{item.recovery_hint || "—"}</td></tr>)}</tbody></table></div> : null}
  </article>;
}
