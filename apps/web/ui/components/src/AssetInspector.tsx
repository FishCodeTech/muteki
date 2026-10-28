"use client";

/* eslint-disable @next/next/no-img-element -- original evidence bytes are served directly, not optimized or rewritten. */
import { useCallback, useEffect, useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { assetMonitoringApi } from "./assetMonitoringApi";
import type { ApiRecord, AssetMonitoringApi, AssetObservation, AssetRelation } from "./assetMonitoringTypes";
import { Empty, ErrorMessage, Field, JsonDetails, Loading, Notice, Pagination, Timestamp } from "./AssetMonitoringShared";
import styles from "./AssetMonitoring.module.css";

function text(value: unknown, fallback = ""): string {
  return typeof value === "string" && value ? value : fallback;
}
function rows<T>(value: unknown): T[] { return Array.isArray(value) ? value as T[] : []; }
function record(value: unknown): ApiRecord { return value && typeof value === "object" && !Array.isArray(value) ? value as ApiRecord : {}; }

export function AssetInspector({ taskId, assetId, api = assetMonitoringApi, onSelectAsset }: {
  taskId: string;
  assetId: string;
  api?: AssetMonitoringApi;
  onSelectAsset?: (assetId: string) => void;
}) {
  const [asset, setAsset] = useState<Awaited<ReturnType<AssetMonitoringApi["getAsset"]>> | null>(null);
  const [observations, setObservations] = useState<Awaited<ReturnType<AssetMonitoringApi["listAssetObservations"]>> | null>(null);
  const [relations, setRelations] = useState<Awaited<ReturnType<AssetMonitoringApi["listAssetRelations"]>> | null>(null);
  const [observationCursors, setObservationCursors] = useState<string[]>([""]);
  const [observationPage, setObservationPage] = useState(0);
  const [relationCursors, setRelationCursors] = useState<string[]>([""]);
  const [relationPage, setRelationPage] = useState(0);
  const [tab, setTab] = useState<"profile" | "sources" | "history" | "relations" | "annotations" | "publish">("profile");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [annotationKind, setAnnotationKind] = useState<"note" | "tag">("note");
  const [annotation, setAnnotation] = useState("");
  const [purpose, setPurpose] = useState("");
  const [dataset, setDataset] = useState("default");
  const [fields, setFields] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [editingAnnotationId, setEditingAnnotationId] = useState("");
  const [editingAnnotationRevision, setEditingAnnotationRevision] = useState<number | undefined>(undefined);
  const [selectedObservations, setSelectedObservations] = useState<ReadonlySet<string>>(new Set());
  const [selectedRelations, setSelectedRelations] = useState<ReadonlySet<string>>(new Set());

  const loadAsset = useCallback(async () => {
    setLoading(true); setError("");
    try { setAsset(await api.getAsset(taskId, assetId)); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setLoading(false); }
  }, [api, assetId, taskId]);
  useEffect(() => { setAsset(null); setObservations(null); setRelations(null); setObservationCursors([""]); setRelationCursors([""]); setObservationPage(0); setRelationPage(0); void loadAsset(); }, [loadAsset]);
  useEffect(() => {
    let active = true;
    api.listAssetObservations(taskId, assetId, { cursor: observationCursors[observationPage] || undefined, limit: 30 })
      .then(value => { if (active) setObservations(value); })
      .catch(cause => { if (active) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => { active = false; };
  }, [api, assetId, observationCursors, observationPage, taskId]);
  useEffect(() => {
    let active = true;
    api.listAssetRelations(taskId, assetId, { cursor: relationCursors[relationPage] || undefined, limit: 30 })
      .then(value => { if (active) setRelations(value); })
      .catch(cause => { if (active) setError(cause instanceof Error ? cause.message : String(cause)); });
    return () => { active = false; };
  }, [api, assetId, relationCursors, relationPage, taskId]);

  const saveAnnotation = async () => {
    if (!asset || !annotation.trim()) return;
    setSubmitting(true); setError("");
    try {
      await api.saveAnnotation(taskId, assetId, {
        annotation_id: editingAnnotationId || undefined,
        kind: annotationKind, text: annotation.trim(), value: annotation.trim(),
        scope: "task", base_revision: editingAnnotationId ? editingAnnotationRevision : undefined,
      });
      setAnnotation(""); setEditingAnnotationId(""); setEditingAnnotationRevision(undefined);
      setNotice(annotationKind === "tag" ? "标签版本已保存；未改变资产或其他 Task 的资料。" : "备注已保存为 Task 私有注解，不会自动发布。"); await loadAsset();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSubmitting(false); }
  };
  const removeAnnotation = async (annotationId: string, kind: "note" | "tag" = "note") => {
    try { await api.deleteAnnotation(taskId, assetId, annotationId, kind); await loadAsset(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  };
  const publish = async () => {
    if (!asset) return;
    setSubmitting(true); setError(""); setNotice("");
    try {
      const result = await api.publishAsset(taskId, assetId, {
        purpose, dataset_id: dataset, asset_revision: asset.revision,
        fields: fields.split(",").map(value => value.trim()).filter(Boolean),
        observation_ids: [...selectedObservations], relation_ids: [...selectedRelations],
        source_watermarks: observations?.watermarks || [],
      });
      setNotice(`已发布固定版本 ${text(result.publication_id || result.record_id, "新发布")}；不会共享实时 Profile、凭据库或未选材料。`);
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSubmitting(false); }
  };
  const toggleArchive = async () => {
    if (!asset) return;
    setSubmitting(true);
    try {
      await api.setAssetArchived(taskId, assetId, !asset.archived_at, asset.revision);
      setNotice(asset.archived_at ? "资产已恢复，历史来源与冻结版本未改写。" : "资产已归档；其他 Task 和冻结报告不受影响。"); await loadAsset();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSubmitting(false); }
  };

  if (loading && !asset) return <Loading>正在恢复资产画像与版本…</Loading>;
  if (error && !asset) return <ErrorMessage message={error} onRetry={() => void loadAsset()} />;
  if (!asset) return <Empty>资产不可读。</Empty>;
  const profile = record(asset.profile);
  const conflicts = rows<ApiRecord>(profile.conflicts || asset.conflicts);
  const screenshots = rows<ApiRecord>(profile.screenshots || asset.screenshots);
  const tags = rows<ApiRecord | string>(asset.tags);
  const notes = rows<ApiRecord>(asset.notes || profile.notes);
  const publications = rows<ApiRecord>(asset.publications || profile.publications);
  return <div className={styles.root}>
    <div className={styles.header}><div><h2>{text(asset.display_name, asset.canonical_key)}</h2><p>{asset.asset_type} · {asset.canonical_key} · v{asset.revision}</p></div><div className={styles.actions}><Button size="sm" variant="ghost" onPress={() => void loadAsset()}><Icon name="refresh" size={14} />刷新</Button><Button size="sm" variant="ghost" isDisabled={submitting} onPress={() => void toggleArchive()}>{asset.archived_at ? "恢复资产" : "归档资产"}</Button></div></div>
    {error ? <ErrorMessage message={error} /> : null}{notice ? <Notice>{notice}</Notice> : null}
    {asset.archived_at ? <Notice tone="warning">该资产已归档；历史观测与报告引用仍保留。</Notice> : null}
    <div className={styles.tabs} role="tablist" aria-label="资产详情"><button role="tab" aria-selected={tab === "profile"} onClick={() => setTab("profile")}>画像</button><button role="tab" aria-selected={tab === "sources"} onClick={() => setTab("sources")}>来源</button><button role="tab" aria-selected={tab === "history"} onClick={() => setTab("history")}>历史</button><button role="tab" aria-selected={tab === "relations"} onClick={() => setTab("relations")}>关系</button><button role="tab" aria-selected={tab === "annotations"} onClick={() => setTab("annotations")}>标签与备注</button><button role="tab" aria-selected={tab === "publish"} onClick={() => setTab("publish")}>发布</button></div>
    {tab === "profile" ? <>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>当前画像</h3><span className={styles.muted}>由所选观测计算，不覆盖历史</span></div><dl className={styles.definition}><Field label="首次可见"><Timestamp value={asset.first_seen_at} /></Field><Field label="最近观察"><Timestamp value={asset.last_observed_at} /></Field><Field label="最近成功核对"><Timestamp value={asset.last_success_at} /></Field><Field label="最近失败"><Timestamp value={asset.last_failure_at} /></Field></dl><JsonDetails title="当前字段与选择依据" value={profile} open /></section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>归属、计划与主动测试判断</h3></div><JsonDetails title="分别保存的判断与来源版本" value={asset.judgement || { state: "unknown" }} /></section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>字段冲突</h3><span className={styles.badge}>{conflicts.length}</span></div>{conflicts.length ? <div className={styles.tableScroll}><table className={styles.table}><thead><tr><th>字段</th><th>原值</th><th>来源 / 时间</th><th>采用依据</th></tr></thead><tbody>{conflicts.map((conflict, index) => <tr key={index}><td>{text(conflict.field, "未知字段")}</td><td><JsonDetails title="查看原值" value={conflict.value ?? conflict.raw_value} /></td><td>{text(conflict.source_id, "未知来源")}<small><Timestamp value={text(conflict.observed_at) || null} /></small></td><td>{text(conflict.selection_reason || conflict.reason, "同级冲突未静默选择")}</td></tr>)}</tbody></table></div> : <Empty>当前没有已投影的字段冲突。</Empty>}</section>
      <section className={styles.section}><div className={styles.sectionHeader}><h3>参数与可视化观测</h3></div><JsonDetails title="参数实例（方法、位置、原名与身份保持区分）" value={profile.parameters || []} />{screenshots.map((shot, index) => <figure key={index}><img className={styles.image} src={text(shot.url)} alt={`页面截图：${text(shot.page_url, "URL 未知")}，身份 ${text(shot.context_id, "未知")}`} /><figcaption className={styles.meta}><span>{text(shot.page_url, "URL 未知")}</span><span>generation {String(shot.browser_generation ?? "未知")}</span><span>{shot.reviewed_artifact_sha256 ? "Reviewer 已读取原图" : "图片未审阅"}</span></figcaption></figure>)}</section>
    </> : null}
    {tab === "sources" || tab === "history" ? <ObservationView tab={tab} data={observations} page={observationPage} selected={selectedObservations} onToggle={id => setSelectedObservations(previous => { const next = new Set(previous); if (next.has(id)) next.delete(id); else next.add(id); return next; })} onPrevious={observationPage > 0 ? () => setObservationPage(value => value - 1) : undefined} onNext={observations?.next_cursor ? () => { const cursor = observations.next_cursor; if (cursor) { setObservationCursors(value => [...value.slice(0, observationPage + 1), cursor]); setObservationPage(value => value + 1); } } : undefined} /> : null}
    {tab === "relations" ? <section className={styles.section}><div className={styles.sectionHeader}><h3>局部关系</h3><span className={styles.muted}>服务端分页；未展开部分不能由前端折叠冒充完整</span></div>{relations?.items.length ? <div className={styles.list}>{relations.items.map(relation => <RelationCard key={relation.relation_id} relation={relation} currentAssetId={assetId} onSelectAsset={onSelectAsset} selected={selectedRelations.has(relation.relation_id)} onToggle={() => setSelectedRelations(previous => { const next = new Set(previous); if (next.has(relation.relation_id)) next.delete(relation.relation_id); else next.add(relation.relation_id); return next; })} />)}</div> : <Empty>当前页没有关系记录；邻居计数与不可读对象不会在此泄露。</Empty>}{relations ? <Pagination page={relations} onPrevious={relationPage > 0 ? () => setRelationPage(value => value - 1) : undefined} onNext={relations.next_cursor ? () => { const cursor = relations.next_cursor; if (cursor) { setRelationCursors(value => [...value.slice(0, relationPage + 1), cursor]); setRelationPage(value => value + 1); } } : undefined} /> : null}</section> : null}
    {tab === "annotations" ? <section className={styles.section}><div className={styles.sectionHeader}><h3>Task 私有标签与备注</h3></div><div className={styles.formRow}><select className={styles.select} value={annotationKind} onChange={event => setAnnotationKind(event.target.value as "note" | "tag")} aria-label="注解类型"><option value="note">备注</option><option value="tag">标签</option></select><input className={styles.input} value={annotation} onChange={event => setAnnotation(event.target.value)} placeholder={annotationKind === "tag" ? "标签名称" : "记录业务背景或研究说明"} /><Button size="sm" variant="primary" isDisabled={submitting || !annotation.trim()} onPress={() => void saveAnnotation()}>{editingAnnotationId ? "保存新版本" : "保存"}</Button>{editingAnnotationId ? <Button size="sm" variant="ghost" onPress={() => { setEditingAnnotationId(""); setEditingAnnotationRevision(undefined); setAnnotation(""); }}>取消编辑</Button> : null}</div><div className={styles.meta}>{tags.map((tag, index) => { const id = typeof tag === "string" ? "" : text(tag.annotation_id || tag.record_id); const rev = typeof tag === "string" ? undefined : Number(tag.revision || tag._record_revision || 1); const value = typeof tag === "string" ? tag : text(tag.name || tag.value); return <span className={styles.badge} key={id || index}>{value}{id ? <><Button size="sm" variant="ghost" onPress={() => { setEditingAnnotationId(id); setEditingAnnotationRevision(rev); setAnnotationKind("tag"); setAnnotation(value); }}>编辑</Button><Button size="sm" variant="ghost" onPress={() => void removeAnnotation(id, "tag")}>删除</Button></> : null}</span>; })}</div>{notes.map((note, index) => { const id = text(note.annotation_id || note.record_id); const rev = Number(note.revision || note._record_revision || 1); return <article className={styles.card} key={id || String(index)}><p>{text(note.text || note.value)}</p><div className={styles.meta}><span>{text(note.author || note.actor, "未知作者")}</span><Timestamp value={text(note.created_at || note.updated_at) || null} /><span>v{String(rev)}</span><Button size="sm" variant="ghost" onPress={() => { setEditingAnnotationId(id); setEditingAnnotationRevision(rev); setAnnotationKind("note"); setAnnotation(text(note.text || note.value)); }}>编辑</Button><Button size="sm" variant="ghost" onPress={() => void removeAnnotation(id, "note")}>删除</Button></div></article>; })}</section> : null}
    {tab === "publish" ? <section className={styles.section}><div className={styles.sectionHeader}><h3>发布选定版本</h3></div><Notice>发布固定实体字段、用户在来源与关系页勾选的 {selectedObservations.size} 条观测和 {selectedRelations.size} 条关系；不会发布实时图、浏览器 Profile 或整个凭据库。</Notice><div className={styles.form}><label className={styles.field}><span>目标资料集</span><input className={styles.input} value={dataset} onChange={event => setDataset(event.target.value)} /></label><label className={styles.field}><span>发布目的</span><input className={styles.input} value={purpose} onChange={event => setPurpose(event.target.value)} placeholder="供后续 Task 复用当前资产画像与来源" /></label><label className={styles.field}><span>字段（逗号分隔；空值由服务端拒绝或按明确契约处理）</span><input className={styles.input} value={fields} onChange={event => setFields(event.target.value)} placeholder="canonical_key,title,status_code" /></label><Button size="sm" variant="primary" isDisabled={submitting || !purpose.trim() || !fields.trim()} onPress={() => void publish()}>发布当前固定版本</Button></div>{publications.length ? <JsonDetails title="已有发布、引用与撤回记录" value={publications} open /> : <Empty>尚无发布记录。</Empty>}</section> : null}
  </div>;
}

function ObservationView({ tab, data, onPrevious, onNext, page, selected, onToggle }: {
  tab: "sources" | "history";
  data: Awaited<ReturnType<AssetMonitoringApi["listAssetObservations"]>> | null;
  onPrevious?: () => void; onNext?: () => void; page: number;
  selected: ReadonlySet<string>; onToggle: (observationId: string) => void;
}) {
  if (!data) return <Loading>正在读取真实来源与历史…</Loading>;
  return <section className={styles.section}><div className={styles.sectionHeader}><h3>{tab === "sources" ? "来源与原始证据" : "不可变观测历史"}</h3><span className={styles.muted}>观察时间、入库时间和重新提取分别保存</span></div>{data.items.length ? <div className={styles.list}>{data.items.map(item => <ObservationCard key={item.observation_id} item={item} tab={tab} selected={selected.has(item.observation_id)} onToggle={() => onToggle(item.observation_id)} />)}</div> : <Empty>当前页没有记录；来源缺页或索引落后会在水位中单独显示。</Empty>}<Pagination page={data} onPrevious={onPrevious} onNext={onNext} /><JsonDetails title="分页快照与来源水位" value={{ snapshot: data.snapshot, watermarks: data.watermarks, page: page + 1 }} /></section>;
}

function ObservationCard({ item, tab, selected, onToggle }: { item: AssetObservation; tab: "sources" | "history"; selected: boolean; onToggle: () => void }) {
  return <article className={styles.card}><div className={styles.cardHeader}><strong>{item.source_kind} · {item.source_id}</strong><label className={styles.checkbox}><input type="checkbox" checked={selected} onChange={onToggle} />发布此项</label></div><div className={styles.meta}><span>现场观察：<Timestamp value={item.observed_at} /></span><span>入库：<Timestamp value={item.ingested_at} /></span><span>解析：{item.parser_version || "未知"}</span>{item.success === false ? <span className={styles.badgeWarning}>失败观测</span> : null}{item.revoked_at ? <span className={styles.badgeDanger}>已撤回</span> : null}</div><JsonDetails title={tab === "sources" ? "原始值、来源与 Artifact" : "属性、纠正与完整性"} value={tab === "sources" ? { raw_values: item.raw_values, source_event_seq: item.source_event_seq, artifacts: item.artifact_refs } : { attributes: item.attributes, correction_of: item.correction_of, selected: item.selected, selection_reason: item.selection_reason }} /></article>;
}

function RelationCard({ relation, currentAssetId, onSelectAsset, selected, onToggle }: { relation: AssetRelation; currentAssetId: string; onSelectAsset?: (assetId: string) => void; selected: boolean; onToggle: () => void }) {
  const other = relation.from_asset_id === currentAssetId ? relation.to_asset_id : relation.from_asset_id;
  const otherAsset = relation.other_asset;
  const originLabel = relation.origin === "observed" ? "实际观察" : relation.origin === "derived" ? "确定性派生" : "模型推断";
  return <article className={styles.card}><div className={styles.cardHeader}><strong>{relation.relation_type}</strong><span className={styles.badgeInfo}>{originLabel}</span><label className={styles.checkbox}><input type="checkbox" checked={selected} onChange={onToggle} />发布</label></div><div>{onSelectAsset ? <button className={styles.linkButton} onClick={() => onSelectAsset(other)}>{otherAsset?.display_name || otherAsset?.canonical_key || other}</button> : other}</div><div className={styles.meta}><span>{relation.from_asset_id} → {relation.to_asset_id}</span></div>{relation.evidence_refs?.length ? <JsonDetails title="关系支持证据" value={relation.evidence_refs} /> : <span className={styles.muted}>未附证据引用</span>}</article>;
}
