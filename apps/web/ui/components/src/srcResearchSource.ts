import { fetchSrcTask, listSrcRuns } from "@/lib/srcApi";
import { SRC_DEMO_DISCLAIMER } from "@/lib/srcDemoData";
import {
  findingCategory, findingSeverity, findingState, linkAssets,
  type ResearchAsset, type ResearchChange, type ResearchEvidence, type ResearchFinding, type ResearchJob, type ResearchSnapshot,
} from "@/lib/srcResearch";
import type { SrcRun } from "@/lib/srcTypes";
import { assetMonitoringApi } from "./assetMonitoringApi";
import type { ApiRecord, Asset } from "./assetMonitoringTypes";
import { errorMessage, querySrc } from "./reviewPrimitives";

/** Upper bound for one in-browser snapshot; larger libraries are reported as incomplete instead of silently truncated. */
const ASSET_CAP = 5000;
const PAGE = 1000;

const text = (value: unknown) => typeof value === "string" ? value : "";

function assetFromRecord(asset: Asset): ResearchAsset {
  const profile = (asset.profile || {}) as ApiRecord;
  return {
    id: asset.asset_id,
    key: asset.canonical_key,
    name: text(asset.display_name) || asset.canonical_key,
    type: asset.asset_type,
    revision: asset.revision,
    firstSeenAt: asset.first_seen_at || undefined,
    lastObservedAt: asset.last_observed_at || undefined,
    source: text(profile.source) || undefined,
    owner: text(profile.owner) || undefined,
    environment: text(profile.environment) || undefined,
    tags: (asset.tags || []).map(tag => typeof tag === "string" ? tag : text(tag.name) || text(tag.tag_id)).filter(Boolean),
    findingIds: [],
    origin: "record",
  };
}

/** A reference that names both a captured exchange and its raw artifact becomes two openable evidence entries. */
function splitEvidence(refs: unknown): ResearchEvidence[] {
  if (!Array.isArray(refs)) return [];
  return refs.flatMap(raw => {
    const ref = raw as ResearchEvidence;
    const rows: ResearchEvidence[] = [];
    if (ref.exchange_id) rows.push({ exchange_id: ref.exchange_id, label: ref.label || "请求与响应", source_run_id: ref.source_run_id, complete: ref.complete });
    if (ref.artifact_id || ref.sha256) rows.push({ artifact_id: ref.artifact_id, sha256: ref.sha256, name: ref.name, media_type: ref.media_type, label: ref.name || ref.artifact_id, source_run_id: ref.source_run_id, complete: ref.complete });
    if (!rows.length && ref.id) rows.push(ref);
    return rows;
  });
}

function findingFromClaim(claim: ApiRecord): ResearchFinding {
  const title = text(claim.text) || text(claim.finding_id);
  return {
    id: text(claim.finding_id),
    claimId: text(claim.claim_id) || undefined,
    title,
    category: findingCategory(title),
    severity: findingSeverity(claim.severity),
    readiness: text(claim.readiness) || undefined,
    state: findingState(claim.state, claim.readiness),
    sourceRunId: text(claim.source_run_id) || undefined,
    version: Number(claim.version || 1),
    targetVersion: text(claim.target_version) || undefined,
    assetIds: Array.isArray(claim.affected_assets) ? claim.affected_assets.map(String) : [],
    evidence: splitEvidence(claim.evidence_refs),
    limitations: Array.isArray(claim.limitations) ? claim.limitations.map(String) : [],
    origin: "record",
  };
}

async function loadAssets(taskId: string) {
  const items: Asset[] = [];
  let cursor: string | undefined;
  let total = 0; let complete = true;
  do {
    const page = await assetMonitoringApi.queryAssets(taskId, { query: "", cursor, limit: PAGE });
    items.push(...page.items);
    total = Number(page.total ?? items.length);
    complete = complete && page.complete !== false;
    cursor = page.next_cursor || undefined;
  } while (cursor && items.length < ASSET_CAP);
  return { assets: items.map(assetFromRecord), total, complete: complete && !cursor };
}

async function loadClaims(taskId: string) {
  const rows: ApiRecord[] = [];
  let cursor = ""; let complete = true;
  do {
    const page = await querySrc<{ records?: Array<{ payload: ApiRecord }>; next_cursor?: string | null; complete?: boolean }>(taskId, "src.record.list", { kind: "finding_claim", limit: PAGE, cursor });
    rows.push(...(page.records || []).map(row => row.payload));
    complete = complete && page.complete !== false;
    cursor = page.next_cursor || "";
  } while (cursor);
  const byFinding = new Map<string, ResearchFinding>();
  rows.map(findingFromClaim).filter(row => row.id).forEach(row => {
    const previous = byFinding.get(row.id);
    byFinding.set(row.id, previous ? { ...previous, evidence: [...previous.evidence, ...row.evidence], assetIds: [...new Set([...previous.assetIds, ...row.assetIds])] } : row);
  });
  return { findings: [...byFinding.values()].sort((a, b) => a.id.localeCompare(b.id)), complete };
}

function runJob(row: SrcRun): ResearchJob {
  return {
    id: row.binding.run_id,
    kind: "run",
    title: row.binding.contract?.question || row.binding.run_id,
    state: row.run.state,
    detail: text((row.run.detail as ApiRecord).summary) || row.binding.target_url || undefined,
    updatedAt: row.run.updated_at || undefined,
    refs: [{ kind: "run", id: row.binding.run_id, label: row.binding.run_id }],
    origin: "record",
  };
}

/** Optional sources keep their error text so the page can show what is missing instead of an empty list. */
async function optional<T>(label: string, errors: string[], load: () => Promise<T>, fallback: T): Promise<T> {
  try { return await load(); } catch (cause) { errors.push(`${label}：${errorMessage(cause)}`); return fallback; }
}

export async function loadSrcResearchSnapshot(taskId: string): Promise<ResearchSnapshot> {
  const errors: string[] = [];
  const [task, runs, assetPage, claimPage] = await Promise.all([
    fetchSrcTask(taskId), listSrcRuns(taskId), loadAssets(taskId), loadClaims(taskId),
  ]);
  const [imports, monitors] = await Promise.all([
    optional("导入批次", errors, () => assetMonitoringApi.listImports(taskId), []),
    optional("持续监控", errors, () => assetMonitoringApi.listMonitors(taskId), []),
  ]);
  const changes: ResearchChange[] = [];
  const jobs: ResearchJob[] = [
    ...imports.map(batch => {
      const counts = Object.fromEntries(Object.entries(batch.counts || {}).map(([key, value]) => [key, Number(value)]));
      changes.push({
        id: `import-${batch.batch_id}`, at: text(batch.created_at), origin: "record",
        summary: `${batch.batch_id} ${batch.status === "completed" ? "完成" : batch.status}：新增 ${counts.created ?? 0} 项、变更 ${counts.updated ?? 0} 项`,
        refs: [{ kind: "import", id: batch.batch_id, label: batch.batch_id }],
      });
      return {
        id: batch.batch_id, kind: "import" as const, title: `资产导入 · ${text(batch.source_name) || batch.batch_id}`, state: batch.status,
        done: counts.processed, total: counts.processed, counts, detail: batch.completeness ? `完整性：${batch.completeness}` : undefined,
        updatedAt: text(batch.created_at) || undefined, refs: [{ kind: "import" as const, id: batch.batch_id, label: batch.batch_id }], origin: "record" as const,
      };
    }),
    ...runs.map(runJob),
    ...monitors.map(monitor => ({
      id: monitor.monitor_id, kind: "monitor" as const, title: monitor.name, state: monitor.status,
      detail: `每 ${Math.round(monitor.interval_seconds / 3600)} 小时 · ${monitor.timezone}`, updatedAt: monitor.last_success_at || undefined,
      refs: [{ kind: "monitor" as const, id: monitor.monitor_id, label: monitor.monitor_id }], origin: "record" as const,
    })),
  ];
  const monitorChanges = await Promise.all(monitors.slice(0, 3).map(monitor => optional(`监控变化 ${monitor.monitor_id}`, errors,
    () => querySrc<{ items?: ApiRecord[] }>(taskId, "src.monitor.changes", { monitor_id: monitor.monitor_id }), { items: [] })));
  monitorChanges.forEach(page => (page.items || []).forEach(item => changes.push({
    id: `change-${text(item.change_id)}`, at: text(item.observed_at), summary: text(item.summary) || text(item.change_type), origin: "record",
    refs: item.asset_id ? [{ kind: "asset", id: text(item.asset_id), label: text(item.asset_id) }] : [],
  })));
  const demo = taskId.startsWith("src-demo-");
  return {
    taskId,
    threadId: task.binding.thread_id,
    title: task.title,
    goal: task.input.goal || "",
    scope: task.input.authorization?.targets || [],
    exclusions: task.input.authorization?.exclusions || [],
    demo,
    disclaimer: demo ? SRC_DEMO_DISCLAIMER : undefined,
    assets: linkAssets(assetPage.assets, claimPage.findings),
    assetTotal: assetPage.total,
    assetsComplete: assetPage.complete,
    findings: claimPage.findings,
    findingsComplete: claimPage.complete,
    jobs,
    changes: changes.filter(row => row.at).sort((a, b) => b.at.localeCompare(a.at)),
    sourceErrors: errors,
  };
}
