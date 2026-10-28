import { querySrc } from "./reviewPrimitives";

export type ArtifactRef = {
  artifact_id: string;
  sha256: string;
  media_type: string;
  size: number;
  task_id: string;
  source_run_id?: string | null;
  source_event_seq?: number | null;
  version?: number;
  original?: boolean;
  complete?: string;
  name?: string;
};

export type ArtifactRecord = { record_id: string; kind: string; revision: number; payload: unknown };
export type ArtifactRecordPage = {
  records: ArtifactRecord[];
  total: number;
  next_cursor?: string | null;
  complete: boolean;
  partial_reason?: string | null;
  watermarks?: Array<Record<string, unknown>>;
};
export type IndexedArtifact = {
  ref: ArtifactRef;
  source_kind: string;
  source_record_id: string;
  sources?: Array<{ source_kind: string; source_record_id: string; artifact_id: string }>;
};

export function indexedArtifacts(records: ArtifactRecord[]): IndexedArtifact[] {
  const entries = new Map<string, IndexedArtifact>();
  const visit = (value: unknown, record: ArtifactRecord) => {
    if (Array.isArray(value)) { value.forEach(item => visit(item, record)); return; }
    if (!value || typeof value !== "object") return;
    const row = value as Record<string, unknown>;
    if (typeof row.sha256 === "string" && /^[a-f0-9]{64}$/.test(row.sha256) && typeof row.task_id === "string") {
      const ref = row as unknown as ArtifactRef;
      const key = `${ref.sha256}:${ref.artifact_id || ""}`;
      const occurrence = { source_kind: record.kind, source_record_id: record.record_id, artifact_id: ref.artifact_id || ref.sha256 };
      const previous = entries.get(key) || entries.get(ref.sha256);
      if (!previous) {
        entries.set(key, { ref, source_kind: record.kind, source_record_id: record.record_id, sources: [occurrence] });
      } else {
        const sources = [...(previous.sources || []), occurrence];
        const preferNew = previous.source_kind.localeCompare(record.kind) > 0;
        entries.set(key, {
          ref: preferNew ? ref : previous.ref,
          source_kind: preferNew ? record.kind : previous.source_kind,
          source_record_id: preferNew ? record.record_id : previous.source_record_id,
          sources,
        });
      }
    }
    Object.values(row).forEach(item => visit(item, record));
  };
  records.forEach(record => visit(record.payload, record));
  return [...entries.values()].sort((a, b) => a.ref.sha256.localeCompare(b.ref.sha256));
}

export async function resolveArtifactRef(taskId: string, artifactId: string): Promise<ArtifactRef> {
  let cursor = "";
  const visited = new Set<string>();
  do {
    const page = await querySrc<ArtifactRecordPage>(taskId, "src.record.list", { kind: "", limit: 1000, cursor });
    const items = indexedArtifacts(page.records || []);
    const entry = items.find(({ ref, sources }) =>
      ref.sha256 === artifactId
      || ref.artifact_id === artifactId
      || (sources || []).some(src => src.artifact_id === artifactId),
    );
    if (entry) return entry.ref;
    const next = page.next_cursor || "";
    if (next && visited.has(next)) throw new Error("Artifact 索引游标重复，无法完成查找");
    if (next) visited.add(next);
    cursor = next;
  } while (cursor);
  throw new Error(`当前 Task 的索引中找不到 Artifact ${artifactId}，不能读取未经定位的字节`);
}
