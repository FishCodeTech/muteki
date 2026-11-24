import type { StudioState, studio } from "../store";
import { querySrc } from "../../src/reviewPrimitives";

export type StudioMutatorName = keyof typeof studio;

export type DomainEffectResult = { idMap?: Record<string, string>; createdId?: string; sourceResult?: { state: string; count: number; detail: string } };

export interface DomainCtx {
  projectId: string;
  taskId: string;
  threadId: string;
  actor: string;
  state: () => StudioState;
  proposal?: () => StudioState;
}

export type DomainRecordRow = Record<string, unknown> & {
  _revision: number;
  _record_id?: string;
  _created_at?: string;
  _updated_at?: string;
};

export async function recordListAll(
  taskId: string,
  kind: string,
  limit = 500,
): Promise<DomainRecordRow[]> {
  const rows: DomainRecordRow[] = [];
  let cursor: string | null = null;
  for (;;) {
    const page: {
      records?: Array<{
        payload: Record<string, unknown>;
        revision: number;
        record_id?: string;
        created_at?: string;
        updated_at?: string;
      }>;
      next_cursor?: string | null;
    } = await querySrc(taskId, "src.record.list", {
      kind,
      limit,
      ...(cursor ? { cursor } : {}),
    });
    for (const r of page.records ?? []) {
      rows.push({
        ...(r.payload ?? {}),
        _revision: Number.isFinite(Number(r.revision)) ? Number(r.revision) : 1,
        ...(r.record_id ? { _record_id: r.record_id } : {}),
        ...(typeof r.created_at === "string" && r.created_at ? { _created_at: r.created_at } : {}),
        ...(typeof r.updated_at === "string" && r.updated_at ? { _updated_at: r.updated_at } : {}),
      });
    }
    cursor = typeof page.next_cursor === "string" && page.next_cursor.length > 0 ? page.next_cursor : null;
    if (!cursor) break;
  }
  return rows;
}

export interface RealDomain<K extends keyof StudioState = keyof StudioState> {
  id: string;
  owns: K[];
  combines?: Partial<Record<K, (local: StudioState[K], server: StudioState[K]) => StudioState[K]>>;
  load(taskId: string, ctx: DomainCtx): Promise<Partial<Pick<StudioState, K>>>;
  pollMs?: number;
  mutators: Partial<Record<StudioMutatorName, (ctx: DomainCtx, ...args: unknown[]) => Promise<void | DomainEffectResult>>>;
}
