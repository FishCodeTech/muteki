/**
 * The real-domain seam. A `RealDomain` owns a set of StudioState fields on a
 * real SRC task: it loads server state into them and intercepts the studio
 * mutators it can back with real commands. Unregistered mutators keep their
 * local behaviour (Phase C-E will claim theirs the same way).
 */
import type { StudioState, studio } from "../store";

export type StudioMutatorName = keyof typeof studio;

export interface DomainCtx {
  taskId: string;
  threadId: string;
  actor: string;
  state: () => StudioState;
}

export interface RealDomain<K extends keyof StudioState = keyof StudioState> {
  id: string;
  /** StudioState fields this domain replaces from the server on every reload. */
  owns: K[];
  /** Fields where only part is server-owned; merges local + server on reload. */
  combines?: Partial<Record<K, (local: StudioState[K], server: StudioState[K]) => StudioState[K]>>;
  load(taskId: string, ctx: DomainCtx): Promise<Partial<Pick<StudioState, K>>>;
  pollMs?: number;
  /** Server-side effects run after the local reducer, serially per domain. */
  mutators: Partial<Record<StudioMutatorName, (ctx: DomainCtx, ...args: unknown[]) => Promise<void | { idMap?: Record<string, string> }>>>;
}
