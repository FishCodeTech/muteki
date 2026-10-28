import { postCommand, querySrc } from "./reviewPrimitives";
import type { ApiRecord, Asset, AssetMonitoringApi, ImportBatch, Monitor, MonitorDetails, PageInfo } from "./assetMonitoringTypes";

function assetView(value: ApiRecord): Asset {
  const identity = (value.identity && typeof value.identity === "object" ? value.identity : {}) as ApiRecord;
  return {
    ...value,
    asset_type: String(value.asset_type || value.type || "unknown"),
    canonical_key: String(value.canonical_key || value.natural_key || value.asset_id),
    display_name: String(value.display_name || identity.url || identity.name || identity.address || identity.host || value.asset_id),
    revision: Number(value.revision || value._record_revision || 1),
    profile: value.current_profile || value.profile,
    judgement: value.judgement || value.judgements,
    first_seen_at: (value.first_seen_at || value.first_observed || null) as string | null,
    last_observed_at: (value.last_observed_at || value.latest_observed || null) as string | null,
    last_success_at: (value.last_success_at || value.last_success || null) as string | null,
    last_failure_at: (value.last_failure_at || value.last_failure || null) as string | null,
  } as Asset;
}

function records<T>(value: unknown, keys: string[]): T[] {
  if (Array.isArray(value)) return value as T[];
  if (!value || typeof value !== "object") return [];
  const row = value as ApiRecord;
  for (const key of keys) {
    if (!Array.isArray(row[key])) continue;
    return (row[key] as unknown[]).map(item => {
      if (item && typeof item === "object" && "payload" in item) {
        const record = item as { payload: unknown; revision?: number; record_id?: string };
        const payload = record.payload;
        if (payload && typeof payload === "object" && !Array.isArray(payload)) {
          const payloadObj = payload as ApiRecord;
          return {
            ...payloadObj,
            record_id: payloadObj.record_id ?? record.record_id,
            revision: payloadObj.revision ?? record.revision,
            _record_revision: record.revision,
          } as T;
        }
      }
      return item as T;
    });
  }
  return [];
}

function page<T>(value: unknown, keys: string[]): PageInfo & { items: T[] } {
  const record = value && typeof value === "object" && !Array.isArray(value) ? value as ApiRecord : {};
  return { ...record, items: records<T>(value, keys) } as PageInfo & { items: T[] };
}

function monitorView(value: ApiRecord): Monitor {
  const current = (value.monitor && typeof value.monitor === "object" ? value.monitor : value) as ApiRecord;
  const definition = (current.definition && typeof current.definition === "object" ? current.definition : current) as ApiRecord;
  const schedule = (definition.schedule || {}) as ApiRecord;
  const baseline = current.baseline && typeof current.baseline === "object" ? current.baseline as ApiRecord : null;
  return {
    ...definition, ...current,
    monitor_id: String(current.monitor_id || definition.monitor_id),
    name: String(definition.name || current.name || definition.monitor_id || current.monitor_id),
    chain_type: String(definition.chain_type || current.chain_type || "asset"),
    version: Number(definition.version || current.current_version || 1),
    revision: Number(current.revision || 0),
    query_version: String(definition.query_version || definition.query_ref || definition.target_selection && (definition.target_selection as ApiRecord).query_version || ""),
    source_refs: definition.source_refs || [definition.source].filter(Boolean),
    authorization_ref: String(definition.authorization_ref || current.authorization_ref || ""),
    interval_seconds: Number(schedule.interval_seconds || definition.interval_seconds || 3600),
    timezone: String(schedule.timezone || definition.timezone || "UTC"),
    budget: definition.budget || current.budget,
    notification: definition.notification || current.notification,
    status: String(current.status || definition.status || "unknown"),
    baseline_id: String(current.baseline_id || baseline?.baseline_id || ""),
    last_success_at: (current.last_success_at || baseline?.finished_at || null) as string | null,
  } as Monitor;
}

async function downloadExport(taskId: string, input: { query: string; format: "csv" | "json" | "zip" }): Promise<Blob> {
  const result = await querySrc<unknown>(taskId, "src.asset.export", {
    expression: input.query, export_format: input.format, partial: false,
  });
  if (!Array.isArray(result) || !result[0] || typeof result[0] !== "object") throw new Error("资产导出缺少二进制内容");
  const row = result[0] as { encoding?: string; data?: string };
  if (row.encoding !== "base64" || !row.data) throw new Error("资产导出缺少 base64 内容");
  const bytes = Uint8Array.from(atob(row.data), char => char.charCodeAt(0));
  return new Blob([bytes], { type: String(result[1] || "application/octet-stream") });
}

export const assetMonitoringApi: AssetMonitoringApi = {
  queryAssets: async (taskId, input) => {
    const result = page(await querySrc(taskId, "src.asset.query", {
      expression: input.query, cursor: input.cursor, limit: input.limit,
    }), ["items", "assets", "results"]);
    return { ...result, items: result.items.map(item => assetView(item as ApiRecord)) } as Awaited<ReturnType<AssetMonitoringApi["queryAssets"]>>;
  },
  getAsset: async (taskId, assetId) => {
    const [result, stored] = await Promise.all([
      querySrc<ApiRecord>(taskId, "src.asset.detail", { asset_id: assetId }),
      querySrc<ApiRecord>(taskId, "src.record.get", { kind: "asset", record_id: assetId }),
    ]);
    const asset = (result.asset || result) as ApiRecord;
    return {
      ...result, ...assetView({ ...asset, _record_revision: stored.revision }),
    } as Awaited<ReturnType<AssetMonitoringApi["getAsset"]>>;
  },
  listAssetObservations: async (taskId, assetId, input) => page(await querySrc(taskId, "src.record.list", {
    kind: "asset_observation", parent_id: assetId, cursor: input.cursor, limit: input.limit,
  }), ["records", "items", "observations", "history", "sources"]),
  listAssetRelations: async (taskId, assetId, input) => {
    const result = await querySrc<ApiRecord>(taskId, "src.asset.pivot", { asset_id: assetId, cursor: input.cursor || 0, limit: input.limit });
    const rows = records<ApiRecord>(result, ["items", "relations"]).map(item => ({
      ...(item.relation && typeof item.relation === "object" ? item.relation : item),
      other_asset: item.asset,
    }));
    return { ...result, items: rows } as Awaited<ReturnType<AssetMonitoringApi["listAssetRelations"]>>;
  },
  saveQuery: (taskId, input) => postCommand(taskId, "src.query.save", input),
  deleteQuery: async (taskId, queryId) => {
    const current = await querySrc<ApiRecord>(taskId, "src.record.get", { kind: "saved_query", record_id: queryId });
    await postCommand(taskId, "src.query.delete", { query_id: queryId, base_revision: current.revision });
  },
  listSavedQueries: async (taskId) => records(await querySrc(taskId, "src.record.list", { kind: "saved_query", limit: 500 }), ["records", "items", "queries"]),
  publishAsset: (taskId, assetId, input) => postCommand(taskId, "src.asset.publish", {
    target_task_id: input.target_task_id || input.dataset_id,
    purpose: input.purpose,
    selection: {
      assets: [{ asset_id: assetId, asset_revision: input.asset_revision, fields: input.fields || [] }],
      relation_ids: input.relation_ids || [],
      observation_ids: input.observation_ids || [],
      artifact_refs: input.artifact_refs || [],
      source_watermarks: input.source_watermarks || [],
    },
  }),
  saveAnnotation: async (taskId, assetId, input) => {
    if (input.kind === "tag") {
      const tag = await postCommand<ApiRecord>(taskId, "src.tag.save", {
        tag_id: input.annotation_id || undefined,
        payload: { name: input.name || input.value || input.text },
        base_revision: input.annotation_id ? input.base_revision : undefined,
      });
      return postCommand(taskId, "src.tag.assign", {
        asset_ids: [assetId], tag_ids: [tag.tag_id || tag.record_id], detach: input.association === "detach",
      });
    }
    return postCommand(taskId, "src.note.save", {
      asset_id: assetId, note_id: input.annotation_id || undefined,
      payload: { text: input.text || input.value, origin: "human" },
      base_revision: input.annotation_id ? input.base_revision : undefined,
    });
  },
  deleteAnnotation: async (taskId, assetId, annotationId, kind = "note") => {
    if (kind === "tag") {
      await postCommand(taskId, "src.tag.assign", { asset_ids: [assetId], tag_ids: [annotationId], detach: true });
      return;
    }
    await postCommand(taskId, "src.note.save", {
      asset_id: assetId, note_id: annotationId,
      payload: { deleted_at: new Date().toISOString() },
    });
  },
  setAssetArchived: async (taskId, assetId, archived, revision) => {
    await postCommand(taskId, "src.asset.lifecycle", {
      asset_id: assetId,
      action: archived ? "archive" : "restore",
      expected_revision: revision,
    });
  },
  listImports: async (taskId) => records<ImportBatch>(await querySrc(taskId, "src.record.list", { kind: "import_batch", limit: 500 }), ["records", "items", "imports", "batches"]),
  getImport: (taskId, batchId) => querySrc(taskId, "src.record.get", { kind: "import_batch", record_id: batchId }),
  retryImport: (taskId, batchId) => postCommand(taskId, "src.ingestion.retry", { batch_id: batchId }),
  exportAssets: downloadExport,
  listMonitors: async (taskId) => records<ApiRecord>(await querySrc(taskId, "src.monitor.list"), ["items", "monitors"]).map(monitorView),
  createMonitor: async (taskId, input) => {
    const revision = Number(input.base_revision || 0);
    const sourceRefs = Array.isArray(input.source_refs) ? input.source_refs : [];
    const source = (sourceRefs[0] && typeof sourceRefs[0] === "object" ? sourceRefs[0] : undefined) as ApiRecord | undefined;
    const sourceRef = String(source?.source_id || input.source_ref || "").trim();
    const separator = sourceRef.lastIndexOf(":");
    const connectorId = String(source?.connector_id || sourceRef.slice(0, separator));
    const configVersion = Number(source?.config_version || sourceRef.slice(separator + 1));
    if (!connectorId || separator < 1 || !Number.isInteger(configVersion) || configVersion < 1) {
      throw new Error("来源引用须为 connector-id:配置版本，例如 fofa:1");
    }
    const config = await querySrc<ApiRecord>(taskId, "src.connector.config", { connector_id: connectorId, version: configVersion });
    const manifest = (config.manifest || {}) as ApiRecord;
    const manifestRef = String(manifest.version || "");
    if (!manifestRef) throw new Error("来源能力版本不可用");
    const fields = ((manifest.credentials as ApiRecord | undefined)?.reference_fields || []) as string[];
    const settings = (config.config || {}) as ApiRecord;
    const credentialRefs = Object.fromEntries(fields.filter(field => settings[field]).map(field => [field, String(settings[field])]));
    const budgetInput = (input.budget && typeof input.budget === "object" ? input.budget : {}) as ApiRecord;
    const totalBudget = Number(budgetInput.total ?? budgetInput.amount ?? 0);
    const perCycleBudget = Number(budgetInput.per_cycle ?? budgetInput.amount ?? 0);
    const payload = {
      name: input.name || undefined,
      continuous: true,
      chain_type: input.chain_type,
      query_ref: input.query_version,
      plan_ref: input.plan_ref,
      target_selection: { query_version: input.query_version },
      source: { connector_id: connectorId, capability_manifest_ref: manifestRef, config_version: configVersion, query: input.source_query, credential_refs: credentialRefs },
      source_refs: sourceRefs,
      authorization_ref: input.authorization_ref,
      schedule: { interval_seconds: input.interval_seconds, timezone: input.timezone },
      budget: {
        total: totalBudget,
        per_cycle: perCycleBudget,
        call_limit: Number(budgetInput.call_limit || 0),
        currency: String(budgetInput.currency || "USD"),
      },
      notification: input.notification,
    };
    const result = await postCommand<ApiRecord>(taskId, revision ? "src.monitor.revise" : "src.monitor.create", revision ? {
      monitor_id: input.monitor_id, expected_revision: revision, payload,
    } : { payload });
    return monitorView(result);
  },
  getMonitor: async (taskId, monitorId) => {
    const [current, changes, notifications] = await Promise.all([
      querySrc<ApiRecord>(taskId, "src.monitor.get", { monitor_id: monitorId }),
      querySrc<ApiRecord>(taskId, "src.monitor.changes", { monitor_id: monitorId }),
      querySrc<ApiRecord>(taskId, "src.monitor.notifications", { monitor_id: monitorId }),
    ]);
    const baseline = current.baseline && typeof current.baseline === "object" ? [current.baseline] : [];
    return {
      ...current, revision: Number(current.revision || 0), monitor: monitorView(current), cycles: records(current, ["cycles"]),
      baselines: records(current, ["baselines"]).concat(baseline),
      changes: records(changes, ["items", "changes"]),
      notifications: records(notifications, ["items", "notifications"]),
    } as MonitorDetails;
  },
  controlMonitor: async (taskId, monitorId, action) => {
    if (action === "run") {
      await postCommand(taskId, "src.monitor.run_once", { monitor_id: monitorId });
      return;
    }
    const current = await querySrc<ApiRecord>(taskId, "src.monitor.get", { monitor_id: monitorId });
    await postCommand(taskId, "src.monitor.state", { monitor_id: monitorId, state: action === "pause" ? "paused" : action === "resume" ? "active" : "cancelled", expected_revision: current.revision });
  },
};
