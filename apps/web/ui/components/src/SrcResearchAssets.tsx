"use client";

import { useMemo, useState } from "react";
import { Badge, Button, Checkbox, EmptyState, IconButton, SearchInput, SegmentedControl } from "@/components/chat/ui";
import {
  assetRef, assetTypeLabel, countBy, filterAssets,
  type AssetFilter, type ResearchAsset, type ResearchFinding, type ResearchRef, type ResearchSnapshot,
} from "@/lib/srcResearch";
import { AssetInspector } from "./AssetInspector";
import { Definitions, FindingStateBadge, formatTime, OriginTag, PaneHeader, Pager, RefChip, SeverityBadge } from "./SrcResearchParts";
import styles from "./srcResearch.module.css";

const PAGE_SIZE = 50;

function AssetDetail({ taskId, asset, snapshot, findingsById, inContext, onClose, onAddContext, onOpenFindings, onOpenFinding, onOpenRef, onSelectAsset }: {
  taskId: string;
  asset: ResearchAsset;
  snapshot: ResearchSnapshot;
  findingsById: Map<string, ResearchFinding>;
  inContext: boolean;
  onClose: () => void;
  onAddContext: (ref: ResearchRef) => void;
  onOpenFindings: (assetId: string) => void;
  onOpenFinding: (findingId: string, assetId: string) => void;
  onOpenRef: (ref: ResearchRef) => void;
  onSelectAsset: (assetId: string) => void;
}) {
  const [tab, setTab] = useState<"summary" | "record">("summary");
  const findings = asset.findingIds.map(id => findingsById.get(id)).filter((row): row is ResearchFinding => Boolean(row));
  const imports = snapshot.jobs.filter(job => job.kind === "import" && (job.refs.some(ref => ref.id === asset.id) || (asset.origin === "record" && job.origin === "record")));
  return <section className={styles.detail} aria-label={`资产 ${asset.id}`}>
    <PaneHeader eyebrow={<>资产 · {asset.id} <OriginTag origin={asset.origin} /></>} title={asset.name} actions={<>
      <Button size="sm" variant={inContext ? "soft" : "secondary"} icon={inContext ? "check" : "messageCircle"} disabled={inContext} onClick={() => onAddContext(assetRef(asset))}>{inContext ? "已在对话上下文" : "加入对话上下文"}</Button>
      <IconButton icon="x" label="关闭资产详情" onClick={onClose} />
    </>} />
    <SegmentedControl ariaLabel="资产详情视图" value={tab} onChange={setTab} options={[
      { value: "summary", label: "概要与关联" },
      { value: "record", label: "完整档案", disabled: asset.origin === "simulated" },
    ]} />
    {asset.origin === "simulated" ? <p className={styles.hint}>本地模拟新增的资产没有后端档案，只在本页状态中存在。</p> : null}
    {tab === "summary" ? <>
      <Definitions rows={[
        ["标识", <code key="key">{asset.key}</code>],
        ["类型", assetTypeLabel(asset.type)],
        ["快照版本", `v${asset.revision}`],
        ["首次 / 最近观察", `${formatTime(asset.firstSeenAt)} / ${formatTime(asset.lastObservedAt)}`],
        ["来源", asset.source || "未记录"],
        ["归属 / 环境", [asset.owner, asset.environment].filter(Boolean).join(" · ") || "未记录"],
        ["标签", asset.tags.length ? asset.tags.join("、") : "—"],
      ]} />
      <section className={styles.detailSection} aria-labelledby={`asset-findings-${asset.id}`}>
        <header><h3 id={`asset-findings-${asset.id}`}>关联发现<span className={styles.count}>{findings.length}</span></h3>
          {findings.length ? <Button size="xs" variant="ghost" iconRight="chevronRight" onClick={() => onOpenFindings(asset.id)}>在发现中筛选</Button> : null}</header>
        {findings.length ? <ul className={styles.linkList}>{findings.map(finding => <li key={finding.id}>
          <button type="button" onClick={() => onOpenFinding(finding.id, asset.id)}>
            <span><strong>{finding.id}</strong> {finding.title}</span>
            <span className={styles.linkMeta}><SeverityBadge severity={finding.severity} /><FindingStateBadge state={finding.state} /></span>
          </button>
        </li>)}</ul> : <p className={styles.empty}>当前没有发现关联到此资产。</p>}
      </section>
      <section className={styles.detailSection} aria-labelledby={`asset-sources-${asset.id}`}>
        <header><h3 id={`asset-sources-${asset.id}`}>来源记录</h3></header>
        {imports.length ? <div className={styles.chipRow}>{imports.map(job => <RefChip key={job.id} refValue={{ kind: "import", id: job.id, label: `${job.id} · ${job.title}` }} onOpen={onOpenRef} />)}</div> : <p className={styles.empty}>没有可关联的导入批次。</p>}
      </section>
    </> : <div className={styles.embedded}><AssetInspector taskId={taskId} assetId={asset.id} onSelectAsset={onSelectAsset} /></div>}
  </section>;
}

export function SrcResearchAssets({ taskId, snapshot, filter, page, selectedAssetId, findingsById, contextRefs, onFilter, onPage, onSelect, onAddContext, onOpenFindings, onOpenFinding, onOpenRef }: {
  taskId: string;
  snapshot: ResearchSnapshot;
  filter: AssetFilter;
  page: number;
  selectedAssetId: string;
  findingsById: Map<string, ResearchFinding>;
  contextRefs: ResearchRef[];
  onFilter: (patch: Partial<AssetFilter>) => void;
  onPage: (page: number) => void;
  onSelect: (assetId: string) => void;
  onAddContext: (ref: ResearchRef) => void;
  onOpenFindings: (assetId: string) => void;
  onOpenFinding: (findingId: string, assetId: string) => void;
  onOpenRef: (ref: ResearchRef) => void;
}) {
  const typeCounts = useMemo(() => countBy(snapshot.assets, row => row.type), [snapshot.assets]);
  const rows = useMemo(() => filterAssets(snapshot.assets, filter), [snapshot.assets, filter]);
  const visible = rows.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
  const selected = snapshot.assets.find(row => row.id === selectedAssetId);
  const simulated = snapshot.assets.filter(row => row.origin === "simulated").length;
  const linked = snapshot.assets.filter(row => row.findingIds.length).length;
  return <div className={styles.split} data-has-detail={selected ? "true" : undefined}>
    <section className={styles.listPane} aria-label="资产库">
      <PaneHeader eyebrow="资产库 · 可持续查询的资料" title={<>资产<span className={styles.count}>{snapshot.assetTotal.toLocaleString()}</span></>}
        actions={<Button size="sm" variant="ghost" icon="activity" onClick={() => onOpenRef({ kind: "import", id: "imports", label: "导入批次" })}>导入与搜集进展</Button>} />
      <div className={styles.toolbar}>
        <SearchInput className={styles.search} value={filter.text} onValueChange={text => onFilter({ text })} placeholder="搜索 ID、域名、名称或标签" aria-label="搜索资产" />
        <SegmentedControl ariaLabel="资产类型" value={filter.type} onChange={type => onFilter({ type })} options={[
          { value: "all", label: "全部" },
          ...Object.entries(typeCounts).map(([type, count]) => ({ value: type, label: <>{assetTypeLabel(type)} <span className="cx-tabular text-cx-fg-4">{count}</span></> })),
        ]} />
        <SegmentedControl ariaLabel="关联发现" value={filter.linkage} onChange={linkage => onFilter({ linkage })} options={[
          { value: "all", label: "全部" }, { value: "linked", label: <>有关联发现 <span className="cx-tabular text-cx-fg-4">{linked}</span></> }, { value: "unlinked", label: "无关联" },
        ]} />
        {simulated ? <Checkbox checked={filter.simulatedOnly} onCheckedChange={value => onFilter({ simulatedOnly: value })} label={`仅本地模拟新增（${simulated}）`} /> : null}
      </div>
      <p className={styles.resultLine} role="status">{rows.length.toLocaleString()} 项匹配{snapshot.assetsComplete ? "" : ` · 已载入 ${snapshot.assets.length.toLocaleString()} / ${snapshot.assetTotal.toLocaleString()}，其余未读取`}</p>
      {visible.length ? <div className={styles.tableScroll}>
        <table className={styles.table}>
          <thead><tr><th scope="col">资产</th><th scope="col">类型</th><th scope="col">关联发现</th><th scope="col">最近观察</th></tr></thead>
          <tbody>{visible.map(asset => <tr key={asset.id} aria-selected={asset.id === selectedAssetId} onClick={() => onSelect(asset.id)}>
            <td><button type="button" className={styles.rowButton} onClick={event => { event.stopPropagation(); onSelect(asset.id); }} aria-current={asset.id === selectedAssetId ? "true" : undefined}>
              <strong>{asset.name}</strong><small><code>{asset.id}</code> · {asset.key}</small>
            </button>{asset.origin === "simulated" ? <Badge tone="accent">模拟新增</Badge> : null}{contextRefs.some(ref => ref.kind === "asset" && ref.id === asset.id) ? <Badge tone="neutral" icon="messageCircle">上下文</Badge> : null}</td>
            <td>{assetTypeLabel(asset.type)}</td>
            <td className="cx-tabular">{asset.findingIds.length || "—"}</td>
            <td>{formatTime(asset.lastObservedAt)}</td>
          </tr>)}</tbody>
        </table>
      </div> : <EmptyState icon="search" title="没有匹配的资产" description="调整搜索词或筛选条件。" action={<Button size="sm" variant="secondary" onClick={() => onFilter({ text: "", type: "all", linkage: "all", simulatedOnly: false })}>清除筛选</Button>} />}
      <Pager page={page} pageSize={PAGE_SIZE} total={rows.length} onChange={onPage} label="资产分页" />
    </section>
    {selected ? <AssetDetail key={selected.id} taskId={taskId} asset={selected} snapshot={snapshot} findingsById={findingsById}
      inContext={contextRefs.some(ref => ref.kind === "asset" && ref.id === selected.id)}
      onClose={() => onSelect("")} onAddContext={onAddContext} onOpenFindings={onOpenFindings} onOpenFinding={onOpenFinding} onOpenRef={onOpenRef} onSelectAsset={onSelect} />
      : <aside className={styles.detailEmpty}><EmptyState compact icon="database" title="选择一项资产" description="查看来源、关联发现，并可把它加入当前对话上下文。" /></aside>}
  </div>;
}
