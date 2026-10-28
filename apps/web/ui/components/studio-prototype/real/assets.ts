/**
 * The real asset domain: binds the 资产 workbench (sources, imports, merges,
 * filters, duplicate verdicts, connectors, asset-side history) to the SRC
 * backend records on a real task. Server records are authoritative; see
 * `domain.ts` for the seam contract.
 */
import type {
  AssetFieldConflict, AssetObservation, AssetRelationItem,
  StudioConnector, StudioRef, StudioSource,
} from "../model";
import type {
  HistoryEntry, MergeRecord, RecordKind, SavedFilter, UndeterminedDuplicate,
} from "../records";
import type { ImportBatch, ImportItem } from "../store";
import type { ImportDraft, ImportFormat, ImportSettings, ImportValues } from "../importParsing";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { DomainCtx, RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";
const PAGE = 200;
const LOAD_CAP = 2000;

/** asset_type -> workbench SourceKind (SOURCE_KINDS: 子域名/Web服务/API服务/CDN节点). */
const TYPE_KIND: Record<string, StudioSource["kind"]> = {
  domain: "interview", dns: "interview", root_domain: "interview",
  web_service: "field", non_web_service: "field", port: "field",
  http_endpoint: "archive", parameter: "archive", app: "archive", miniprogram: "archive",
  cloud: "minutes", repository: "minutes", cdn: "minutes", ip: "minutes", cidr: "minutes",
};
const KIND_TYPE: Record<StudioSource["kind"], string> = {
  interview: "domain", field: "web_service", archive: "http_endpoint", minutes: "ip",
};

function sourceKindOf(asset: Api): StudioSource["kind"] {
  const override = text((asset.current_profile as Api | undefined)?.workbench_kind);
  if (override === "interview" || override === "field" || override === "archive" || override === "minutes") return override;
  return TYPE_KIND[text(asset.type) || text(asset.asset_type)] ?? "interview";
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
    artifactRef: ((obs.artifact_refs as Api[]) ?? [])[0] ? text(((obs.artifact_refs as Api[])[0] as Api).artifact_id) || text(((obs.artifact_refs as Api[])[0] as Api).sha256) : "",
    sha256: text(((obs.artifact_refs as Api[])?.[0] as Api | undefined)?.sha256),
    summary: text(obs.summary) || Object.entries((obs.attributes as Api) ?? {}).slice(0, 4).map(([k, v]) => `${k}=${v}`).join(" · "),
    fields,
  };
}

function assetToSource(row: Api, projectId: string): StudioSource {
  const profile = (row.current_profile ?? row.profile ?? {}) as Api;
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
          : Object.entries((obs.attributes as Api) ?? {}).map(([k, v]) => `${k}: ${v}`).join("\n") || "（无文本正文——以属性与观测记录为准）",
      }))
    : [{
        id: `${id}-attrs`,
        sourceId: id,
        index: 1,
        text: Object.entries(attrs).map(([k, v]) => `${k}: ${v}`).join("\n") || "（无文本正文——以属性与观测记录为准）",
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
  const relations: AssetRelationItem[] = ((row.relations as Api[]) ?? []).map((rel) => ({
    id: text(rel.relation_id),
    direction: text(rel.from_asset_id) === id ? "outbound" : "inbound",
    relationType: text(rel.relation_type),
    targetId: text(rel.from_asset_id) === id ? text(rel.to_asset_id) : text(rel.from_asset_id),
    targetLabel: "",
    targetKind: "source",
    origin: (["observed", "derived", "inferred"].includes(text(rel.origin)) ? text(rel.origin) : "observed") as AssetRelationItem["origin"],
    evidenceRef: "",
    observedAt: iso(rel.valid_from) || iso(rel.created_at),
  }));
  return {
    id,
    projectId,
    title: text(row.display_name) || text(row.canonical_key) || id,
    kind: sourceKindOf(row),
    author: text(observations[0]?.source_id) || "SRC 资产库",
    collectedAt: iso(row.first_seen_at) || iso(row.first_observed),
    status: reviewStatus(row),
    passages,
    canonicalKey: text(row.canonical_key) || text(row.natural_key),
    aliases: ((row.aliases as unknown[]) ?? []).map(String).filter(Boolean),
    importance: (["core", "high", "normal", "low"].includes(text(row.importance)) ? text(row.importance) : "normal") as StudioSource["importance"],
    tags: ((row.tags as unknown[]) ?? []).map((t) => (typeof t === "string" ? t : text((t as Api).name) || text((t as Api).tag_id))).filter(Boolean),
    firstSeenAt: iso(row.first_seen_at) || iso(row.first_observed),
    lastObservedAt: iso(row.last_observed_at) || iso(row.latest_observed),
    lastVerifiedAt: iso(row.last_success_at) || iso(row.last_success),
    lastFailedAt: iso(row.last_failure_at) || iso(row.last_failure),
    archived: text(row.lifecycle) === "archived",
    attributes: attrs,
    sourceHistory: observations.map(observationToItem),
    conflicts,
    relations,
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
  } while (cursor >= 0 && items.length < LOAD_CAP && items.length < total);
  return { items, total, complete: complete && items.length >= total };
}

function batchToImport(row: Api): ImportBatch {
  const meta = (row.source_metadata ?? {}) as Api;
  return {
    id: text(row.batch_id) || text(row.record_id),
    projectId: "",
    fileName: text(meta.filename) || text(row.source_id) || text(row.batch_id),
    format: (["csv", "json"].includes(text(row.source_kind)) ? text(row.source_kind) : "txt") as ImportFormat,
    size: num((row.original_artifact as Api | undefined)?.size),
    importedAt: iso(row.created_at),
    text: "",
    settings: {} as ImportSettings,
    items: ((row.items as Api[]) ?? []).map((item): ImportItem => ({
      id: `${row.batch_id}-${num(item.index)}`,
      index: num(item.index),
      location: { label: `#${num(item.index) + 1}`, line: num(item.index) + 1, endLine: num(item.index) + 1 },
      raw: "",
      values: { title: text(item.asset_id), body: "", kind: "", author: "", collectedAt: "", key: "" },
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
  fofa: "search_engine", quake: "search_engine", hunter: "search_engine", zoomeye: "search_engine",
  bbot: "recon_pipeline", reconftw: "recon_pipeline", scopesentry: "recon_pipeline",
  burp: "proxy_import", caido: "proxy_import", bbrf: "recon_pipeline", cli: "recon_pipeline",
  cloud_inventory: "asset_repo", github: "asset_repo", gitlab: "asset_repo", aws_ec2: "asset_repo",
  external_event: "mcp_plugin",
};

function manifestToConnector(m: Api): StudioConnector {
  const id = text(m.connector_id);
  const caps = (m.capabilities as string[]) ?? [];
  return {
    id,
    name: id.toUpperCase(),
    category: CONNECTOR_CATEGORIES[id] ?? "mcp_plugin",
    version: text(m.version),
    status: "ready",
    capabilities: caps,
    supportedAssetTypes: Object.keys((m.types as Api) ?? {}),
    defaultQuery: "",
    quotaRemaining: 0,
    quotaTotal: 0,
    costPerSync: 0,
    completenessGuarantee: text((m.completeness as Api | undefined)?.kind) || "unknown",
    description: `真实连接器清单 · ${text(m.schema) || "capability manifest"}`,
  };
}

// ---------------------------------------------------------------------------
// Server history helpers
// ---------------------------------------------------------------------------

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: Omit<HistoryEntry, "id" | "at">) {
  const entry = ctx.state().history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, scope: entry.scope, refs: entry.refs, at: entry.at }
    : { id: `wb-${Date.now().toString(36)}`, action: fallback.action, summary: fallback.summary, scope: fallback.scope, refs: fallback.refs, at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "assets" } });
}

async function bulkQuery(ctx: DomainCtx) {
  return paged((cursor) => querySrc(ctx.taskId, "src.workbench.asset.bulk", { cursor, limit: PAGE }));
}

async function recordList(ctx: DomainCtx, kind: string, limit = 500) {
  const page = await querySrc<{ records?: Array<{ payload: Api; revision: number }>; next_cursor?: string | null }>(ctx.taskId, "src.record.list", { kind, limit });
  return (page.records ?? []).map((r) => r.payload);
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
        // Findings domain owns the clue-kind part; keep it (and unknown local kinds) here.
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
      const [bulk, importRows, tagRows, linkRows, verdicts, histPage, manifests, savedQueries, allAssets] = await Promise.all([
        bulkQuery(ctx),
        recordList(ctx, "import_batch"),
        recordList(ctx, "tag"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.asset.links", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.duplicate.verdicts", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<Api[]>(taskId, "src.connector.manifests", {}),
        querySrc<{ records?: Array<{ payload: Api; record_id?: string }> }>(taskId, "src.record.list", { kind: "saved_query", limit: 500 }),
        recordList(ctx, "asset", 1000),
      ]);
      const taskLinks = await querySrc<{ items?: Api[] }>(taskId, "src.workbench.asset.tasklinks", {});
      const detached = new Set(((taskLinks.items ?? [])).filter((l) => l.state === "detached").map((l) => text(l.asset_id)));
      const tagName = new Map(tagRows.map((t) => [text(t.tag_id) || text(t.record_id), text(t.name) || text((t.payload as Api)?.name)]));
      const projectId = `task-${taskId}`;
      const sources = bulk.items.map((raw) => {
        const source = assetToSource(raw, projectId);
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
      const { dismissed, undetermined } = verdictsToLocal(verdicts.items ?? []);
      const savedFilters: SavedFilter[] = ((savedQueries.records ?? []).map((r) => r.payload)).filter((q) => text(q.scope) !== "findings").map((q) => ({
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
      const imports = await Promise.all(importRows.map(async (row) => {
        const batch = batchToImport(row);
        const artifact = (row.original_artifact ?? {}) as Api;
        if (artifact.sha256) {
          try {
            const raw = await querySrc<unknown>(taskId, "src.artifact.read", { ref_data: artifact });
            const env = Array.isArray(raw) ? (raw[0] as Api) : (raw as Api);
            if (env && env.encoding === "base64" && typeof env.data === "string") {
              batch.text = new TextDecoder("utf-8", { fatal: false }).decode(Uint8Array.from(atob(env.data), (c) => c.charCodeAt(0)));
            }
          } catch { batch.text = ""; }
        }
        return batch;
      }));
      imports.sort((a, b) => b.importedAt.localeCompare(a.importedAt));
      return {
        sources,
        imports,
        importOrigins,
        savedFilters,
        dismissedDuplicates: dismissed,
        undeterminedDuplicates: undetermined,
        connectors: (Array.isArray(manifests) ? manifests : []).map(manifestToConnector),
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
        const tagRows = await recordList(ctx, "tag");
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
        const rows = await recordList(ctx, "saved_query");
        const row = rows.find((r) => text(r.query_id) === id || text(r.record_id) === id);
        if (row) {
          const rec = await querySrc<{ revision: number }>(ctx.taskId, "src.record.get", { kind: "saved_query", record_id: id });
          await postCommand(ctx.taskId, "src.query.delete", { query_id: id, base_revision: rec.revision });
        }
      },
      async addSource(ctx, ...args) {
        const [input] = args as [{ projectId: string; title: string; kind: StudioSource["kind"]; text: string; author: string; canonicalKey?: string; aliases?: string[]; importance?: StudioSource["importance"] }];
        const type = KIND_TYPE[input.kind] ?? "domain";
        const identity = type === "http_endpoint" ? { url: input.canonicalKey || input.title, method: "GET" }
          : type === "ip" ? { address: input.canonicalKey || input.title }
          : type === "web_service" ? { host: input.canonicalKey || input.title, port: 443, protocol: "https" }
          : { name: input.canonicalKey || input.title };
        await postCommand(ctx.taskId, "src.asset.observe", {
          asset_type: type, identity, source_kind: "manual", source_id: ctx.actor,
          attributes: { title: input.title, body: input.text, author: input.author, importance: input.importance ?? "normal" },
          raw_values: { body: input.text },
          aliases: input.aliases ?? [], success: true, manual: true,
        });
      },
      async importFile(ctx, ...args) {
        const [input] = args as [{ projectId: string; fileName: string; size: number; text: string; settings: ImportSettings; drafts: ImportDraft[] }];
        // The drafts already carry the user's column mapping; translate them
        // into backend-native items and ingest as one real batch.
        const rows = input.drafts.map((draft, index) => {
          const v = draft.values;
          const kindKey = (v.kind || input.settings.defaultKind) as StudioSource["kind"];
          const assetType = KIND_TYPE[kindKey] ?? "domain";
          const identity = assetType === "http_endpoint" ? { url: v.key || v.title, method: "GET" }
            : assetType === "ip" ? { address: v.key || v.title }
            : assetType === "web_service" ? { host: v.key || v.title, port: 443, protocol: "https" }
            : { name: v.key || v.title };
          return { index, asset_type: assetType, identity, attributes: { title: v.title, body: v.body, author: v.author || input.settings.defaultAuthor, collected_at: v.collectedAt, import_key: v.key }, raw: draft.raw };
        });
        const content_b64 = btoa(unescape(encodeURIComponent(JSON.stringify({ items: rows }))));
        await postCommand(ctx.taskId, "src.ingestion.ingest", {
          source_kind: "json", content_b64,
          media_type: "application/json",
          source_metadata: { filename: input.fileName, studio_format: input.settings.format },
          field_mapping: {},
          idempotency_key: `studio-import-${input.fileName}-${input.size}`,
        });
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
        const assetType = KIND_TYPE[kindKey] ?? "domain";
        const identity = assetType === "http_endpoint" ? { url: values.key || values.title, method: "GET" }
          : assetType === "ip" ? { address: values.key || values.title }
          : assetType === "web_service" ? { host: values.key || values.title, port: 443, protocol: "https" }
          : { name: values.key || values.title };
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
        if (!manifest) throw new Error(`连接器 ${connectorId} 没有真实清单（未接入）`);
        const caps = (manifest.capabilities as string[]) ?? [];
        if (!caps.includes("query") && !caps.includes("import_file") && !caps.includes("read") && !caps.includes("read_repository")) {
          throw new Error(`连接器 ${connectorId} 不支持在线同步（仅文件导入或未接入）`);
        }
        await postCommand(ctx.taskId, "src.connector.query", {
          connector_id: connectorId,
          query: customQuery || "",
          max_items: 100,
        });
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
