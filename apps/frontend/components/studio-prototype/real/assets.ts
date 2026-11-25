import { randomUuid } from "@/lib/browserCrypto";
import {
  SRC_ASSET_TYPES,
  type AssetFieldConflict,
  type AssetObservation,
  type AssetRelationItem,
  type AssetVersionSnapshot,
  type SrcAssetType,
  type StudioConnector,
  type StudioRef,
  type StudioSource,
} from "../model";
import type {
  HistoryEntry, MergeRecord, RecordKind, SavedFilter, UndeterminedDuplicate,
} from "../records";
import type { ImportBatch, ImportItem } from "../store";
import {
  resolveSrcAssetType,
  type ImportDraft,
  type ImportFormat,
  type ImportSettings,
  type ImportValues,
} from "../importParsing";
import { studio } from "../store";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import { recordListAll, type DomainCtx, type RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";
const PAGE = 200;

export function arrayBufferToBase64(bytesInput: ArrayBuffer | Uint8Array): string {
  const bytes = bytesInput instanceof Uint8Array ? bytesInput : new Uint8Array(bytesInput);
  const chunkSize = 0x8000;
  const parts: string[] = [];
  for (let i = 0; i < bytes.length; i += chunkSize) {
    parts.push(String.fromCharCode(...bytes.subarray(i, i + chunkSize)));
  }
  return btoa(parts.join(""));
}

export function inferIngestionSourceKind(fileName: string, format?: ImportFormat): string {
  const lower = fileName.trim().toLowerCase();
  if (lower.endsWith(".har")) return "har";
  if (lower.endsWith(".xml") || lower.endsWith(".burp")) return "burp_xml";
  if (lower.endsWith(".jsonl") || lower.endsWith(".ndjson")) return "jsonl";
  if (lower.endsWith(".http") || lower.endsWith(".request")) return "raw_http";
  if (lower.endsWith(".csv") || format === "csv") return "csv";
  if (lower.endsWith(".json") || format === "json") return "json";
  return "auto";
}

function buildServerFieldMapping(mapping?: ImportSettings["mapping"]): Record<string, string> {
  if (!mapping) return {};
  const out: Record<string, string> = {};
  if (mapping.kind) {
    out[mapping.kind] = "asset_type";
    out.asset_type = mapping.kind;
  }
  if (mapping.title) out[mapping.title] = "title";
  if (mapping.body) out[mapping.body] = "body";
  if (mapping.author) out[mapping.author] = "author";
  if (mapping.collectedAt) out[mapping.collectedAt] = "collected_at";
  if (mapping.key) out[mapping.key] = "name";
  return out;
}

const TYPE_TO_KIND = new Map<string, StudioSource["kind"]>(SRC_ASSET_TYPES.map((t) => [t.value, t.kind]));
const KIND_TYPE: Record<StudioSource["kind"], SrcAssetType> = {
  interview: "domain",
  field: "web_service",
  archive: "http_endpoint",
  minutes: "ip",
};

function sourceKindOf(asset: Api): StudioSource["kind"] {
  const override = text((asset.current_profile as Api | undefined)?.workbench_kind);
  if (override === "interview" || override === "field" || override === "archive" || override === "minutes") return override;
  const normalized = canonicalAssetType(asset);
  return (normalized ? TYPE_TO_KIND.get(normalized) : undefined) ?? "interview";
}

function canonicalAssetType(asset: Api): SrcAssetType | undefined {
  const raw = text(asset.type) || text(asset.asset_type);
  return resolveSrcAssetType(raw) ?? undefined;
}

function reviewStatus(asset: Api): StudioSource["status"] {
  const value = text((asset.current_profile as Api | undefined)?.review_status);
  return value === "inbox" ? "inbox" : "filed";
}

function observationToItem(obs: Api): AssetObservation {
  const raw = (obs.raw_values ?? {}) as Api;
  const fields = Object.fromEntries(Object.entries(raw).map(([k, v]) => [k, typeof v === "string" ? v : JSON.stringify(v)]));
  return {
    id: text(obs.observation_id) || text(obs.record_id),
    sourceKind: obs.manual ? "manual" : text(obs.source_kind).startsWith("import") || text(obs.source_kind) === "csv" || text(obs.source_kind) === "json" || text(obs.source_kind) === "jsonl" ? "import" : text(obs.source_kind) === "derived" || text(obs.source_kind).startsWith("connector") ? "connector" : "run",
    sourceLabel: `${text(obs.source_kind)}${obs.source_id ? ` · ${text(obs.source_id)}` : ""}`,
    batchOrRunId: text(obs.source_id),
    observedAt: iso(obs.observed_at) || iso(obs.ingested_at),
    ingestedAt: iso(obs.ingested_at),
    artifactRef: ((obs.artifact_refs as Api[]) ?? [])[0] ? text(((obs.artifact_refs as Api[])[0] as Api).artifact_id) : "",
    summary: text(obs.summary) || Object.entries((obs.attributes as Api) ?? {}).slice(0, 4).map(([k, v]) => `${k}=${v}`).join(" · "),
    fields,
  };
}

function assetVersionToSnapshot(v: Api): AssetVersionSnapshot {
  const snap = (v.snapshot && typeof v.snapshot === "object" ? (v.snapshot as Api) : {});
  const profile = (snap.current_profile ?? snap.profile ?? {}) as Api;
  const attrs = Object.fromEntries(Object.entries(profile).map(([k, val]) => [k, typeof val === "string" ? val : JSON.stringify(val)]));
  return {
    version: num(v.version, 1),
    label: text(v.label) || `v${num(v.version, 1)}`,
    createdAt: iso(v.pinned_at) || iso(v.created_at),
    sourceLabel: text(v.pinned_by) || "operator",
    summary: text(profile.title) || text(snap.canonical_key),
    attributes: attrs,
    passageCount: Array.isArray(snap.observations) ? snap.observations.length : 1,
    bodyPreview: JSON.stringify(profile, null, 2),
  };
}

const MATERIAL_ASSET_TYPES = new Set([
  "openapi_spec",
  "javascript_artifact",
  "skill_material",
  "authorization_snapshot",
  "research_note",
]);

function normalizeJudgementState(raw: string): StudioSource["judgementState"] | undefined {
  if (raw === "confirmed_in_scope" || raw === "candidate" || raw === "out_of_scope" || raw === "needs_verify") return raw;
  if (raw === "uncertain") return "needs_verify";
  return undefined;
}

function assetToSource(row: Api, projectId: string, versions: AssetVersionSnapshot[]): StudioSource {
  const profile = (row.current_profile ?? row.profile ?? {}) as Api;
  const identity = (row.identity ?? {}) as Api;
  const judgement = (row.judgement ?? ((row.judgements as Api | undefined)?.researcher_judgement) ?? {}) as Api;
  const canonicalKey = text(row.canonical_key) || text(identity.url) || text(identity.name)
    || text(identity.address) || text(identity.host) || text(identity.domain);
  const observations = (row.observations as Api[]) ?? [];
  const attrs = Object.fromEntries(Object.entries(profile).map(([k, v]) => [k, typeof v === "string" ? v : JSON.stringify(v)]));
  const id = text(row.asset_id) || text(row.record_id);
  const passages = observations.length
    ? observations.map((obs, index) => ({
        id: `${id}-obs-${index + 1}`,
        sourceId: id,
        index: index + 1,
        text: Object.keys((obs.raw_values as Api) ?? {}).length
          ? JSON.stringify(obs.raw_values, null, 2)
          : Object.entries((obs.attributes as Api) ?? {}).map(([k, v]) => `${k}: ${v}`).join("\n") || "（以属性与观测记录为准）",
      }))
    : [{
        id: `${id}-attrs`,
        sourceId: id,
        index: 1,
        text: Object.entries(attrs).map(([k, v]) => `${k}: ${v}`).join("\n") || "（以属性与观测记录为准）",
      }];
  const profileSources = (row.profile_sources ?? {}) as Api;
  const conflicts: AssetFieldConflict[] = Object.entries((row.conflicts ?? {}) as Api).map(([field, rows]) => {
    const selected = (profileSources[field] ?? {}) as Api;
    return {
      id: `${id}-conf-${field}`,
      field,
      fieldLabel: field,
      selectedValue: typeof selected.value === "string" ? selected.value : JSON.stringify(selected.value ?? ""),
      resolutionBasis: selected.manual ? "人工校正" : "最新观测优先",
      resolvedBy: selected.manual ? ("manual" as const) : ("rule" as const),
      candidates: ((rows as Api[]) ?? []).map((c) => ({
        source: `${text(c.source_kind)} · ${text(c.source_id)}`,
        value: typeof c.value === "string" ? c.value : JSON.stringify(c.value),
        observedAt: iso(c.observed_at),
        artifactRef: "",
      })),
    };
  });
  const relations: AssetRelationItem[] = ((row.relations as Api[]) ?? []).map((rel) => {
    const tid = text(rel.from_asset_id) === id ? text(rel.to_asset_id) : text(rel.from_asset_id);
    return {
      id: text(rel.relation_id),
      direction: text(rel.from_asset_id) === id ? "outbound" : "inbound",
      relationType: text(rel.relation_type),
      targetId: tid,
      targetLabel: text(rel.target_label) || tid,
      targetKind: "source",
      origin: (["observed", "derived", "inferred"].includes(text(rel.origin)) ? text(rel.origin) : "observed") as AssetRelationItem["origin"],
      evidenceRef: "",
      observedAt: iso(rel.valid_from) || iso(rel.created_at),
    };
  });
  const sortedVersions = [...versions].sort((a, b) => b.version - a.version);
  const rawType = text(row.type) || text(row.asset_type);
  const materialKind = text(row.material_kind) || (MATERIAL_ASSET_TYPES.has(rawType) ? rawType : "");
  const isMaterial = Boolean(row.is_material) || Boolean(materialKind);
  const judgementState = normalizeJudgementState(text(row.judgement_state) || text(judgement.judgement_state));
  const systemName = text(row.system_name) || text(judgement.system_name) || undefined;
  const uncertaintyNote = text(row.uncertainty_note) || text(judgement.uncertainty_note) || undefined;
  const observationRole = text(row.observation_role) || text(observations[0]?.observation_role) || undefined;
  const activeOperationAuthorized = typeof row.active_operation_authorized === "boolean"
    ? row.active_operation_authorized
    : (typeof row.authorized === "boolean" ? row.authorized : undefined);
  const mergedItems = Array.isArray(row.merged_items)
    ? (row.merged_items as Api[]).map((item) => ({
        id: text(item.asset_id) || text(item.id),
        title: text(item.display_label) || text(item.title) || text(item.asset_id) || text(item.id),
        assetType: text(item.type) || text(item.asset_type) || undefined,
        reason: text(item.reason) || undefined,
      })).filter((item) => item.id)
    : undefined;
  return {
    id,
    projectId,
    title: text(profile.title) || text(row.display_name) || canonicalKey || id,
    kind: sourceKindOf(row),
    assetType: canonicalAssetType(row),
    author: text(observations[0]?.source_id) || "SRC 资产库",
    collectedAt: iso(row.first_seen_at) || iso(row.first_observed),
    status: reviewStatus(row),
    passages,
    canonicalKey,
    aliases: ((row.aliases as unknown[]) ?? []).map(String).filter(Boolean),
    importance: (["core", "high", "normal", "low"].includes(text(row.importance)) ? text(row.importance) : "normal") as StudioSource["importance"],
    tags: ((row.tags as unknown[]) ?? []).map((t) => (typeof t === "string" ? t : text((t as Api).name) || text((t as Api).tag_id))).filter(Boolean),
    firstSeenAt: iso(row.first_seen_at) || iso(row.first_observed),
    lastObservedAt: iso(row.last_observed_at) || iso(row.latest_observed),
    lastVerifiedAt: iso(row.last_success_at) || iso(row.last_success),
    lastFailedAt: iso(row.last_failure_at) || iso(row.last_failure),
    archived: text(row.lifecycle) === "archived",
    pinnedVersion: sortedVersions[0]?.version ?? (num(row._revision) || 1),
    versions: sortedVersions,
    attributes: attrs,
    systemName,
    judgementState,
    uncertaintyNote,
    isMaterial,
    materialKind: materialKind || undefined,
    observationRole,
    activeOperationAuthorized,
    mergedItems,
    sourceHistory: observations.map(observationToItem),
    conflicts,
    relations,
  };
}

function materialRecordToSource(
  kind: "research_document" | "skill_material" | "authorization_snapshot",
  row: Api,
  projectId: string,
): StudioSource {
  const collectedAt = iso(row.ingested_at) || iso(row.observed_at) || iso(row.registered_at) || iso(row.created_at);
  const stableFallback = text(row.content_digest || row.source_digest) || text(row.source_url) || collectedAt || "material";
  const id = text(row.record_id) || text(row._record_id) || text(row._parent_id) || `${kind}-${stableFallback}`;
  let materialKind: string = kind;
  let title = id;
  let body = "";
  let author = text(row.created_by) || text(row.registered_by) || "研究资料";
  const attributes: Record<string, string> = { record_kind: kind };

  if (kind === "research_document") {
    const docType = text(row.document_type);
    materialKind = docType === "openapi" ? "openapi_spec" : docType === "javascript" ? "javascript_artifact" : (docType || "research_document");
    const artifact = (row.artifact as Api | undefined) ?? {};
    title = text(row.source_url) || text(row.title) || text(artifact.name) || id;
    attributes.material_kind = materialKind;
    if (text(row.status)) attributes.status = text(row.status);
    if (text(row.source_url)) attributes.source_url = text(row.source_url);
    if (text(row.content_digest || row.source_digest)) attributes.content_digest = text(row.content_digest || row.source_digest);
    if (text(row.runtime_reachability)) attributes.runtime_reachability = text(row.runtime_reachability);
    body = JSON.stringify(row, null, 2);
    author = text(row.extractor_version) || "静态解析";
  } else if (kind === "skill_material") {
    materialKind = "skill_material";
    title = text(row.source_url) || text(row.material_id) || id;
    attributes.material_kind = materialKind;
    if (text(row.status)) attributes.status = text(row.status);
    if (text(row.source_commit)) attributes.source_commit = text(row.source_commit);
    if (text(row.content_digest || row.digest)) attributes.content_digest = text(row.content_digest || row.digest);
    body = JSON.stringify(row, null, 2);
  } else {
    materialKind = "authorization_snapshot";
    title = `授权范围快照 · ${text(row.snapshot_id) || id}`;
    attributes.material_kind = materialKind;
    if (text(row.authorization_is_not_execution_grant)) attributes.notice = "授权文档仅声明研究范围";
    body = JSON.stringify(row, null, 2);
  }

  return {
    id,
    projectId,
    title,
    kind: "archive",
    author,
    collectedAt,
    status: "filed",
    isMaterial: true,
    materialKind,
    passages: [{
      id: `${id}-material-1`,
      sourceId: id,
      index: 1,
      text: body || "（暂无资料正文）",
    }],
    canonicalKey: title,
    importance: "normal",
    tags: ["项目资料", materialKind],
    firstSeenAt: collectedAt,
    lastObservedAt: collectedAt,
    attributes,
  };
}

async function paged(path: (cursor: number) => Promise<{ items?: Api[]; next_cursor?: number | null; total?: number | null; complete?: boolean }>): Promise<{ items: Api[]; total: number; complete: boolean }> {
  const items: Api[] = [];
  let cursor = 0;
  let total = 0;
  let complete = true;
  do {
    const page = await path(cursor);
    items.push(...(page.items ?? []));
    total = num(page.total, items.length);
    complete = page.complete !== false;
    cursor = page.next_cursor == null ? -1 : num(page.next_cursor);
  } while (cursor >= 0 && items.length < total);
  return { items, total, complete: complete && items.length >= total };
}

function batchToImport(row: Api, projectId: string): ImportBatch {
  const meta = (row.source_metadata ?? {}) as Api;
  const artifact = (row.source_artifact ?? row.original_artifact ?? {}) as Api;
  const importedFormat = text(meta.studio_format) || text(row.source_kind);
  const entries: Api[] = [...((row.items as Api[]) ?? []), ...((row.failed_items as Api[]) ?? []).map((item): Api => ({ ...item, ok: false }))]
    .sort((a, b) => num(a.index) - num(b.index));
  return {
    id: text(row.batch_id) || text(row.record_id),
    projectId,
    fileName: text(meta.filename) || text(row.source_id) || text(row.batch_id),
    format: (["csv", "json"].includes(importedFormat) ? importedFormat : "txt") as ImportFormat,
    size: num(artifact.size),
    importedAt: iso(row.created_at),
    text: "",
    settings: {} as ImportSettings,
    items: entries.map((item): ImportItem => ({
      id: `${row.batch_id}-${num(item.index)}`,
      index: num(item.index),
      location: { label: `#${num(item.index) + 1}`, line: num(item.index) + 1, endLine: num(item.index) + 1 },
      raw: "",
      values: { title: text(item.asset_id) || text(((item.item ?? {}) as Api).title), body: "", kind: "", author: "", collectedAt: "", key: "" },
      parseError: item.ok === false ? text(item.message) : "",
      outcome: item.ok === false ? "failed" : text(item.action) === "merged" || text(item.action) === "updated" ? "updated" : "created",
      reason: item.ok === false ? text(item.message) : text(item.action),
      sourceId: text(item.asset_id) || undefined,
      attempts: 1,
    })),
    undoneAt: iso(row.reverted_at) || undefined,
    connectorMeta: text(row.source_kind) !== "csv" && text(row.source_kind) !== "json" ? { connectorId: text(row.source_kind), sourceSystem: text(row.source_kind), query: "", completeness: "complete", costCredits: 0, receiptId: text(row.batch_id) } : undefined,
  };
}

function verdictsToLocal(items: Api[]): { dismissed: string[]; undetermined: Record<string, UndeterminedDuplicate> } {
  const dismissed: string[] = [];
  const undetermined: Record<string, UndeterminedDuplicate> = {};
  for (const v of items) {
    const key = text(v.key);
    if (!key.startsWith("source:")) continue;
    if (v.state === "dismissed") dismissed.push(key);
    else if (v.state === "undetermined") {
      undetermined[key] = { key, kind: (text(v.kind) === "clue" ? "clue" : "source"), projectId: "", ids: (v.ids as string[]) ?? [], reason: text(v.reason), missingNote: text(v.missing_note), at: iso(v.decided_at), by: text(v.decided_by) };
    }
  }
  return { dismissed, undetermined };
}

function mergeGroups(assets: Api[]): MergeRecord[] {
  const byTarget = new Map<string, Api[]>();
  for (const a of assets) {
    if (a.lifecycle === "merged" && a.merged_into) {
      const list = byTarget.get(text(a.merged_into)) ?? [];
      list.push(a);
      byTarget.set(text(a.merged_into), list);
    }
  }
  return [...byTarget.entries()].map(([target, members]) => ({
    id: `server-merge-${target}`,
    kind: "source" as const,
    projectId: "",
    primaryId: target,
    memberIds: members.map((m) => text(m.asset_id)).filter(Boolean),
    reason: text(members[0]?.merge_reason) || "资产归并",
    detected: "server",
    at: iso(members[0]?.merged_at) || iso(members[0]?.updated_at),
  }));
}

function historyEntries(items: Api[]): HistoryEntry[] {
  return items.map((row): HistoryEntry => ({
    id: text(row.entry_id),
    at: iso(row.at),
    action: (text(row.action) || "status") as HistoryEntry["action"],
    summary: text(row.summary),
    scope: text(row.scope) || undefined,
    refs: (row.refs as StudioRef[]) ?? [],
    serverId: text(row.entry_id),
    serverDomain: text(row.domain) || "assets",
    serverUndo: (row.undo as HistoryEntry["serverUndo"]) ?? undefined,
    revertedBy: row.reverted_at ? "server" : undefined,
  }));
}

const CONNECTOR_CATEGORIES: Record<string, StudioConnector["category"]> = {
  fofa: "search_engine", quake: "search_engine", hunter: "search_engine", zoomeye: "search_engine", shodan: "search_engine",
  bbot: "recon_pipeline", reconftw: "recon_pipeline", scopesentry: "recon_pipeline",
  burp: "proxy_import", caido: "proxy_import", bbrf: "recon_pipeline", cli: "recon_pipeline",
  cloud_inventory: "asset_repo", github: "asset_repo", gitlab: "asset_repo", aws_ec2: "asset_repo",
  external_event: "mcp_plugin",
};

function connectorBatchToImport(row: Api, projectId: string): ImportBatch {
  const batchId = text(row.batch_id) || text(row.record_id);
  const connectorId = text(row.connector_id) || "connector";
  const statusVal = text(row.status) || text(row.outcome) || "completed";
  const acquisition = (row.acquisition as Api | undefined) ?? {};
  const errObj = row.error && typeof row.error === "object" ? (row.error as Api) : null;
  const errMsg = errObj ? [text(errObj.code), text(errObj.message)].filter(Boolean).join(" · ") : text(row.error);
  const counts = (row.counts as Api | undefined) ?? {};
  const itemCount = num(counts.items ?? counts.total ?? counts.returned, 0);
  const items: ImportItem[] = errMsg
    ? [{
        id: `${batchId}-err`,
        index: 0,
        location: { label: connectorId.toUpperCase(), line: 1, endLine: 1 },
        raw: "",
        values: { title: `${connectorId.toUpperCase()} 同步失败`, body: errMsg, kind: "", author: connectorId, collectedAt: iso(row.created_at), key: batchId },
        parseError: errMsg,
        outcome: "failed",
        reason: errMsg,
        attempts: 1,
      }]
    : Array.from({ length: itemCount > 0 ? 1 : 0 }, () => ({
        id: `${batchId}-summary`,
        index: 0,
        location: { label: connectorId.toUpperCase(), line: 1, endLine: 1 },
        raw: "",
        values: { title: `同步 ${itemCount} 条观测 (${text(row.reconciliation_state) || "unreconciled"})`, body: "", kind: "", author: connectorId, collectedAt: iso(row.created_at), key: batchId },
        parseError: "",
        outcome: "created" as const,
        reason: `${text(row.reconciliation_state) || statusVal} · ${itemCount} 条`,
        attempts: 1,
      }));
  return {
    id: batchId,
    projectId,
    fileName: `${connectorId.toUpperCase()} 连接器批次`,
    format: "json",
    size: 0,
    importedAt: iso(row.created_at),
    text: JSON.stringify(row, null, 2),
    settings: {} as ImportSettings,
    items,
    connectorMeta: {
      connectorId,
      sourceSystem: connectorId,
      query: text(acquisition.query) || text(acquisition.repository),
      completeness: statusVal === "partial" ? "partial" : statusVal === "quota_limited" ? "quota_limited" : "complete",
      costCredits: 0,
      receiptId: batchId,
    },
  };
}

function manifestToConnector(m: Api, cfg?: Api, statusEntry?: Api): StudioConnector {
  const id = text(m.connector_id) || text(statusEntry?.connector_id);
  const caps = (m.capabilities as string[]) ?? [];
  const cfgWrap = (statusEntry?.config as Api | undefined) ?? cfg;
  const currentConfig = ((cfgWrap?.config as Api | undefined) ?? (cfgWrap && !cfgWrap.version ? cfgWrap : {}) ?? {}) as Record<string, unknown>;
  const cfgVersion = num(cfgWrap?.version, 0);
  const revoked = Boolean(cfgWrap?.revoked_at);
  const creds = ((m.credentials ?? m.auth ?? {}) as Api);
  const referenceFields = Array.isArray(creds.reference_fields)
    ? (creds.reference_fields as unknown[]).map(String).filter(Boolean)
    : Array.isArray(m.reference_fields)
      ? (m.reference_fields as unknown[]).map(String).filter(Boolean)
      : [];
  const configFields = Array.isArray(m.config_fields)
    ? (m.config_fields as Api[]).map((f) => ({
        key: text(f.key),
        label: text(f.label) || text(f.key),
        kind: text(f.kind) || undefined,
        type: text(f.type) || undefined,
        required: Boolean(f.required),
        default: f.default,
        placeholder: text(f.placeholder) || undefined,
      })).filter((f) => f.key)
    : [];
  const rawGrant = statusEntry?.grant && typeof statusEntry.grant === "object" ? (statusEntry.grant as Api) : null;
  const grant: StudioConnector["grant"] = rawGrant
    ? {
        allowedPurposes: Array.isArray(rawGrant.allowed_purposes) ? rawGrant.allowed_purposes.map(String) : [],
        budgetLimit: typeof rawGrant.budget_limit === "number" ? rawGrant.budget_limit : null,
        usedRequests: typeof rawGrant.used_requests === "number" ? rawGrant.used_requests : 0,
        revokedAt: text(rawGrant.revoked_at) || null,
        revision: num(rawGrant.revision, 0),
      }
    : null;
  const rawReadiness = text(statusEntry?.readiness);
  const readiness: StudioConnector["readiness"] = (
    rawReadiness === "unconnected"
    || rawReadiness === "configured_unverified"
    || rawReadiness === "verified"
    || rawReadiness === "permission_denied"
    || rawReadiness === "quota_unknown"
    || rawReadiness === "file_import_only"
  )
    ? rawReadiness
    : (revoked || cfgVersion === 0 ? "unconnected" : "configured_unverified");
  const status: StudioConnector["status"] = readiness === "verified"
    ? "ready"
    : readiness === "permission_denied" || readiness === "unconnected"
      ? "degraded"
      : "configured";
  const missingFields = Array.isArray(statusEntry?.missing_fields)
    ? statusEntry.missing_fields.map(String).filter(Boolean)
    : [];
  const lastError = text(statusEntry?.last_error) || undefined;
  const verifiedAt = text(statusEntry?.verified_at) || undefined;
  const quotaRemaining = num(statusEntry?.quota_remaining, 0);
  const descParts = [
    cfgVersion > 0 && !revoked ? `配置 v${cfgVersion}` : "未配置",
    missingFields.length ? `缺少：${missingFields.join(", ")}` : "",
    lastError ? `错误：${lastError}` : "",
  ].filter(Boolean);
  return {
    id,
    name: text(m.display_name) || id.toUpperCase(),
    category: CONNECTOR_CATEGORIES[id] ?? "mcp_plugin",
    version: text(m.version),
    status,
    readiness,
    capabilities: caps,
    supportedAssetTypes: Object.keys((m.asset_types as Api) ?? {}),
    referenceFields,
    configFields,
    currentConfig,
    grant,
    missingFields,
    verifiedAt,
    lastError,
    defaultQuery: text(currentConfig.query) || text(currentConfig.repository),
    quotaRemaining,
    quotaTotal: 0,
    costPerSync: 0,
    completenessGuarantee: text((m.pagination as Api | undefined)?.kind) || "unknown",
    description: descParts.join(" · "),
  };
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: Omit<HistoryEntry, "id" | "at">) {
  const entry = (ctx.proposal?.() ?? ctx.state()).history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, scope: entry.scope, refs: entry.refs, at: entry.at }
    : { id: `wb-${Date.now().toString(36)}`, action: fallback.action, summary: fallback.summary, scope: fallback.scope, refs: fallback.refs, at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "assets" } });
}

async function bulkQuery(ctx: DomainCtx) {
  return paged((cursor) => querySrc(ctx.taskId, "src.workbench.asset.bulk", { cursor, limit: PAGE }));
}

function buildIdentityForAsset(type: SrcAssetType | string, rawKey: string): Record<string, unknown> {
  const val = rawKey.trim();
  switch (type) {
    case "root_domain": return { domain: val };
    case "domain": return { name: val };
    case "dns": return { host: val, record_type: "A", value: val };
    case "ip": return { address: val };
    case "cidr": return { cidr: val };
    case "port": {
      const [host, p] = val.split(":");
      return { host: host || val, port: Number(p) || 443 };
    }
    case "web_service": return { host: val, port: 443, protocol: "https" };
    case "non_web_service": return { host: val, port: 22, protocol: "tcp" };
    case "http_endpoint": return { url: val, method: "GET" };
    case "parameter": return { url: val, method: "GET", param_location: "query", param_name: "id" };
    case "app": return { platform: "Android", name: val };
    case "miniprogram": return { platform: "WeChat", appid: val };
    case "repository": return { host: "github", canonical: val };
    case "cloud": return { provider: "aliyun", resource_id: val };
    case "cdn":
    case "certificate":
    default: return { name: val };
  }
}

export function createAssetDomain(): RealDomain {
  const id = "assets";
  return {
    id,
    pollMs: 45000,
    owns: ["sources", "imports", "importOrigins", "connectors", "assetLoad"],
    combines: {
      merges: (local, server) => {
        const serverIds = new Set((server as MergeRecord[]).map((m) => m.id));
        return [...(server as MergeRecord[]), ...(local as MergeRecord[]).filter((m) => m.kind !== "source" && !serverIds.has(m.id))];
      },
      history: (local, server) => {
        const serverIds = new Set((server as HistoryEntry[]).map((e) => e.id));
        return [...(server as HistoryEntry[]), ...(local as HistoryEntry[]).filter((e) => e.serverDomain !== "assets" && !serverIds.has(e.id))];
      },
      savedFilters: (local, server) => {
        const serverIds = new Set((server as SavedFilter[]).map((f) => f.id));
        return [...(server as SavedFilter[]), ...(local as SavedFilter[]).filter((f) => f.scope !== "library" && !serverIds.has(f.id))];
      },
      dismissedDuplicates: (local, server) => {
        const serverKeys = new Set(server as string[]);
        return [...(server as string[]), ...(local as string[]).filter((k) => !k.startsWith("source:") && !serverKeys.has(k))];
      },
      undeterminedDuplicates: (local, server) => {
        const serverKeys = new Set(Object.keys(server as Record<string, UndeterminedDuplicate>));
        const keep = Object.fromEntries(Object.entries(local as Record<string, UndeterminedDuplicate>).filter(([k]) => !k.startsWith("source:") && !serverKeys.has(k)));
        return { ...(server as Record<string, UndeterminedDuplicate>), ...keep };
      },
    },
    async load(taskId, ctx) {
      const [
        bulk,
        importRows,
        connectorBatchRes,
        tagRows,
        linkRows,
        verdicts,
        histPage,
        manifests,
        connectorStatusDoc,
        savedQueries,
        allAssets,
        versionRows,
        researchDocs,
        skillMaterials,
        authSnapshots,
        taskLinks,
      ] = await Promise.all([
        bulkQuery(ctx),
        recordListAll(taskId, "import_batch"),
        querySrc<{ batches?: Api[] }>(taskId, "src.connector.batch.list", {}),
        recordListAll(taskId, "tag"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.asset.links", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.duplicate.verdicts", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<Api[]>(taskId, "src.connector.manifests", {}),
        querySrc<{ task_id?: string; project_id?: string | null; connectors?: Api[] }>(taskId, "src.connector.status", {}),
        recordListAll(taskId, "saved_query"),
        recordListAll(taskId, "asset"),
        recordListAll(taskId, "asset_version"),
        recordListAll(taskId, "research_document"),
        recordListAll(taskId, "skill_material"),
        recordListAll(taskId, "authorization_snapshot"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.asset.tasklinks", {}),
      ]);
      const detached = new Set(((taskLinks.items ?? [])).filter((l) => l.state === "detached").map((l) => text(l.asset_id)));
      const tagName = new Map(tagRows.map((t) => [text(t.tag_id) || text(t.record_id), text(t.name) || text((t.payload as Api)?.name)]));
      const versionsByAsset = new Map<string, AssetVersionSnapshot[]>();
      for (const v of versionRows) {
        const aid = text(v.asset_id);
        if (!aid) continue;
        versionsByAsset.set(aid, [...(versionsByAsset.get(aid) ?? []), assetVersionToSnapshot(v)]);
      }
      const projectId = ctx.projectId;
      const assetSources = bulk.items.map((raw) => {
        const aid = text(raw.asset_id) || text(raw.record_id);
        const source = assetToSource(raw, projectId, versionsByAsset.get(aid) ?? []);
        source.tags = ((raw.tags as unknown[]) ?? []).map((t) => (typeof t === "string" ? tagName.get(t) ?? t : text((t as Api).name))).filter(Boolean);
        if (detached.has(source.id)) source.unlinkedTaskIds = [taskId];
        const links = (linkRows.items ?? []).filter((l) => ((l.asset_ids as string[]) ?? []).includes(source.id));
        if (links.length) source.relations = [...(source.relations ?? []), ...links.map((l): AssetRelationItem => ({
          id: `link-${l.ref_kind}-${l.ref_id}`,
          direction: "outbound",
          relationType: `关联${l.ref_kind === "task" ? "任务" : l.ref_kind === "run" ? "工作记录" : "报告"}`,
          targetId: text(l.ref_id), targetLabel: text(l.ref_id), targetKind: "source",
          origin: "observed", evidenceRef: "", observedAt: iso(l.updated_at) || iso(l.created_at),
        }))];
        return source;
      });
      const materialSources: StudioSource[] = [
        ...researchDocs.map((row) => materialRecordToSource("research_document", row, projectId)),
        ...skillMaterials.map((row) => materialRecordToSource("skill_material", row, projectId)),
        ...authSnapshots.map((row) => materialRecordToSource("authorization_snapshot", row, projectId)),
      ];
      const existingIds = new Set(assetSources.map((s) => s.id));
      const sources = [...assetSources, ...materialSources.filter((m) => !existingIds.has(m.id))];
      const titleById = new Map(sources.map((s) => [s.id, s.title]));
      for (const s of sources) {
        for (const rel of s.relations ?? []) {
          rel.targetLabel = titleById.get(rel.targetId) || rel.targetLabel || rel.targetId;
        }
      }
      const manifestList = Array.isArray(manifests) ? manifests : [];
      const statusEntries = Array.isArray(connectorStatusDoc?.connectors) ? connectorStatusDoc.connectors : [];
      const statusByConnector = new Map(statusEntries.map((entry) => [text(entry.connector_id), entry]));
      const connectors = statusEntries.length
        ? statusEntries.map((entry) => {
            const cid = text(entry.connector_id);
            const manifest = (entry.manifest as Api | undefined)
              ?? manifestList.find((m) => text(m.connector_id) === cid)
              ?? { connector_id: cid };
            return manifestToConnector(manifest, entry.config as Api | undefined, entry);
          })
        : manifestList.map((m) => manifestToConnector(m, undefined, statusByConnector.get(text(m.connector_id))));
      const { dismissed, undetermined } = verdictsToLocal(verdicts.items ?? []);
      const savedFilters: SavedFilter[] = savedQueries.filter((q) => text(q.scope) !== "findings").map((q) => ({
        id: text(q.query_id) || text(q.record_id),
        name: text(q.name),
        createdAt: iso(q.created_at),
        ...(text(q.scope) === "findings"
          ? { scope: "findings", filter: q.filter ?? {} }
          : { scope: "library", filter: q.filter ?? {} }),
      }) as SavedFilter);
      const importOrigins: Record<string, { batchId: string; key: string }> = {};
      for (const row of importRows) {
        for (const item of (row.items as Api[]) ?? []) {
          const assetId = text(item.asset_id);
          if (assetId) importOrigins[assetId] = { batchId: text(row.batch_id), key: text(item.source_event_id) };
        }
      }
      const fileImports = await Promise.all(importRows.map(async (row) => {
        const batch = batchToImport(row, projectId);
        const artifact = (row.source_artifact ?? row.original_artifact ?? {}) as Api;
        if (artifact.artifact_id) {
          try {
            const raw = await querySrc<unknown>(taskId, "src.artifact.read", { ref_data: artifact });
            const env = Array.isArray(raw) ? (raw[0] as Api) : (raw as Api);
            if (env.encoding === "base64" && typeof env.data === "string") {
              batch.text = new TextDecoder().decode(Uint8Array.from(atob(env.data), (c) => c.charCodeAt(0)));
            } else {
              batch.originalError = "原文响应缺少文件内容";
            }
          } catch (error) {
            batch.originalError = error instanceof Error ? error.message : String(error);
          }
        }
        return batch;
      }));
      const seenBatchIds = new Set(fileImports.map((b) => b.id));
      const connectorImports = (connectorBatchRes?.batches ?? [])
        .map((row) => connectorBatchToImport(row, projectId))
        .filter((b) => b.id && !seenBatchIds.has(b.id));
      const imports = [...fileImports, ...connectorImports];
      imports.sort((a, b) => b.importedAt.localeCompare(a.importedAt));
      return {
        sources,
        imports,
        importOrigins,
        savedFilters,
        dismissedDuplicates: dismissed,
        undeterminedDuplicates: undetermined,
        connectors,
        assetLoad: { loaded: bulk.items.length, total: bulk.total, complete: bulk.complete },
        merges: mergeGroups(allAssets),
        history: historyEntries(histPage.items ?? []),
      };
    },
    mutators: {
      async setSourceImportance(ctx, ...args) {
        const [ids, importance] = args as [string | string[], StudioSource["importance"]];
        const list = Array.isArray(ids) ? ids : [ids];
        await postCommand(ctx.taskId, "src.workbench.asset.importance", { asset_ids: list, importance });
        await recordHistory(ctx, { op: "importance", params: { asset_ids: list } }, { action: "status", summary: "调整资产重要性", refs: list.map((i) => ({ kind: "source", id: i })) });
      },
      async setSourceStatus(ctx, ...args) {
        const [ids, status] = args as [string | string[], StudioSource["status"]];
        for (const assetId of Array.isArray(ids) ? ids : [ids]) {
          await postCommand(ctx.taskId, "src.asset.correct", { asset_id: assetId, attributes: { review_status: status }, reason: `工作台标记为「${status === "inbox" ? "收件箱" : "已归档"}」` });
        }
        await recordHistory(ctx, { op: "noop" }, { action: "status", summary: "调整资产归档状态", refs: [] });
      },
      async setSourceKind(ctx, ...args) {
        const [ids, kind] = args as [string | string[], StudioSource["kind"]];
        for (const assetId of Array.isArray(ids) ? ids : [ids]) {
          await postCommand(ctx.taskId, "src.asset.correct", { asset_id: assetId, attributes: { workbench_kind: kind }, reason: "工作台调整资产类型显示" });
        }
      },
      async resolveAssetConflict(ctx, ...args) {
        const [sourceId, conflictId, selectedValue, basis] = args as [string, string, string, string];
        const conflict = ctx.state().sources.find((s) => s.id === sourceId)?.conflicts?.find((c) => c.id === conflictId);
        await postCommand(ctx.taskId, "src.asset.correct", {
          asset_id: sourceId,
          attributes: conflict ? { [conflict.field]: selectedValue } : {},
          reason: basis || "工作台人工裁决字段冲突",
        });
        await recordHistory(ctx, { op: "noop" }, { action: "conflict", summary: `裁决资产字段冲突 ${conflict?.field ?? conflictId}`, refs: [{ kind: "source", id: sourceId }] });
      },
      async batchEditTags(ctx, ...args) {
        const [kind, ids, mode, tags] = args as [RecordKind, string[], "add" | "remove" | "replace", string[]];
        if (kind !== "source") return;
        const state = ctx.state();
        const tagRows = await recordListAll(ctx.taskId, "tag");
        const tagIdByName = new Map(tagRows.map((t) => [text(t.name), text(t.tag_id) || text(t.record_id)]));
        const tagIds: string[] = [];
        for (const name of tags) {
          const existing = tagIdByName.get(name);
          if (existing) { tagIds.push(existing); continue; }
          const tag = await postCommand<Api>(ctx.taskId, "src.tag.save", { payload: { name } });
          const tagId = text(tag.tag_id) || text(tag.record_id);
          tagIds.push(tagId);
          tagIdByName.set(name, tagId);
        }
        for (const assetId of ids) {
          const current = new Set((state.sources.find((s) => s.id === assetId)?.tags ?? []));
          const detachNames = mode === "replace" ? [...current].filter((n) => !tags.includes(n)) : mode === "remove" ? tags : [];
          const detachIds = detachNames.map((n) => tagIdByName.get(n)).filter((x): x is string => Boolean(x));
          if (detachIds.length) await postCommand(ctx.taskId, "src.tag.assign", { asset_ids: [assetId], tag_ids: detachIds, detach: true });
          if (mode !== "remove" && tagIds.length) await postCommand(ctx.taskId, "src.tag.assign", { asset_ids: [assetId], tag_ids: tagIds });
        }
        await recordHistory(ctx, { op: "noop" }, { action: "tag", summary: `批量编辑标签（${mode}）`, refs: ids.map((i) => ({ kind: "source", id: i })) });
      },
      async batchLinkRecords(ctx, ...args) {
        const [kind, ids, target] = args as [RecordKind, string[], StudioRef];
        if (kind !== "source") return;
        const refKind = target.kind === "task" ? "task" : target.kind === "run" ? "run" : target.kind === "report" ? "report" : "";
        if (!refKind) return;
        await postCommand(ctx.taskId, "src.workbench.asset.link", { ref_kind: refKind, ref_id: target.id, asset_ids: ids });
        await recordHistory(ctx, { op: "noop" }, { action: "link", summary: `批量关联到 ${target.kind} ${target.id}`, refs: ids.map((i) => ({ kind: "source", id: i })) });
      },
      async batchDeleteRecords(ctx, ...args) {
        const [kind, ids] = args as ["source" | "clue" | "task", string[]];
        if (kind !== "source") return;
        for (const assetId of ids) {
          await postCommand(ctx.taskId, "src.asset.lifecycle", { asset_id: assetId, action: "archive" });
        }
        await recordHistory(ctx, { op: "lifecycle", params: { asset_ids: ids, state: "active" } }, { action: "delete", summary: `批量删除 ${ids.length} 份资产（归档，可撤销）`, refs: ids.map((i) => ({ kind: "source", id: i })) });
      },
      async toggleAssetArchive(ctx, ...args) {
        const [ids, archived, reason] = args as [string | string[], boolean, string?];
        for (const assetId of Array.isArray(ids) ? ids : [ids]) {
          await postCommand(ctx.taskId, "src.asset.lifecycle", { asset_id: assetId, action: archived ? "archive" : "restore" });
        }
        void reason;
      },
      async toggleTaskUnlinkAsset(ctx, ...args) {
        const [sourceId] = args as [string, string];
        const isUnlinked = (ctx.state().sources.find((s) => s.id === sourceId)?.unlinkedTaskIds ?? []).includes(ctx.taskId);
        await postCommand(ctx.taskId, "src.workbench.asset.tasklink", { asset_id: sourceId, state: isUnlinked ? "attached" : "detached" });
      },
      async publishOrPinAssetVersion(ctx, ...args) {
        const [sourceId, version, targetTaskId] = args as [string, number, string?];
        if (targetTaskId) {
          await postCommand(ctx.taskId, "src.asset.publish", {
            target_task_id: targetTaskId, purpose: "工作台跨任务复用固定版本",
            selection: { assets: [{ asset_id: sourceId, asset_revision: version, fields: [] }], relation_ids: [], observation_ids: [], artifact_refs: [], source_watermarks: [] },
          });
        } else {
          await postCommand(ctx.taskId, "src.asset.version.pin", { asset_id: sourceId, label: `v${version}` });
        }
      },
      async mergeRecords(ctx, ...args) {
        const [input] = args as [{ kind: RecordKind; primaryId: string; memberIds: string[]; reason: string }];
        if (input.kind !== "source") return;
        for (const memberId of input.memberIds.filter((m) => m !== input.primaryId)) {
          await postCommand(ctx.taskId, "src.asset.merge", { source_asset_id: memberId, target_asset_id: input.primaryId, reason: input.reason });
        }
        await recordHistory(ctx, { op: "unmerge", params: { source_asset_ids: input.memberIds.filter((m) => m !== input.primaryId) } }, { action: "merge", summary: `归并资产到 ${input.primaryId}`, refs: [] });
      },
      async undoMerge(ctx, ...args) {
        const [mergeId] = args as [string];
        const merge = ctx.state().merges.find((m) => m.id === mergeId);
        if (!merge) return;
        for (const memberId of merge.memberIds) {
          await postCommand(ctx.taskId, "src.asset.merge.revert", { source_asset_id: memberId });
        }
      },
      async dismissDuplicates(ctx, ...args) {
        const [kind, ids] = args as [RecordKind, string[]];
        if (kind !== "source") return;
        const key = `source:${[...ids].sort().join(",")}`;
        await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key, kind: "source", ids, state: "dismissed" });
      },
      async markDuplicatesUndetermined(ctx, ...args) {
        const [kind, ids, reason, missingNote] = args as [RecordKind, string[], string, string];
        if (kind !== "source") return;
        const key = `source:${[...ids].sort().join(",")}`;
        await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key, kind: "source", ids, state: "undetermined", reason, missing_note: missingNote });
      },
      async saveFilter(ctx, ...args) {
        const [input] = args as [{ scope: string; name: string; filter: unknown }];
        await postCommand(ctx.taskId, "src.query.save", { payload: { name: input.name, scope: input.scope, filter: input.filter } });
      },
      async deleteSavedFilter(ctx, ...args) {
        const [id] = args as [string];
        const rows = await recordListAll(ctx.taskId, "saved_query");
        const row = rows.find((r) => text(r.query_id) === id || text(r.record_id) === id);
        if (row) {
          const rec = await querySrc<{ revision: number }>(ctx.taskId, "src.record.get", { kind: "saved_query", record_id: id });
          await postCommand(ctx.taskId, "src.query.delete", { query_id: id, base_revision: rec.revision });
        }
      },
      async addSource(ctx, ...args) {
        const [input] = args as [{ projectId: string; title: string; kind: StudioSource["kind"]; assetType?: SrcAssetType; text: string; author: string; canonicalKey?: string; aliases?: string[]; importance?: StudioSource["importance"] }];
        const type = (input.assetType ? resolveSrcAssetType(input.assetType) : undefined) ?? KIND_TYPE[input.kind] ?? "domain";
        const identity = buildIdentityForAsset(type, input.canonicalKey || input.title);
        const saved = await postCommand<Api>(ctx.taskId, "src.asset.observe", {
          asset_type: type, identity, source_kind: "manual", source_id: ctx.actor,
          attributes: { title: input.title, body: input.text, author: input.author, importance: input.importance ?? "normal" },
          raw_values: { body: input.text },
          aliases: input.aliases ?? [], success: true, manual: true,
        });
        const serverId = text((saved.asset as Api | undefined)?.asset_id);
        const local = ctx.proposal?.().sources.find((source) => source.title === input.title && source.id !== serverId);
        return { createdId: serverId, idMap: local && serverId ? { [local.id]: serverId } : undefined };
      },
      async importFile(ctx, ...args) {
        const [input] = args as [{
          projectId: string;
          fileName: string;
          size: number;
          text: string;
          rawBytes?: ArrayBuffer;
          settings: ImportSettings;
          drafts: ImportDraft[];
        }];
        const inferredSourceKind = inferIngestionSourceKind(input.fileName, input.settings.format);
        const canSendRawBytes = Boolean(
          input.rawBytes
          && (input.settings.combineMode ?? "per_row") === "per_row"
          && inferredSourceKind !== "auto",
        );
        const fieldMapping = buildServerFieldMapping(input.settings.mapping);
        let sourceKind = "json";
        let mediaType = "application/json";
        let contentB64: string;
        if (canSendRawBytes && input.rawBytes) {
          sourceKind = inferredSourceKind;
          mediaType = inferredSourceKind === "csv"
            ? "text/csv"
            : inferredSourceKind === "jsonl"
              ? "application/x-ndjson"
              : inferredSourceKind === "burp_xml"
                ? "application/xml"
                : inferredSourceKind === "raw_http"
                  ? "message/http"
                  : "application/json";
          contentB64 = arrayBufferToBase64(input.rawBytes);
        } else {
          const rows = input.drafts.map((draft, index) => {
            const v = draft.values;
            const kindKey = (v.kind || input.settings.defaultKind) as StudioSource["kind"];
            const assetType = resolveSrcAssetType(v.kind) ?? input.settings.defaultAssetType ?? KIND_TYPE[kindKey] ?? "domain";
            const identity = buildIdentityForAsset(assetType, v.key || v.title);
            return {
              index,
              asset_type: assetType,
              identity,
              attributes: {
                title: v.title,
                body: v.body,
                author: v.author || input.settings.defaultAuthor,
                collected_at: v.collectedAt,
                import_key: v.key,
              },
              raw: draft.raw,
            };
          });
          contentB64 = arrayBufferToBase64(new TextEncoder().encode(JSON.stringify({ items: rows })));
        }
        const saved = await postCommand<Api>(ctx.taskId, "src.ingestion.ingest", {
          source_kind: sourceKind,
          content_b64: contentB64,
          media_type: mediaType,
          encoding: input.settings.encoding || "utf-8",
          source_metadata: {
            filename: input.fileName,
            studio_format: input.settings.format,
            default_asset_type: input.settings.defaultAssetType,
          },
          field_mapping: canSendRawBytes ? fieldMapping : {},
          idempotency_key: `studio-import-${randomUuid()}`,
        });
        const localId = ctx.proposal?.().workspace.library.importBatchId;
        const serverId = text(saved.batch_id);
        if (localId && serverId && localId !== serverId) return { idMap: { [localId]: serverId } };
      },
      async undoImportBatch(ctx, ...args) {
        const [batchId] = args as [string];
        await postCommand(ctx.taskId, "src.ingestion.batch.revert", { batch_id: batchId });
      },
      async retryImportItems(ctx, ...args) {
        const [batchId] = args as [string];
        await postCommand(ctx.taskId, "src.ingestion.retry", { batch_id: batchId });
      },
      async fixImportItem(ctx, ...args) {
        const [batchId, itemId, values] = args as [string, string, ImportValues];
        const batch = ctx.state().imports.find((b) => b.id === batchId);
        const item = batch?.items.find((i) => i.id === itemId);
        if (!batch || !item) return;
        const kindKey = (values.kind || "interview") as StudioSource["kind"];
        const assetType = resolveSrcAssetType(values.kind) ?? batch.settings?.defaultAssetType ?? KIND_TYPE[kindKey] ?? "domain";
        const identity = buildIdentityForAsset(assetType, values.key || values.title);
        await postCommand(ctx.taskId, "src.asset.observe", {
          asset_type: assetType, identity, source_kind: "manual", source_id: ctx.actor,
          attributes: { title: values.title, body: values.body, author: values.author, corrected_item: `${batchId}:${item.index}` },
          raw_values: { body: values.body, batch_id: batchId, item_index: item.index },
          success: true, manual: true, source_event_id: `fix-${batchId}-${item.index}-${Date.now()}`,
        });
      },
      async runConnectorSync(ctx, ...args) {
        const [connectorId, , customQuery] = args as [string, string, string?];
        const manifests = await querySrc<Api[]>(ctx.taskId, "src.connector.manifests", {});
        const manifest = (Array.isArray(manifests) ? manifests : []).find((m) => text(m.connector_id) === connectorId);
        if (!manifest) throw new Error(`连接器 ${connectorId} 没有真实清单`);
        const caps = (manifest.capabilities as string[]) ?? [];
        const trimmedQuery = customQuery?.trim() || "";
        if (caps.includes("read_repository")) {
          const payload: Record<string, unknown> = {
            provider: connectorId,
            connector_id: connectorId,
            idempotency_key: `wb-repo-${connectorId}-${Date.now().toString(36)}`,
          };
          if (trimmedQuery) payload.repository = trimmedQuery;
          await postCommand(ctx.taskId, "src.connector.repository", payload);
          return;
        }
        if (!caps.includes("query") && !caps.includes("import_file") && !caps.includes("read")) {
          throw new Error(`连接器 ${connectorId} 不支持在线同步`);
        }
        const batch = await postCommand<Api>(ctx.taskId, "src.connector.query", {
          connector_id: connectorId,
          query: customQuery || "",
          max_items: 100,
        });
        const errors = (batch.errors as Api[] | undefined) ?? [];
        return { sourceResult: {
          state: text(batch.external_state), count: Array.isArray(batch.items) ? batch.items.length : 0,
          detail: errors.map((e) => [text(e.code), text(e.message)].filter(Boolean).join(" · ")).join("\n"),
        } };
      },
      async revertHistory(ctx, ...args) {
        const [entryId] = args as [string];
        const entry = ctx.state().history.find((e) => e.id === entryId || e.serverId === entryId);
        if (entry?.serverDomain && entry.serverDomain !== "assets") return;
        const undo = entry?.serverUndo;
        if (!undo) return;
        const params = (undo.params ?? {}) as Api;
        switch (undo.op) {
          case "lifecycle":
            for (const assetId of (params.asset_ids as string[]) ?? []) {
              await postCommand(ctx.taskId, "src.asset.lifecycle", { asset_id: assetId, action: params.state === "archived" ? "archive" : "restore" });
            }
            break;
          case "unmerge":
            for (const assetId of (params.source_asset_ids as string[]) ?? []) {
              await postCommand(ctx.taskId, "src.asset.merge.revert", { source_asset_id: assetId });
            }
            break;
          case "batch_revert":
            await postCommand(ctx.taskId, "src.ingestion.batch.revert", { batch_id: params.batch_id });
            break;
          case "verdict_clear":
            await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key: params.key, kind: "source", ids: params.ids ?? [], state: "clear" });
            break;
          case "importance":
            for (const assetId of (params.asset_ids as string[]) ?? []) {
              await postCommand(ctx.taskId, "src.workbench.asset.importance", { asset_ids: [assetId], importance: params.previous ?? "normal" });
            }
            break;
          case "noop":
            break;
          default:
            break;
        }
        await postCommand(ctx.taskId, "src.workbench.history.revert", { entry_id: entry?.serverId ?? entryId });
      },
    },
  };
}

export interface IngestionPreviewReport {
  format: string;
  total_rows: number;
  new_entities: Array<Record<string, unknown>>;
  added_observations: Array<Record<string, unknown>>;
  unrecognized_rows: Array<Record<string, unknown>>;
  counts: {
    total: number;
    new_entities: number;
    added_observations: number;
    unrecognized: number;
  };
}

export async function previewIngestionFile(
  taskId: string,
  input: {
    fileName: string;
    rawBytes?: ArrayBuffer;
    text?: string;
    format?: ImportFormat;
    encoding?: string;
    mapping?: ImportSettings["mapping"];
  },
): Promise<IngestionPreviewReport> {
  const sourceKind = inferIngestionSourceKind(input.fileName, input.format);
  const payload: Record<string, unknown> = {
    source_kind: sourceKind,
    filename: input.fileName,
    encoding: input.encoding || "utf-8",
    source_metadata: { filename: input.fileName, studio_format: input.format },
    field_mapping: buildServerFieldMapping(input.mapping),
  };
  if (input.rawBytes) {
    payload.content_b64 = arrayBufferToBase64(input.rawBytes);
  } else if (typeof input.text === "string" && input.text) {
    payload.content = input.text;
  }
  const raw = await querySrc<Api>(taskId, "src.ingestion.preview", payload);
  const newEntities = Array.isArray(raw.new_entities) ? (raw.new_entities as Array<Record<string, unknown>>) : [];
  const addedObservations = Array.isArray(raw.added_observations) ? (raw.added_observations as Array<Record<string, unknown>>) : [];
  const unrecognizedRows = Array.isArray(raw.unrecognized_rows) ? (raw.unrecognized_rows as Array<Record<string, unknown>>) : [];
  const rawCounts = (raw.counts as Api | undefined) ?? {};
  return {
    format: text(raw.format) || sourceKind,
    total_rows: num(raw.total_rows ?? rawCounts.total, newEntities.length + addedObservations.length + unrecognizedRows.length),
    new_entities: newEntities,
    added_observations: addedObservations,
    unrecognized_rows: unrecognizedRows,
    counts: {
      total: num(rawCounts.total, num(raw.total_rows, 0)),
      new_entities: num(rawCounts.new_entities, newEntities.length),
      added_observations: num(rawCounts.added_observations, addedObservations.length),
      unrecognized: num(rawCounts.unrecognized, unrecognizedRows.length),
    },
  };
}

export async function saveAssetJudgement(
  taskId: string,
  input: {
    assetId: string;
    judgementState?: StudioSource["judgementState"];
    systemName?: string;
    uncertaintyNote?: string;
    priority?: string;
    baseRevision?: number;
  },
) {
  const result = await postCommand<Api>(taskId, "src.asset.judgement.save", {
    asset_id: input.assetId,
    ...(input.judgementState ? { judgement_state: input.judgementState } : {}),
    system_name: input.systemName?.trim() ? input.systemName.trim() : null,
    uncertainty_note: input.uncertaintyNote?.trim() ? input.uncertaintyNote.trim() : null,
    ...(input.priority ? { priority: input.priority } : {}),
    ...(typeof input.baseRevision === "number" ? { base_revision: input.baseRevision } : {}),
  });
  await studio.reloadDomain("assets");
  return result;
}

export async function splitMergedAsset(
  taskId: string,
  input: { assetId: string; observationIds: string[] },
) {
  const result = await postCommand<Api>(taskId, "src.asset.split", {
    merged_asset_id: input.assetId,
    observation_ids: input.observationIds,
  });
  await studio.reloadDomain("assets");
  return result;
}

export async function putConnectorCredential(
  taskId: string,
  input: { alias: string; value: string; scope: "project" | "task"; projectId?: string; reference?: string },
): Promise<{ reference: string; scope: string }> {
  const saved = await postCommand<Api>(taskId, "src.credential.put", {
    alias: input.alias,
    value: input.value,
    scope: input.scope,
    ...(input.scope === "project" && input.projectId ? { project_id: input.projectId } : {}),
    ...(input.reference ? { reference: input.reference } : {}),
  });
  return {
    reference: text(saved.reference),
    scope: text(saved.scope) || input.scope,
  };
}

export async function saveConnectorConfig(
  taskId: string,
  input: { connectorId: string; config: Record<string, unknown> },
) {
  const result = await postCommand<Api>(taskId, "src.connector.config.save", {
    connector_id: input.connectorId,
    config: input.config,
  });
  await studio.reloadDomain("assets");
  return result;
}

export async function verifyConnectorConnection(taskId: string, connectorId: string) {
  const result = await postCommand<Api>(taskId, "src.connector.verify", {
    connector_id: connectorId,
  });
  await studio.reloadDomain("assets");
  return result;
}

export async function saveConnectorProjectGrant(
  taskId: string,
  input: {
    connectorId: string;
    projectId?: string;
    allowedPurposes: string[];
    budgetLimit?: number | null;
    notes?: string;
    baseRevision?: number;
  },
) {
  const result = await postCommand<Api>(taskId, "src.connector.grant.save", {
    connector_id: input.connectorId,
    ...(input.projectId ? { project_id: input.projectId } : {}),
    allowed_purposes: input.allowedPurposes,
    budget_limit: input.budgetLimit ?? null,
    ...(input.notes ? { notes: input.notes } : {}),
    ...(typeof input.baseRevision === "number" ? { base_revision: input.baseRevision } : {}),
  });
  await studio.reloadDomain("assets");
  return result;
}

export async function revokeConnectorProjectGrant(
  taskId: string,
  input: { connectorId: string; baseRevision?: number },
) {
  const result = await postCommand<Api>(taskId, "src.connector.grant.revoke", {
    connector_id: input.connectorId,
    ...(typeof input.baseRevision === "number" ? { base_revision: input.baseRevision } : {}),
  });
  await studio.reloadDomain("assets");
  return result;
}
