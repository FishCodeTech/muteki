"use client";

import { useMemo, useState } from "react";
import { Badge, Button, Callout } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import {
  countBy, FINDING_SEVERITY_LABELS, FINDING_STATE_LABELS, mentionedRefs,
  type FindingFilter, type FindingSeverity, type FindingState, type ResearchJob, type ResearchJobKind, type ResearchRef, type ResearchSnapshot,
} from "@/lib/srcResearch";
import { Definitions, formatTime, jobStateLabel, jobTone, OriginTag, PaneHeader, RefChip } from "./SrcResearchParts";
import styles from "./srcResearch.module.css";

export type SrcResearchDemoControls = { done: number; total: number; nextLabel?: string; onAdvance: () => void; onReset: () => void };

const GROUPS: Array<{ kind: ResearchJobKind; title: string; hint: string }> = [
  { kind: "import", title: "资产搜集与导入", hint: "导入批次是任务状态；结果进入可持续查询的资产库。" },
  { kind: "run", title: "Pentest 评估 Run", hint: "每个 Run 绑定具体目标与问题，状态来自 Run 记录。" },
  { kind: "monitor", title: "持续监控", hint: "监控按固定查询版本运行，变化写入下方变化记录。" },
];
const RUN_PREVIEW = 4;

function JobRow({ job, onOpenRef }: { job: ResearchJob; onOpenRef: (ref: ResearchRef) => void }) {
  const progress = job.total ? Math.round(((job.done || 0) / job.total) * 100) : undefined;
  const primary = job.refs[0];
  return <li className={styles.jobRow} data-state={job.state}>
    <div className={styles.jobMain}>
      <div className={styles.jobTitle}>
        <Badge tone={jobTone(job.state)} dot>{jobStateLabel(job.state)}</Badge>
        <strong>{job.title}</strong>
        <OriginTag origin={job.origin} />
      </div>
      <p className={styles.jobMeta}>
        <code>{job.id}</code>
        {job.detail ? <span>{job.detail}</span> : null}
        {job.updatedAt ? <span>更新于 {formatTime(job.updatedAt)}</span> : null}
      </p>
      {job.counts ? <p className={styles.jobCounts}>{Object.entries(job.counts).map(([key, value]) => <span key={key}>{key} <b className="cx-tabular">{value}</b></span>)}</p> : null}
      {progress !== undefined && job.state !== "completed" ? <div className={styles.progressTrack} role="progressbar" aria-label={`${job.id} 进度`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={progress}><span style={{ width: `${progress}%` }} /></div> : null}
      {job.refs.length > 1 ? <div className={styles.chipRow}>{job.refs.slice(1).map(ref => <RefChip key={`${ref.kind}:${ref.id}`} refValue={ref} compact onOpen={onOpenRef} />)}</div> : null}
    </div>
    {primary ? <Button size="xs" variant="outline" iconRight="chevronRight" onClick={() => onOpenRef(primary)}>打开</Button> : null}
  </li>;
}

export function SrcResearchProgress({ snapshot, conversationTexts, onOpenRef, onAddContext, onFilterFindings, demo }: {
  snapshot: ResearchSnapshot;
  conversationTexts: string[];
  onOpenRef: (ref: ResearchRef) => void;
  onAddContext: (ref: ResearchRef) => void;
  onFilterFindings: (patch: Partial<FindingFilter>) => void;
  demo?: SrcResearchDemoControls;
}) {
  const [allRuns, setAllRuns] = useState(false);
  const active = snapshot.jobs.filter(job => ["running", "dispatched", "pending", "waiting_approval"].includes(job.state));
  const stateCounts = useMemo(() => countBy(snapshot.findings, row => row.state), [snapshot.findings]);
  const severityCounts = useMemo(() => countBy(snapshot.findings, row => row.severity), [snapshot.findings]);
  const mentioned = useMemo(() => mentionedRefs(conversationTexts, snapshot), [conversationTexts, snapshot]);
  const stateOrder: FindingState[] = ["reviewed", "reviewing", "needs_evidence", "unreviewed"];
  const severityOrder: FindingSeverity[] = ["critical", "high", "medium", "low", "unrated"];
  return <div className={styles.viewScroll}>
    <PaneHeader eyebrow="研究进展" title="正在发生什么" actions={<span className={styles.muted}>数据来源：任务、Run、导入批次与监控记录</span>} />
    <div className={styles.progressGrid}>
      <div className={styles.progressColumn}>
        <section className={styles.card} aria-labelledby="src-research-now">
          <header className={styles.cardHeader}><h3 id="src-research-now">进行中</h3><span className={styles.muted}>{active.length ? `${active.length} 项` : "无"}</span></header>
          {active.length ? <ul className={styles.jobList}>{active.map(job => <JobRow key={job.id} job={job} onOpenRef={onOpenRef} />)}</ul>
            : <p className={styles.empty}>当前没有进行中的导入、Run 或监控周期。下方保留已完成任务的记录。</p>}
          {demo ? <div className={styles.demoControl}>
            <div>
              <strong>演示进展（本地模拟）</strong>
              <p>{demo.nextLabel ? `下一步：${demo.nextLabel}。` : "演示脚本已全部完成。"}由你手动推进，仅改变本页状态，不连接 MCP、Agent 或任何网络目标。</p>
            </div>
            <div className={styles.demoActions}>
              <Button size="sm" variant="primary" icon="play" disabled={!demo.nextLabel} onClick={demo.onAdvance}>{demo.nextLabel ? `推进一步 ${demo.done + 1}/${demo.total}` : "已完成"}</Button>
              <Button size="sm" variant="ghost" icon="undo" disabled={!demo.done} onClick={demo.onReset}>重置</Button>
            </div>
          </div> : null}
        </section>

        {GROUPS.map(group => {
          const rows = snapshot.jobs.filter(job => job.kind === group.kind);
          const visible = group.kind === "run" && !allRuns ? rows.slice(0, RUN_PREVIEW) : rows;
          return <section key={group.kind} className={styles.card} aria-labelledby={`src-research-${group.kind}`}>
            <header className={styles.cardHeader}>
              <h3 id={`src-research-${group.kind}`}>{group.title}<span className={styles.count}>{rows.length}</span></h3>
              {group.kind === "run" && rows.length > RUN_PREVIEW ? <Button size="xs" variant="ghost" onClick={() => setAllRuns(value => !value)}>{allRuns ? "收起" : `显示全部 ${rows.length} 个`}</Button> : null}
            </header>
            <p className={styles.hint}>{group.hint}</p>
            {rows.length ? <ul className={styles.jobList}>{visible.map(job => <JobRow key={job.id} job={job} onOpenRef={onOpenRef} />)}</ul> : <p className={styles.empty}>暂无记录。</p>}
          </section>;
        })}
      </div>

      <div className={styles.progressColumn}>
        <section className={styles.card} aria-labelledby="src-research-queue">
          <header className={styles.cardHeader}><h3 id="src-research-queue">发现队列<span className={styles.count}>{snapshot.findings.length}</span></h3></header>
          <p className={styles.hint}>点击状态或等级，直接进入已筛选的发现列表。</p>
          <div className={styles.filterLinks} role="group" aria-label="按状态查看发现">
            {stateOrder.filter(state => stateCounts[state]).map(state => <button key={state} type="button" onClick={() => onFilterFindings({ state, severity: "all" })}>
              <span>{FINDING_STATE_LABELS[state]}</span><b className="cx-tabular">{stateCounts[state]}</b><Icon name="chevronRight" size={12} />
            </button>)}
          </div>
          <div className={styles.filterLinks} role="group" aria-label="按等级查看发现">
            {severityOrder.filter(severity => severityCounts[severity]).map(severity => <button key={severity} type="button" onClick={() => onFilterFindings({ severity, state: "all" })}>
              <span>{FINDING_SEVERITY_LABELS[severity]}</span><b className="cx-tabular">{severityCounts[severity]}</b><Icon name="chevronRight" size={12} />
            </button>)}
          </div>
        </section>

        <section className={styles.card} aria-labelledby="src-research-changes">
          <header className={styles.cardHeader}><h3 id="src-research-changes">最新变化<span className={styles.count}>{snapshot.changes.length}</span></h3></header>
          {snapshot.changes.length ? <ol className={styles.changeList}>{snapshot.changes.slice(0, 30).map(change => <li key={change.id}>
            <div className={styles.changeHead}><time dateTime={change.at}>{formatTime(change.at)}</time><OriginTag origin={change.origin} /></div>
            <p>{change.summary}</p>
            {change.refs.length ? <div className={styles.chipRow}>{change.refs.map(ref => <RefChip key={`${ref.kind}:${ref.id}`} refValue={ref} compact onOpen={onOpenRef} />)}</div> : null}
          </li>)}</ol> : <p className={styles.empty}>尚无变化记录。</p>}
        </section>

        <section className={styles.card} aria-labelledby="src-research-mentions">
          <header className={styles.cardHeader}><h3 id="src-research-mentions">对话中提及的对象<span className={styles.count}>{mentioned.length}</span></h3></header>
          <p className={styles.hint}>按对话原文中出现的编号匹配当前研究的资产、发现与证据，不推断对话以外的内容。</p>
          {mentioned.length ? <ul className={styles.mentionList}>{mentioned.map(ref => <li key={`${ref.kind}:${ref.id}`}>
            <RefChip refValue={ref} onOpen={onOpenRef} />
            <Button size="xs" variant="ghost" icon="plus" onClick={() => onAddContext(ref)}>加入上下文</Button>
          </li>)}</ul> : <p className={styles.empty}>对话里尚未提及可识别的对象编号。</p>}
        </section>

        <section className={styles.card} aria-labelledby="src-research-scope">
          <header className={styles.cardHeader}><h3 id="src-research-scope">研究目标与边界</h3></header>
          <Definitions rows={[["目标", snapshot.goal || "未记录"], ["授权范围", snapshot.scope.join("\n") || "未记录"], ["排除", snapshot.exclusions.join("\n") || "未记录"]]} />
        </section>
        {snapshot.sourceErrors.length ? <Callout tone="warning" title="部分来源未能读取">{snapshot.sourceErrors.join("\n")}</Callout> : null}
      </div>
    </div>
  </div>;
}
