/**
 * One research-object model shared by the SRC progress, asset and finding views.
 * Every view reads the same snapshot; demo fixtures only differ as a data source.
 */

export type ResearchRefKind = "task" | "asset" | "finding" | "evidence" | "run" | "import" | "monitor";
export type ResearchRef = { kind: ResearchRefKind; id: string; label: string };

export type ResearchEvidence = {
  id?: string;
  artifact_id?: string;
  exchange_id?: string;
  label?: string;
  sha256?: string;
  source_run_id?: string | null;
  source_event_seq?: number | null;
  complete?: string;
  media_type?: string;
  name?: string;
};

export type ResearchOrigin = "record" | "simulated";

export type ResearchAsset = {
  id: string;
  key: string;
  name: string;
  type: string;
  revision: number;
  firstSeenAt?: string;
  lastObservedAt?: string;
  source?: string;
  owner?: string;
  environment?: string;
  tags: string[];
  findingIds: string[];
  origin: ResearchOrigin;
};

export type FindingState = "reviewed" | "reviewing" | "needs_evidence" | "unreviewed";
export type FindingSeverity = "critical" | "high" | "medium" | "low" | "unrated";

export type ResearchFinding = {
  id: string;
  claimId?: string;
  title: string;
  category: string;
  severity: FindingSeverity;
  readiness?: string;
  state: FindingState;
  stateNote?: string;
  sourceRunId?: string;
  version: number;
  targetVersion?: string;
  assetIds: string[];
  evidence: ResearchEvidence[];
  limitations: string[];
  origin: ResearchOrigin;
};

export type ResearchJobKind = "import" | "run" | "monitor" | "queue";
export type ResearchJob = {
  id: string;
  kind: ResearchJobKind;
  title: string;
  state: string;
  detail?: string;
  done?: number;
  total?: number;
  counts?: Record<string, number>;
  updatedAt?: string;
  refs: ResearchRef[];
  origin: ResearchOrigin;
};

export type ResearchChangeOrigin = "record" | "simulated" | "chat" | "local";
export type ResearchChange = { id: string; at: string; summary: string; refs: ResearchRef[]; origin: ResearchChangeOrigin };

export type ResearchSnapshot = {
  taskId: string;
  threadId: string;
  title: string;
  goal: string;
  scope: string[];
  exclusions: string[];
  demo: boolean;
  disclaimer?: string;
  assets: ResearchAsset[];
  assetTotal: number;
  assetsComplete: boolean;
  findings: ResearchFinding[];
  findingsComplete: boolean;
  jobs: ResearchJob[];
  changes: ResearchChange[];
  sourceErrors: string[];
};

export const FINDING_STATE_LABELS: Record<FindingState, string> = {
  reviewed: "已复核", reviewing: "复核中", needs_evidence: "待补证", unreviewed: "未审阅",
};
export const FINDING_SEVERITY_LABELS: Record<FindingSeverity, string> = {
  critical: "严重", high: "高危", medium: "中危", low: "低危", unrated: "未评级",
};
export const ASSET_TYPE_LABELS: Record<string, string> = {
  subdomain: "子域名", service: "Web 与服务", api: "API", cdn: "CDN", endpoint: "端点", host: "主机",
};
export const REF_KIND_LABELS: Record<ResearchRefKind, string> = {
  task: "研究", asset: "资产", finding: "发现", evidence: "证据", run: "Run", import: "导入", monitor: "监控",
};

const STATE_ALIASES: Record<string, FindingState> = {
  已复核: "reviewed", reviewed: "reviewed", GREEN: "reviewed",
  复核中: "reviewing", reviewing: "reviewing", YELLOW: "reviewing",
  待补证: "needs_evidence", awaiting_evidence: "needs_evidence", needs_evidence: "needs_evidence", RED: "needs_evidence",
};

export function findingState(state?: unknown, readiness?: unknown): FindingState {
  return STATE_ALIASES[String(state || "")] || STATE_ALIASES[String(readiness || "")] || "unreviewed";
}

export function findingSeverity(value?: unknown): FindingSeverity {
  const key = String(value || "").toLowerCase();
  return key === "critical" || key === "high" || key === "medium" || key === "low" ? key : "unrated";
}

export function findingCategory(title: string): string {
  return title.split(/\s+·\s+/)[0]?.replace(/（虚构）$/, "").trim() || title;
}

export function assetTypeLabel(type: string): string {
  return ASSET_TYPE_LABELS[type] || type;
}

export function evidenceId(ref: ResearchEvidence): string {
  return ref.exchange_id || ref.artifact_id || ref.sha256 || ref.id || "";
}

export function evidenceRef(ref: ResearchEvidence): ResearchRef {
  const id = evidenceId(ref);
  return { kind: "evidence", id, label: ref.exchange_id ? `${ref.exchange_id}${ref.label ? ` · ${ref.label}` : ""}` : ref.label || ref.name || id };
}

export function assetRef(asset: ResearchAsset): ResearchRef {
  return { kind: "asset", id: asset.id, label: `${asset.id} · ${asset.key}` };
}

export function findingRef(finding: ResearchFinding): ResearchRef {
  return { kind: "finding", id: finding.id, label: `${finding.id} · ${finding.title}` };
}

export function sameRef(a: ResearchRef, b: ResearchRef): boolean {
  return a.kind === b.kind && a.id === b.id;
}

/** Rebuilds the asset → finding back-links from each finding's affected assets. */
export function linkAssets(assets: ResearchAsset[], findings: ResearchFinding[]): ResearchAsset[] {
  const byAsset = new Map<string, string[]>();
  findings.forEach(finding => finding.assetIds.forEach(id => byAsset.set(id, [...(byAsset.get(id) || []), finding.id])));
  return assets.map(asset => ({ ...asset, findingIds: byAsset.get(asset.id) || [] }));
}

export type AssetFilter = { text: string; type: string; linkage: "all" | "linked" | "unlinked"; simulatedOnly: boolean };
export const DEFAULT_ASSET_FILTER: AssetFilter = { text: "", type: "all", linkage: "all", simulatedOnly: false };

export function filterAssets(assets: ResearchAsset[], filter: AssetFilter): ResearchAsset[] {
  const term = filter.text.trim().toLowerCase();
  return assets.filter(asset => (filter.type === "all" || asset.type === filter.type)
    && (filter.linkage === "all" || (filter.linkage === "linked" ? asset.findingIds.length > 0 : asset.findingIds.length === 0))
    && (!filter.simulatedOnly || asset.origin === "simulated")
    && (!term || [asset.id, asset.key, asset.name, asset.type, ...asset.tags].some(value => value.toLowerCase().includes(term))));
}

export type FindingFilter = { text: string; state: FindingState | "all"; severity: FindingSeverity | "all"; assetId: string; hideMerged: boolean };
export const DEFAULT_FINDING_FILTER: FindingFilter = { text: "", state: "all", severity: "all", assetId: "", hideMerged: false };

export function filterFindings(findings: ResearchFinding[], filter: FindingFilter, mergedInto: Map<string, string>): ResearchFinding[] {
  const term = filter.text.trim().toLowerCase();
  return findings.filter(finding => (filter.state === "all" || finding.state === filter.state)
    && (filter.severity === "all" || finding.severity === filter.severity)
    && (!filter.assetId || finding.assetIds.includes(filter.assetId))
    && (!filter.hideMerged || !mergedInto.has(finding.id))
    && (!term || [finding.id, finding.title, finding.sourceRunId || "", ...finding.assetIds].some(value => value.toLowerCase().includes(term))));
}

export function countBy<T, K extends string>(rows: T[], key: (row: T) => K): Record<K, number> {
  return rows.reduce((acc, row) => { const value = key(row); acc[value] = (acc[value] || 0) + 1; return acc; }, {} as Record<K, number>);
}

export type FindingMerge = { id: string; primaryId: string; memberIds: string[]; createdAt: string };

/** member id → retained primary id. The primary itself is not a key. */
export function mergedIndex(merges: FindingMerge[]): Map<string, string> {
  const index = new Map<string, string>();
  merges.forEach(merge => merge.memberIds.forEach(id => { if (id !== merge.primaryId) index.set(id, merge.primaryId); }));
  return index;
}

const STATE_RANK: Record<FindingState, number> = { reviewed: 0, reviewing: 1, needs_evidence: 2, unreviewed: 3 };
const SEVERITY_RANK: Record<FindingSeverity, number> = { critical: 0, high: 1, medium: 2, low: 3, unrated: 4 };

/** Proposes the most complete record to keep: best review state, then severity, then evidence count. */
export function pickPrimary(findings: ResearchFinding[]): ResearchFinding | undefined {
  return [...findings].sort((a, b) => STATE_RANK[a.state] - STATE_RANK[b.state]
    || SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity]
    || b.evidence.length - a.evidence.length
    || a.id.localeCompare(b.id))[0];
}

export function uniqueEvidence(findings: ResearchFinding[]): ResearchEvidence[] {
  const seen = new Map<string, ResearchEvidence>();
  findings.forEach(finding => finding.evidence.forEach(ref => { const id = evidenceId(ref); if (id && !seen.has(id)) seen.set(id, ref); }));
  return [...seen.values()];
}

export type FindingGroupSuggestion = { id: string; findingIds: string[]; category: string; sharedAssetIds: string[]; reason: string };

/** Local rule only: same category and at least one shared affected asset. Already merged findings are skipped. */
export function suggestFindingGroups(findings: ResearchFinding[], merges: FindingMerge[]): FindingGroupSuggestion[] {
  const merged = new Set(merges.flatMap(merge => merge.memberIds));
  const candidates = findings.filter(finding => !merged.has(finding.id));
  const parent = new Map(candidates.map(finding => [finding.id, finding.id]));
  const root = (id: string): string => { const next = parent.get(id) || id; if (next === id) return id; const value = root(next); parent.set(id, value); return value; };
  const firstByKey = new Map<string, string>();
  candidates.forEach(finding => finding.assetIds.forEach(assetId => {
    const key = `${finding.category}\u0000${assetId}`;
    const first = firstByKey.get(key);
    if (first) parent.set(root(finding.id), root(first)); else firstByKey.set(key, finding.id);
  }));
  const groups = new Map<string, ResearchFinding[]>();
  candidates.forEach(finding => { const id = root(finding.id); groups.set(id, [...(groups.get(id) || []), finding]); });
  return [...groups.values()].filter(rows => rows.length > 1).map(rows => {
    const counts = countBy(rows.flatMap(row => row.assetIds), id => id);
    const shared = Object.entries(counts).filter(([, count]) => count > 1).map(([id]) => id);
    const ids = rows.map(row => row.id).sort();
    return { id: `group-${ids.join("-")}`, findingIds: ids, category: rows[0].category, sharedAssetIds: shared, reason: `同类「${rows[0].category}」且共同关联 ${shared.join("、")}` };
  }).sort((a, b) => a.findingIds[0].localeCompare(b.findingIds[0]));
}

const MENTION = /\b(?:F-\d{3,}|A-\d{4,}|EX-\d+|ART-\d+)\b/g;

/** Objects literally named in conversation text that exist in the snapshot. */
export function mentionedRefs(texts: string[], snapshot: ResearchSnapshot): ResearchRef[] {
  const findings = new Map(snapshot.findings.map(row => [row.id, row]));
  const assets = new Map(snapshot.assets.map(row => [row.id, row]));
  const evidence = new Map(uniqueEvidence(snapshot.findings).flatMap(ref => [ref.exchange_id, ref.artifact_id].filter(Boolean).map(id => [id as string, ref] as const)));
  const refs = new Map<string, ResearchRef>();
  texts.forEach(text => (text.match(MENTION) || []).forEach(token => {
    const finding = findings.get(token); const asset = assets.get(token); const ref = evidence.get(token);
    const value = finding ? findingRef(finding) : asset ? assetRef(asset) : ref ? { ...evidenceRef(ref), id: token, label: token } : null;
    if (value) refs.set(`${value.kind}:${value.id}`, value);
  }));
  return [...refs.values()];
}

/** Progress that a source reports after the snapshot was read (for example a user-triggered demo step). */
export type ResearchOverlay = {
  assets?: ResearchAsset[];
  findingPatches?: Record<string, { state?: FindingState; stateNote?: string; addAssetIds?: string[] }>;
  jobs?: ResearchJob[];
  changes?: ResearchChange[];
};

export function applyOverlays(snapshot: ResearchSnapshot, overlays: ResearchOverlay[]): ResearchSnapshot {
  if (!overlays.length) return snapshot;
  let assets = snapshot.assets; let findings = snapshot.findings; let jobs = snapshot.jobs; let changes = snapshot.changes;
  let added = 0;
  overlays.forEach(overlay => {
    if (overlay.assets?.length) { assets = [...assets, ...overlay.assets]; added += overlay.assets.length; }
    const patches = overlay.findingPatches;
    if (patches) findings = findings.map(finding => {
      const patch = patches[finding.id];
      if (!patch) return finding;
      return { ...finding, state: patch.state || finding.state, stateNote: patch.stateNote || finding.stateNote, assetIds: [...new Set([...finding.assetIds, ...(patch.addAssetIds || [])])] };
    });
    overlay.jobs?.forEach(job => { jobs = jobs.some(row => row.id === job.id) ? jobs.map(row => row.id === job.id ? job : row) : [job, ...jobs]; });
    if (overlay.changes?.length) changes = [...overlay.changes, ...changes];
  });
  return { ...snapshot, assets: linkAssets(assets, findings), findings, jobs, changes, assetTotal: snapshot.assetTotal + added };
}
