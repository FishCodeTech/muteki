"use client";

import { useMemo, useState } from "react";
import { Badge, Button, Callout, Checkbox, EmptyState, IconButton, SearchInput, SegmentedControl, Select } from "@/components/chat/ui";
import {
  countBy, evidenceId, evidenceRef, FINDING_SEVERITY_LABELS, filterFindings, findingRef, suggestFindingGroups, uniqueEvidence,
  type FindingFilter, type FindingMerge, type FindingSeverity, type FindingState, type ResearchAsset, type ResearchEvidence,
  type ResearchFinding, type ResearchRef, type ResearchSnapshot,
} from "@/lib/srcResearch";
import { FindingInspector } from "./FindingInspector";
import { Definitions, FindingStateBadge, OriginTag, PaneHeader, Pager, RefChip, SeverityBadge } from "./SrcResearchParts";
import styles from "./srcResearch.module.css";

const PAGE_SIZE = 40;

export type SrcResearchFindingActions = {
  onFilter: (patch: Partial<FindingFilter>) => void;
  onPage: (page: number) => void;
  onToggle: (findingId: string) => void;
  onSelect: (ids: string[]) => void;
  onFocus: (findingId: string) => void;
  onStartMerge: (findings: ResearchFinding[]) => void;
  onMergePrimary: (primaryId: string) => void;
  onCancelMerge: () => void;
  onConfirmMerge: () => void;
  onUndoMerge: (mergeId: string) => void;
  onOpenEvidence: (ref: ResearchEvidence, findingId: string) => void;
  onOpenAsset: (assetId: string) => void;
  onOpenRef: (ref: ResearchRef) => void;
  onAddContext: (...refs: ResearchRef[]) => void;
};

function evidenceLabel(ref: ResearchEvidence): string {
  if (ref.exchange_id) return `请求与响应 · ${ref.exchange_id}`;
  return `原文 · ${ref.artifact_id || ref.name || "Artifact"}${ref.sha256 ? ` · sha256:${ref.sha256.slice(0, 10)}…` : ""}`;
}

function EvidenceButtons({ finding, onOpen }: { finding: ResearchFinding; onOpen: (ref: ResearchEvidence, findingId: string) => void }) {
  if (!finding.evidence.length) return <p className={styles.empty}>此发现未引用原始证据。</p>;
  return <ul className={styles.linkList}>{finding.evidence.map(ref => <li key={evidenceId(ref)}>
    <button type="button" onClick={() => onOpen(ref, finding.id)}>
      <span><strong>{evidenceLabel(ref)}</strong>{ref.source_run_id ? <small> · {ref.source_run_id}</small> : null}</span>
      <span className={styles.linkMeta}>在主区域打开</span>
    </button>
  </li>)}</ul>;
}

function FindingDetail({ taskId, finding, assetsById, merges, mergedInto, selected, inContext, actions }: {
  taskId: string;
  finding: ResearchFinding;
  assetsById: Map<string, ResearchAsset>;
  merges: FindingMerge[];
  mergedInto: Map<string, string>;
  selected: boolean;
  inContext: boolean;
  actions: SrcResearchFindingActions;
}) {
  const [fullReview, setFullReview] = useState(false);
  const primaryId = mergedInto.get(finding.id);
  const ownMerge = merges.find(merge => merge.primaryId === finding.id);
  return <section className={styles.detail} aria-label={`发现 ${finding.id}`}>
    <PaneHeader eyebrow={<>发现 · {finding.id} <OriginTag origin={finding.origin} /></>} title={finding.title} actions={<>
      <Button size="sm" variant={inContext ? "soft" : "secondary"} icon={inContext ? "check" : "messageCircle"} disabled={inContext} onClick={() => actions.onAddContext(findingRef(finding))}>{inContext ? "已在对话上下文" : "加入对话上下文"}</Button>
      <Button size="sm" variant="ghost" icon={selected ? "checkCheck" : "plus"} active={selected} onClick={() => actions.onToggle(finding.id)}>{selected ? "已选择" : "选择"}</Button>
      <IconButton icon="x" label="关闭发现详情" onClick={() => actions.onFocus("")} />
    </>} />
    {primaryId ? <Callout tone="accent" icon="layers" title={`已归入 ${primaryId}（本地演示）`} action={<Button size="xs" variant="secondary" onClick={() => actions.onFocus(primaryId)}>查看保留对象</Button>}>原始记录与来源仍完整保留，可随时撤销归并。</Callout> : null}
    {ownMerge ? <Callout tone="accent" icon="layers" title="归并保留对象（本地演示）" action={<Button size="xs" variant="secondary" icon="undo" onClick={() => actions.onUndoMerge(ownMerge.id)}>撤销归并</Button>}>
      包含 {ownMerge.memberIds.filter(id => id !== finding.id).join("、")}；这些原始记录未删除。
    </Callout> : null}
    {finding.stateNote ? <Callout tone="neutral" icon="info">{finding.stateNote}</Callout> : null}
    <Definitions rows={[
      ["等级 / 状态", <span key="badges" className={styles.inline}><SeverityBadge severity={finding.severity} /><FindingStateBadge state={finding.state} />{finding.readiness ? <code>{finding.readiness}</code> : null}</span>],
      ["来源 Run", finding.sourceRunId ? <RefChip key="run" refValue={{ kind: "run", id: finding.sourceRunId, label: finding.sourceRunId }} onOpen={actions.onOpenRef} /> : "未记录"],
      ["版本", `v${finding.version}${finding.targetVersion ? ` · ${finding.targetVersion}` : ""}`],
    ]} />
    <section className={styles.detailSection} aria-labelledby={`finding-assets-${finding.id}`}>
      <header><h3 id={`finding-assets-${finding.id}`}>受影响资产<span className={styles.count}>{finding.assetIds.length}</span></h3></header>
      {finding.assetIds.length ? <ul className={styles.linkList}>{finding.assetIds.map(id => { const asset = assetsById.get(id); return <li key={id}>
        <button type="button" onClick={() => actions.onOpenAsset(id)}><span><strong>{id}</strong> {asset?.key || "（资产库中未找到）"}</span><span className={styles.linkMeta}>{asset?.origin === "simulated" ? "模拟新增" : "打开资产"}</span></button>
      </li>; })}</ul> : <p className={styles.empty}>未关联资产。</p>}
    </section>
    <section className={styles.detailSection} aria-labelledby={`finding-evidence-${finding.id}`}>
      <header><h3 id={`finding-evidence-${finding.id}`}>原始证据<span className={styles.count}>{finding.evidence.length}</span></h3></header>
      <EvidenceButtons finding={finding} onOpen={actions.onOpenEvidence} />
    </section>
    {finding.limitations.length ? <section className={styles.detailSection} aria-labelledby={`finding-limits-${finding.id}`}>
      <header><h3 id={`finding-limits-${finding.id}`}>限制说明</h3></header>
      <ul className={styles.plainList}>{finding.limitations.map(item => <li key={item}>{item}</li>)}</ul>
    </section> : null}
    <section className={styles.detailSection}>
      <header><h3>完整审阅记录</h3><Button size="xs" variant="ghost" icon={fullReview ? "chevronUp" : "chevronDown"} aria-expanded={fullReview} disabled={finding.origin === "simulated"} onClick={() => setFullReview(value => !value)}>{fullReview ? "收起" : "展开"}</Button></header>
      {fullReview ? <div className={styles.embedded}><FindingInspector taskId={taskId} findingId={finding.id} onOpenEvidence={ref => actions.onOpenEvidence(ref, finding.id)} /></div> : <p className={styles.hint}>展开后使用 SRC 工作台的审阅组件读取主张、复核与去重记录。</p>}
    </section>
  </section>;
}

function MergePreview({ draft, findingsById, assetsById, actions }: {
  draft: { ids: string[]; primaryId: string };
  findingsById: Map<string, ResearchFinding>;
  assetsById: Map<string, ResearchAsset>;
  actions: SrcResearchFindingActions;
}) {
  const members = draft.ids.map(id => findingsById.get(id)).filter((row): row is ResearchFinding => Boolean(row));
  const primary = members.find(row => row.id === draft.primaryId) || members[0];
  const assets = [...new Set(members.flatMap(row => row.assetIds))];
  const evidence = uniqueEvidence(members);
  return <section className={styles.detail} aria-label="归并预览">
    <PaneHeader eyebrow="归并预览 · 本地演示" title={`${members.length} 条记录归为一组`} actions={<IconButton icon="x" label="取消归并预览" onClick={actions.onCancelMerge} />} />
    <Callout tone="neutral" icon="shield">预览不会删除或修改任何原始记录。确认后只在本页记录一条归并决定；后端尚未提供归并命令，刷新页面后不保留。</Callout>
    <fieldset className={styles.radioGroup}>
      <legend>保留对象</legend>
      {members.map(row => <label key={row.id} data-checked={row.id === primary?.id ? "true" : undefined}>
        <input type="radio" name="src-merge-primary" value={row.id} checked={row.id === primary?.id} onChange={() => actions.onMergePrimary(row.id)} />
        <span className={styles.radioBody}>
          <strong>{row.id} · {row.title}</strong>
          <span className={styles.inline}><SeverityBadge severity={row.severity} /><FindingStateBadge state={row.state} /><small>{row.sourceRunId || "来源未记录"} · v{row.version} · 证据 {row.evidence.length}</small></span>
        </span>
      </label>)}
    </fieldset>
    <section className={styles.detailSection} aria-labelledby="merge-sources">
      <header><h3 id="merge-sources">归并后仍保留的原始记录与来源</h3></header>
      <div className={styles.tableScroll}><table className={styles.table}>
        <thead><tr><th scope="col">记录</th><th scope="col">角色</th><th scope="col">来源 Run</th><th scope="col">证据</th></tr></thead>
        <tbody>{members.map(row => <tr key={row.id}>
          <td><code>{row.id}</code></td>
          <td>{row.id === primary?.id ? <Badge tone="accent">保留对象</Badge> : <Badge>归入 · 原记录保留</Badge>}</td>
          <td>{row.sourceRunId || "—"}</td>
          <td>{row.evidence.map(ref => evidenceId(ref)).join("、") || "—"}</td>
        </tr>)}</tbody>
      </table></div>
    </section>
    <section className={styles.detailSection} aria-labelledby="merge-union">
      <header><h3 id="merge-union">保留对象将汇总</h3></header>
      <p className={styles.hint}>资产 {assets.length} 项 · 证据 {evidence.length} 项（按编号去重）</p>
      <div className={styles.chipRow}>
        {assets.map(id => <RefChip key={id} compact refValue={{ kind: "asset", id, label: assetsById.get(id)?.key || id }} onOpen={ref => actions.onOpenAsset(ref.id)} />)}
        {evidence.map(ref => <RefChip key={evidenceId(ref)} compact refValue={evidenceRef(ref)} />)}
      </div>
    </section>
    <div className={styles.footerActions}>
      <Button size="sm" variant="primary" icon="layers" disabled={!primary} onClick={actions.onConfirmMerge}>确认归并（本地演示）</Button>
      <Button size="sm" variant="ghost" onClick={actions.onCancelMerge}>取消</Button>
    </div>
  </section>;
}

export function SrcResearchFindings({ taskId, snapshot, filter, page, selectedIds, focusId, mergeDraft, merges, mergedInto, findingsById, assetsById, contextRefs, actions }: {
  taskId: string;
  snapshot: ResearchSnapshot;
  filter: FindingFilter;
  page: number;
  selectedIds: string[];
  focusId: string;
  mergeDraft: { ids: string[]; primaryId: string } | null;
  merges: FindingMerge[];
  mergedInto: Map<string, string>;
  findingsById: Map<string, ResearchFinding>;
  assetsById: Map<string, ResearchAsset>;
  contextRefs: ResearchRef[];
  actions: SrcResearchFindingActions;
}) {
  const [showSuggestions, setShowSuggestions] = useState(true);
  const rows = useMemo(() => filterFindings(snapshot.findings, filter, mergedInto), [snapshot.findings, filter, mergedInto]);
  const stateCounts = useMemo(() => countBy(snapshot.findings, row => row.state), [snapshot.findings]);
  const suggestions = useMemo(() => suggestFindingGroups(snapshot.findings, merges), [snapshot.findings, merges]);
  const visible = rows.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
  const selected = selectedIds.map(id => findingsById.get(id)).filter((row): row is ResearchFinding => Boolean(row));
  const focus = focusId ? findingsById.get(focusId) : undefined;
  const pageAllSelected = visible.length > 0 && visible.every(row => selectedIds.includes(row.id));
  const primaries = new Map(merges.map(merge => [merge.primaryId, merge]));
  const inContext = (ref: ResearchRef) => contextRefs.some(row => row.kind === ref.kind && row.id === ref.id);
  const filterAsset = filter.assetId ? assetsById.get(filter.assetId) : undefined;
  const severityOptions: Array<FindingSeverity | "all"> = ["all", ...(Object.keys(FINDING_SEVERITY_LABELS) as FindingSeverity[]).filter(key => snapshot.findings.some(row => row.severity === key))];
  const stateOptions: Array<{ value: FindingState | "all"; label: string }> = [
    { value: "all", label: "全部" }, { value: "reviewed", label: "已复核" }, { value: "reviewing", label: "复核中" }, { value: "needs_evidence", label: "待补证" },
    ...(stateCounts.unreviewed ? [{ value: "unreviewed" as const, label: "未审阅" }] : []),
  ];
  const detail = mergeDraft ? <MergePreview draft={mergeDraft} findingsById={findingsById} assetsById={assetsById} actions={actions} />
    : focus ? <FindingDetail key={focus.id} taskId={taskId} finding={focus} assetsById={assetsById} merges={merges} mergedInto={mergedInto}
      selected={selectedIds.includes(focus.id)} inContext={inContext(findingRef(focus))} actions={actions} />
      : null;
  return <div className={styles.split} data-has-detail={detail ? "true" : undefined}>
    <section className={styles.listPane} aria-label="发现与证据">
      <PaneHeader eyebrow="发现与证据" title={<>发现<span className={styles.count}>{snapshot.findings.length}</span></>} />
      {suggestions.length ? <section className={styles.suggestions} aria-labelledby="src-finding-suggestions">
        <header>
          <h3 id="src-finding-suggestions"><span>建议分组</span><span className={styles.count}>{suggestions.length}</span></h3>
          <span className={styles.muted}>本地规则：同类标题且关联同一资产</span>
          <Button size="xs" variant="ghost" icon={showSuggestions ? "chevronUp" : "chevronDown"} aria-expanded={showSuggestions} onClick={() => setShowSuggestions(value => !value)}>{showSuggestions ? "收起" : "展开"}</Button>
        </header>
        {showSuggestions ? <ul>{suggestions.map(group => <li key={group.id}>
          <div><strong>{group.findingIds.join("、")}</strong><small>{group.reason}</small></div>
          <Button size="xs" variant="outline" icon="layers" onClick={() => actions.onStartMerge(group.findingIds.map(id => findingsById.get(id)).filter((row): row is ResearchFinding => Boolean(row)))}>预览归并</Button>
        </li>)}</ul> : null}
      </section> : null}
      <div className={styles.toolbar}>
        <SearchInput className={styles.search} value={filter.text} onValueChange={text => actions.onFilter({ text })} placeholder="搜索编号、标题、来源 Run 或资产" aria-label="搜索发现" />
        <SegmentedControl ariaLabel="发现状态" value={filter.state} onChange={state => actions.onFilter({ state })} options={stateOptions.map(option => ({
          value: option.value, label: <>{option.label}{option.value !== "all" ? <span className="cx-tabular text-cx-fg-4"> {stateCounts[option.value] || 0}</span> : null}</>,
        }))} />
        <Select<FindingSeverity | "all"> size="sm" ariaLabel="等级" value={filter.severity} onChange={severity => actions.onFilter({ severity })}
          options={severityOptions.map(value => ({ value, label: value === "all" ? "全部等级" : FINDING_SEVERITY_LABELS[value] }))} />
        <Checkbox checked={filter.hideMerged} onCheckedChange={value => actions.onFilter({ hideMerged: value })} label="隐藏已归入记录" />
      </div>
      {filter.assetId ? <div className={styles.activeFilter}>
        <span>仅显示关联</span>
        <RefChip refValue={{ kind: "asset", id: filter.assetId, label: `${filter.assetId}${filterAsset ? ` · ${filterAsset.key}` : ""}` }} onOpen={ref => actions.onOpenAsset(ref.id)} />
        <Button size="xs" variant="ghost" icon="x" onClick={() => actions.onFilter({ assetId: "" })}>清除资产筛选</Button>
      </div> : null}
      {selected.length ? <div className={styles.selectionBar} role="region" aria-label="已选发现">
        <strong className="cx-tabular">已选 {selected.length} 项</strong>
        <Button size="xs" variant="secondary" icon="messageCircle" onClick={() => actions.onAddContext(...selected.map(findingRef))}>加入对话上下文</Button>
        <Button size="xs" variant="secondary" icon="layers" disabled={selected.length < 2} tooltip={selected.length < 2 ? "至少选择 2 条发现" : undefined} onClick={() => actions.onStartMerge(selected)}>预览归并</Button>
        <Button size="xs" variant="ghost" onClick={() => actions.onSelect([])}>清除选择</Button>
      </div> : null}
      <p className={styles.resultLine} role="status">{rows.length} 条匹配{snapshot.findingsComplete ? "" : " · 来源分页未完整读取"}</p>
      {visible.length ? <div className={styles.tableScroll}>
        <table className={styles.table}>
          <thead><tr>
            <th scope="col" className={styles.checkCell}><input type="checkbox" aria-label="选择本页全部发现" checked={pageAllSelected} onChange={event => actions.onSelect(event.target.checked
              ? [...new Set([...selectedIds, ...visible.map(row => row.id)])]
              : selectedIds.filter(id => !visible.some(row => row.id === id)))} /></th>
            <th scope="col">发现</th><th scope="col">等级</th><th scope="col">状态</th><th scope="col">资产</th><th scope="col">证据</th>
          </tr></thead>
          <tbody>{visible.map(row => {
            const mergedTo = mergedInto.get(row.id);
            const merge = primaries.get(row.id);
            return <tr key={row.id} aria-selected={row.id === focusId} data-merged={mergedTo ? "true" : undefined} onClick={() => actions.onFocus(row.id)}>
              <td className={styles.checkCell} onClick={event => event.stopPropagation()}><input type="checkbox" aria-label={`选择 ${row.id}`} checked={selectedIds.includes(row.id)} onChange={() => actions.onToggle(row.id)} /></td>
              <td>
                <button type="button" className={styles.rowButton} onClick={event => { event.stopPropagation(); actions.onFocus(row.id); }} aria-current={row.id === focusId ? "true" : undefined}>
                  <strong>{row.title}</strong><small><code>{row.id}</code>{row.sourceRunId ? ` · ${row.sourceRunId}` : ""}</small>
                </button>
                <span className={styles.inline}>
                  {mergedTo ? <Badge tone="neutral" icon="layers">归入 {mergedTo}</Badge> : null}
                  {merge ? <Badge tone="accent" icon="layers">保留对象 · 含 {merge.memberIds.length - 1}</Badge> : null}
                  {row.stateNote ? <Badge tone="accent">已更新</Badge> : null}
                  {inContext(findingRef(row)) ? <Badge icon="messageCircle">上下文</Badge> : null}
                </span>
              </td>
              <td><SeverityBadge severity={row.severity} /></td>
              <td><FindingStateBadge state={row.state} /></td>
              <td>{row.assetIds.length ? <code>{row.assetIds[0]}{row.assetIds.length > 1 ? ` +${row.assetIds.length - 1}` : ""}</code> : "—"}</td>
              <td className="cx-tabular">{row.evidence.length}</td>
            </tr>;
          })}</tbody>
        </table>
      </div> : <EmptyState icon="search" title="没有匹配的发现" description="调整状态、等级或搜索词。" action={<Button size="sm" variant="secondary" onClick={() => actions.onFilter({ text: "", state: "all", severity: "all", assetId: "", hideMerged: false })}>清除筛选</Button>} />}
      <Pager page={page} pageSize={PAGE_SIZE} total={rows.length} onChange={actions.onPage} label="发现分页" />
    </section>
    {detail || <aside className={styles.detailEmpty}><EmptyState compact icon="listChecks" title="选择一条发现" description="查看受影响资产与原始证据；勾选多条可预览归并。" /></aside>}
  </div>;
}
