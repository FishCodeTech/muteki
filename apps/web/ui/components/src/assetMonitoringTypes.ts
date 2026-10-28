export type ApiRecord = Record<string, unknown>;

export type PageInfo = {
  next_cursor?: string | null;
  total?: number | null;
  returned?: number;
  complete?: boolean;
  partial_reason?: string;
  snapshot?: string;
  watermarks?: ApiRecord[];
};

export type Asset = {
  asset_id: string;
  asset_type: string;
  canonical_key: string;
  display_name?: string;
  revision: number;
  profile?: ApiRecord;
  judgement?: ApiRecord;
  aliases?: Array<string | ApiRecord>;
  first_seen_at?: string | null;
  last_observed_at?: string | null;
  last_success_at?: string | null;
  last_failure_at?: string | null;
  archived_at?: string | null;
  tags?: Array<string | ApiRecord>;
  [key: string]: unknown;
};

export type AssetObservation = {
  observation_id: string;
  asset_id: string;
  source_kind: string;
  source_id: string;
  observed_at?: string | null;
  ingested_at: string;
  attributes?: ApiRecord;
  raw_values?: ApiRecord;
  completeness?: string;
  success?: boolean | null;
  parser_version?: string;
  selected?: boolean;
  selection_reason?: string;
  revoked_at?: string | null;
  [key: string]: unknown;
};

export type AssetRelation = {
  relation_id: string;
  from_asset_id: string;
  to_asset_id: string;
  relation_type: string;
  origin: "observed" | "derived" | "inferred" | string;
  evidence_refs?: unknown[];
  other_asset?: Asset;
  [key: string]: unknown;
};

export type AssetQueryResult = PageInfo & {
  items: Asset[];
  query?: ApiRecord;
  semantic_version?: string;
  reasons?: ApiRecord[];
};

export type ImportItem = {
  index: number;
  status: string;
  code?: string;
  field?: string;
  message?: string;
  recovery_hint?: string;
  asset_id?: string;
  [key: string]: unknown;
};

export type ImportBatch = {
  batch_id: string;
  status: string;
  source_kind?: string;
  source_name?: string;
  created_at?: string;
  artifact?: ApiRecord;
  field_mapping?: ApiRecord;
  preview?: ApiRecord[];
  items?: ImportItem[];
  counts?: Partial<Record<"processed" | "created" | "updated" | "merged" | "skipped" | "failed" | "truncated", number>>;
  completeness?: string;
  next_page?: string | null;
  upstream_limit?: number | null;
  cost?: ApiRecord;
  [key: string]: unknown;
};

export type Monitor = {
  monitor_id: string;
  name: string;
  chain_type: "asset" | "rule" | string;
  version: number;
  revision: number;
  query_version: string;
  source_refs: unknown[];
  authorization_ref?: string;
  interval_seconds: number;
  timezone: string;
  budget?: ApiRecord;
  notification?: ApiRecord;
  status: "active" | "paused" | "cancelled" | string;
  last_success_at?: string | null;
  baseline_id?: string;
  [key: string]: unknown;
};

export type MonitorCycle = {
  cycle_id: string;
  monitor_id: string;
  definition_version: number;
  status: string;
  scheduled_at: string;
  started_at?: string | null;
  finished_at?: string | null;
  completeness?: string;
  baseline_id?: string;
  compared_baseline_id?: string;
  source_watermarks?: ApiRecord;
  cost?: ApiRecord;
  end_reason?: string;
  [key: string]: unknown;
};

export type ChangeEvent = {
  change_id: string;
  monitor_id: string;
  cycle_id: string;
  chain_type: string;
  change_type: string;
  subject_ref: ApiRecord;
  before_ref?: ApiRecord;
  after_ref?: ApiRecord;
  raw_diff?: unknown;
  denoised_diff?: unknown;
  denoise_rule?: ApiRecord;
  comparable: boolean;
  incomparable_reason?: string;
  completeness?: string;
  candidate?: ApiRecord;
  notification_ids?: string[];
  [key: string]: unknown;
};

export type MonitorDetails = {
  monitor: Monitor;
  revision: number;
  cycles: MonitorCycle[];
  baselines: ApiRecord[];
  changes: ChangeEvent[];
  notifications?: ApiRecord[];
};

export type AssetMonitoringApi = {
  queryAssets(taskId: string, input: { query: string; cursor?: string; limit: number }): Promise<AssetQueryResult>;
  getAsset(taskId: string, assetId: string): Promise<Asset>;
  listAssetObservations(taskId: string, assetId: string, input: { cursor?: string; limit: number }): Promise<PageInfo & { items: AssetObservation[] }>;
  listAssetRelations(taskId: string, assetId: string, input: { cursor?: string; limit: number }): Promise<PageInfo & { items: AssetRelation[] }>;
  saveQuery(taskId: string, input: ApiRecord): Promise<ApiRecord>;
  deleteQuery(taskId: string, queryId: string): Promise<void>;
  listSavedQueries(taskId: string): Promise<ApiRecord[]>;
  publishAsset(taskId: string, assetId: string, input: ApiRecord): Promise<ApiRecord>;
  saveAnnotation(taskId: string, assetId: string, input: ApiRecord): Promise<ApiRecord>;
  deleteAnnotation(taskId: string, assetId: string, annotationId: string, kind?: "note" | "tag"): Promise<void>;
  setAssetArchived(taskId: string, assetId: string, archived: boolean, revision?: number): Promise<void>;
  listImports(taskId: string): Promise<ImportBatch[]>;
  getImport(taskId: string, batchId: string): Promise<ImportBatch>;
  retryImport(taskId: string, batchId: string): Promise<ImportBatch>;
  exportAssets(taskId: string, input: { query: string; format: "csv" | "json" | "zip" }): Promise<Blob>;
  listMonitors(taskId: string): Promise<Monitor[]>;
  createMonitor(taskId: string, input: ApiRecord): Promise<Monitor>;
  getMonitor(taskId: string, monitorId: string): Promise<MonitorDetails>;
  controlMonitor(taskId: string, monitorId: string, action: "pause" | "resume" | "cancel" | "run"): Promise<void>;
};
