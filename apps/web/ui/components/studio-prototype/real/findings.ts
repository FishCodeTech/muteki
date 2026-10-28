/**
 * The real findings domain: binds the 发现 workbench (clues = real finding
 * claim groups, evidence = real http_exchange/image/artifact records,
 * finding merges + verdicts + saved filters + history) to the SRC backend.
 * Server records are authoritative; see `domain.ts` for the seam contract.
 */
import type { FindingClaimItem, StudioClue, StudioEvidence, StudioRef } from "../model";
import type {
  ClueCategory, ClueStatus, FindingSeverity, ReadinessLevel,
} from "../model";
import type {
  HistoryEntry, MergeRecord, RecordKind, SavedFilter, UndeterminedDuplicate,
} from "../records";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import type { DomainCtx, RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";

const CLAIM_KIND: Record<FindingClaimItem["kind"], string> = {
  observed: "observed", inferred: "supported_inference", unverified: "unverified",
};
const VALID_CLUE_STATUS: ClueStatus[] = ["unverified", "verified", "doubtful"];
function validStatus(v: string): ClueStatus | null { return VALID_CLUE_STATUS.includes(v as ClueStatus) ? (v as ClueStatus) : null; }

const CLAIM_STATUS: Record<string, FindingClaimItem["kind"]> = {
  observed: "observed", supported_inference: "inferred", unverified: "unverified",
};

async function recordList(ctx: DomainCtx, kind: string, limit = 500) {
  const page = await querySrc<{ records?: Array<{ payload: Api; revision: number }> }>(ctx.taskId, "src.record.list", { kind, limit });
  return (page.records ?? []).map((r) => r.payload);
}

function latestClaims(claims: Api[]): Map<string, Api[]> {
  const byFinding = new Map<string, Map<string, Api>>();
  for (const claim of claims) {
    const findingId = text(claim.finding_id);
    const claimId = text(claim.claim_id);
    const bucket = byFinding.get(findingId) ?? new Map<string, Api>();
    const prior = bucket.get(claimId);
    if (!prior || num(claim.version) > num(prior.version)) bucket.set(claimId, claim);
    byFinding.set(findingId, bucket);
  }
  return new Map([...byFinding.entries()].map(([k, v]) => [k, [...v.values()]]));
}

function claimItem(claim: Api): FindingClaimItem {
  return {
    id: text(claim.claim_id),
    kind: CLAIM_STATUS[text(claim.status)] ?? "unverified",
    text: text(claim.text),
    evidenceIds: [...((claim.exchange_refs as string[]) ?? []), ...((claim.evidence_refs as Api[]) ?? []).map((r) => text(r.artifact_id) || text(r.sha256))].filter(Boolean),
  };
}

function decodeB64Envelope(raw: unknown): string {
  const env = (Array.isArray(raw) ? raw[0] : raw) as Api | undefined;
  if (env && env.encoding === "base64" && typeof env.data === "string") {
    try { return new TextDecoder("utf-8", { fatal: false }).decode(Uint8Array.from(atob(env.data), (c) => c.charCodeAt(0))); } catch { return ""; }
  }
  return "";
}

function exchangeToEvidence(row: Api, linkMap: Map<string, string[]>, projectId: string, raw: { request: string; response: string }): StudioEvidence {
  const meta = (row.metadata ?? {}) as Api;
  const method = text(row.method) || text(meta.method);
  const target = text(row.target) || text(row.url) || text(meta.target);
  const reqRef = (row.request_raw_ref ?? {}) as Api;
  const state = text(row.capture_state) || "complete";
  return {
    id: text(row.exchange_id) || text(row.record_id),
    projectId,
    title: `${method || "HTTP"} ${target || text(row.exchange_id)}`.trim() || text(row.exchange_id),
    kind: "http_exchange",
    sourceKind: text(row.source_kind) === "connector" ? "connector" : "http_tool",
    sourceRunId: text(row.source_run_id) || undefined,
    actorRef: text(row.actor_ref) || text(row.captured_by) || "SRC 交换记录",
    capturedAt: iso(row.captured_at) || iso(row.observed_at) || iso(row.created_at),
    version: String(num(row.version) || num(row.revision) || 1),
    sha256: text(reqRef.sha256),
    digestVerified: reqRef.sha256 ? "unchecked" : "unchecked",
    size: num(reqRef.size) || num(((row.response_raw_ref ?? {}) as Api).size),
    mediaType: "message/http",
    completeness: state === "complete" ? "complete" : state === "truncated" || state === "partial" ? "partial" : "unavailable",
    unreadableReason: state === "complete" ? undefined : `服务端捕获状态：${state}`,
    linkedSourceIds: [],
    linkedClueIds: linkMap.get(text(row.exchange_id)) ?? [],
    method,
    observedUrl: target,
    statusCode: num(meta.status_code) || num(row.status_code) || undefined,
    protocol: text(meta.protocol) || undefined,
    requestHeaders: ((meta.request_headers ?? row.request_headers ?? []) as Array<[string, string]>),
    responseHeaders: ((meta.response_headers ?? row.response_headers ?? []) as Array<[string, string]>),
    requestRaw: raw.request,
    responseRaw: raw.response,
  };
}

function historyEntries(items: Api[]): HistoryEntry[] {
  return items.filter((row) => text(row.domain) === "findings").map((row): HistoryEntry => ({
    id: text(row.entry_id),
    at: iso(row.at),
    action: (text(row.action) || "status") as HistoryEntry["action"],
    summary: text(row.summary),
    scope: text(row.scope) || undefined,
    refs: (row.refs as StudioRef[]) ?? [],
    serverId: text(row.entry_id),
    serverDomain: "findings",
    serverUndo: (row.undo as HistoryEntry["serverUndo"]) ?? undefined,
    revertedBy: row.reverted_at ? "server" : undefined,
  }));
}

async function recordHistory(ctx: DomainCtx, undo: { op: string; params?: Record<string, unknown> }, fallback: { action: string; summary: string; refs?: StudioRef[]; scope?: string }) {
  const entry = ctx.state().history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, scope: entry.scope, refs: entry.refs, at: entry.at }
    : { id: `wb-${Date.now().toString(36)}`, action: fallback.action, summary: fallback.summary, scope: fallback.scope, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "findings" } });
}

const META_FIELDS = ["title", "severity", "category", "importance", "missingMaterials", "targetVersion", "reviewer", "confirmedImpact", "conditionalImpact", "alternativeExplanations", "counterClaim", "tags", "status", "quote", "impact", "linkedAssetIds", "linkedTaskIds", "linkedRunIds", "linkedReportIds", "stale", "staleReason", "excluded", "kind", "duplicatesKey", "readyNotes", "evidenceIds"] as const;

export function createFindingDomain(): RealDomain {
  const evidenceRefs = new Map<string, Api>();
  return {
    id: "findings",
    pollMs: 45000,
    owns: ["clues", "evidences"],
    combines: {
      merges: (local, server) => {
        const serverIds = new Set((server as MergeRecord[]).map((m) => m.id));
        return [...(server as MergeRecord[]), ...(local as MergeRecord[]).filter((m) => m.kind !== "clue" && !serverIds.has(m.id))];
      },
      history: (local, server) => {
        const serverIds = new Set((server as HistoryEntry[]).map((e) => e.id));
        return [...(server as HistoryEntry[]), ...(local as HistoryEntry[]).filter((e) => e.serverDomain !== "findings" && !serverIds.has(e.id))];
      },
      savedFilters: (local, server) => {
        const serverIds = new Set((server as SavedFilter[]).map((f) => f.id));
        return [...(server as SavedFilter[]), ...(local as SavedFilter[]).filter((f) => f.scope !== "findings" && !serverIds.has(f.id))];
      },
      dismissedDuplicates: (local, server) => {
        const serverKeys = new Set(server as string[]);
        return [...(server as string[]), ...(local as string[]).filter((k) => !k.startsWith("clue:") && !serverKeys.has(k))];
      },
      undeterminedDuplicates: (local, server) => {
        const serverKeys = new Set(Object.keys(server as Record<string, UndeterminedDuplicate>));
        const keep = Object.fromEntries(Object.entries(local as Record<string, UndeterminedDuplicate>).filter(([k]) => !k.startsWith("clue:") && !serverKeys.has(k)));
        return { ...(server as Record<string, UndeterminedDuplicate>), ...keep };
      },
    },
    async load(taskId, ctx) {
      const [claims, metaRows, exchangeRows, mergeRows, verdicts, histPage, savedQueries, staleRows] = await Promise.all([
        recordList(ctx, "finding_claim", 1000),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.finding.meta", {}),
        recordList(ctx, "http_exchange", 500),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.finding.merges", { kind: "clue" }),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.duplicate.verdicts", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        querySrc<{ records?: Array<{ payload: Api }> }>(taskId, "src.record.list", { kind: "saved_query", limit: 500 }),
        recordList(ctx, "finding_stale"),
      ]);
      const projectId = `task-${taskId}`;
      const meta = new Map((metaRows.items ?? []).map((m) => [text(m.finding_id), m]));
      // exchange -> findings link map (claims reference exchanges)
      const linkMap = new Map<string, string[]>();
      for (const claim of claims) {
        for (const ref of (claim.exchange_refs as string[]) ?? []) {
          linkMap.set(ref, [...(linkMap.get(ref) ?? []), text(claim.finding_id)]);
        }
      }
      // status per finding (bounded)
      const claimsByFinding = latestClaims(claims);
      const findingIds = [...claimsByFinding.keys(), ...[...meta.keys()].filter((id) => !claimsByFinding.has(id))].filter(Boolean);
      const statuses = new Map<string, Api>();
      await Promise.all(findingIds.slice(0, 100).map(async (fid) => {
        try { statuses.set(fid, await querySrc<Api>(taskId, "src.finding.status", { finding_id: fid })); } catch { /* status optional */ }
      }));
      const openStale = new Map<string, string>();
      for (const s of staleRows) {
        if (s.cleared) continue;
        const fid = text(s.finding_id);
        openStale.set(fid, [...(openStale.get(fid) ? [openStale.get(fid)] : []), text(s.reason)].filter(Boolean).join("；"));
      }
      const validCategories: ClueCategory[] = ["person", "place", "timeline", "turn", "detail"];
      const clues: StudioClue[] = findingIds.map((fid) => {
        const m = (meta.get(fid) ?? {}) as Api;
        const status = statuses.get(fid) ?? {};
        const review = (status.review ?? {}) as Api;
        const clueClaims = (claimsByFinding.get(fid) ?? []).map(claimItem);
        const first = clueClaims[0]?.text ?? "";
        const stale = Boolean(status.stale) || openStale.has(fid);
        const reviewReadiness = text(review.readiness);
        const clueStatus: ClueStatus = validStatus(text(m.status)) ?? (reviewReadiness === "GREEN" ? "verified" : status.state === "unreviewed" ? "unverified" : "doubtful");
        const cvss = (review.cvss ?? m.cvss) as Api | undefined;
        const cat = text(m.category);
        return {
          id: fid,
          projectId,
          title: text(m.title) || first || fid,
          quote: text(m.quote) || first,
          category: (validCategories.includes(cat as ClueCategory) ? cat : "detail") as ClueCategory,
          status: clueStatus,
          tags: (m.tags as string[]) ?? [],
          sourceId: text(m.sourceId) || "",
          passageId: text(m.passageId) || "",
          severity: (text(m.severity) || "medium") as FindingSeverity,
          cvssVector: text(cvss?.vector) || text(m.cvssVector) || undefined,
          cvssScore: num(cvss?.score ?? m.cvssScore) || undefined,
          severityBasis: text(cvss?.basis) || text(m.severityBasis) || undefined,
          impactSummary: text(m.impactSummary) || text(m.impact) || undefined,
          conditionalImpact: text(m.conditionalImpact) || undefined,
          affectedSourceIds: (m.linkedAssetIds as string[]) ?? [],
          claims: clueClaims,
          supportingEvidenceIds: clueClaims.flatMap((c) => c.evidenceIds),
          counterEvidence: (m.counterEvidence as string[]) ?? (text(m.counterClaim) ? [text(m.counterClaim)] : undefined),
          missingMaterials: ((m.missingMaterials ?? review.missing) as string[]) ?? undefined,
          targetVersion: text(m.targetVersion) || text(review.target_version) || undefined,
          reviewer: text(m.reviewer) || text(review.reviewer) || undefined,
          reviewedAt: iso(review.reviewed_at) || iso(review.created_at) || undefined,
          readiness: (reviewReadiness || text(m.readiness) || "RED") as ReadinessLevel,
          readinessReasons: (review.reasons as string[]) ?? undefined,
          stale,
          staleReason: openStale.get(fid) || (status.stale ? ((status.stale_reasons as string[]) ?? []).join("；") : undefined),
          sourceRunId: text(m.sourceRunId) || undefined,
        } as StudioClue;
      });
      // evidence raw fetch is bounded; artifacts that fail read are marked unavailable
      const digestState = new Map<string, Api>();
      for (const ev of (await recordList(ctx, "workbench_evidence"))) digestState.set(text(ev.evidence_id), ev);
      const evidences: StudioEvidence[] = await Promise.all(exchangeRows.slice(0, 80).map(async (row) => {
        const ev = exchangeToEvidence(row, linkMap, projectId, { request: "", response: "" });
        const reqRef = (row.request_raw_ref ?? {}) as Api;
        const respRef = (row.response_raw_ref ?? {}) as Api;
        evidenceRefs.set(ev.id, reqRef);
        const checked = digestState.get(ev.id);
        if (checked) ev.digestVerified = (text(checked.state) === "verified" ? "verified" : "mismatch");
        try {
          if (reqRef.sha256) ev.requestRaw = decodeB64Envelope(await querySrc(taskId, "src.artifact.read", { ref_data: reqRef }));
          if (respRef.sha256) ev.responseRaw = decodeB64Envelope(await querySrc(taskId, "src.artifact.read", { ref_data: respRef }));
        } catch (error) {
          ev.completeness = "unavailable";
          ev.unreadableReason = error instanceof Error ? error.message : "artifact unreadable";
        }
        return ev;
      }));
      const { dismissed, undetermined } = (() => {
        const d: string[] = []; const u: Record<string, UndeterminedDuplicate> = {};
        for (const v of verdicts.items ?? []) {
          const key = text(v.key);
          if (!key.startsWith("clue:")) continue;
          if (v.state === "dismissed") d.push(key);
          else if (v.state === "undetermined") u[key] = { key, kind: "clue", projectId, ids: (v.ids as string[]) ?? [], reason: text(v.reason), missingNote: text(v.missing_note), at: iso(v.decided_at), by: text(v.decided_by) };
        }
        return { dismissed: d, undetermined: u };
      })();
      const merges: MergeRecord[] = (mergeRows.items ?? []).filter((r) => !r.undone_at).map((r) => ({
        id: text(r.merge_id), kind: "clue", projectId,
        primaryId: text(r.primary_id), memberIds: (r.member_ids as string[]) ?? [],
        reason: text(r.reason), detected: "server", at: iso(r.merged_at),
      }));
      const savedFilters: SavedFilter[] = ((savedQueries.records ?? []).map((r) => r.payload)).filter((q) => text(q.scope) === "findings").map((q) => ({
        id: text(q.query_id) || text(q.record_id), name: text(q.name), createdAt: iso(q.created_at),
        scope: "findings", filter: q.filter ?? {},
      }) as SavedFilter);
      return {
        clues, evidences, merges, savedFilters,
        dismissedDuplicates: dismissed, undeterminedDuplicates: undetermined,
        history: historyEntries(histPage.items ?? []),
      };
    },
    mutators: {
      async addClue(ctx, ...args) {
        const [input] = args as [{ projectId: string; sourceId: string; passageId: string; quote: string; category: ClueCategory }];
        const created = await postCommand<Api>(ctx.taskId, "src.finding.claim.create", {
          text: input.quote, status: "unverified", finding_id: "",
          idempotency_key: `wb-clue-${Date.now().toString(36)}`,
        });
        const findingId = text(created.finding_id);
        await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: findingId, meta: { title: input.quote, quote: input.quote, category: input.category, linkedAssetIds: input.sourceId ? [input.sourceId] : [] } });
        const local = ctx.state().clues.find((c) => c.quote === input.quote && c.id !== findingId);
        return { idMap: local ? { [local.id]: findingId } : undefined };
      },
      async updateFindingDossier(ctx, ...args) {
        const [clueId, patch] = args as [string, Partial<StudioClue>];
        const meta: Api = {};
        for (const field of META_FIELDS) if (patch[field as keyof StudioClue] !== undefined) meta[field] = patch[field as keyof StudioClue];
        if (Object.keys(meta).length) await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: clueId, meta });
      },
      async addFindingClaim(ctx, ...args) {
        const [clueId, claim] = args as [string, Omit<FindingClaimItem, "id">];
        const evidenceIds = claim.evidenceIds ?? [];
        await postCommand(ctx.taskId, "src.finding.claim.create", {
          finding_id: clueId, text: claim.text, status: CLAIM_KIND[claim.kind] ?? "unverified",
          exchange_refs: evidenceIds,
        });
        await recordHistory(ctx, { op: "noop" }, { action: "status", summary: "新增发现论断", refs: [{ kind: "clue", id: clueId }] });
      },
      async setClueCategory(ctx, ...args) {
        const [ids, category] = args as [string | string[], ClueCategory];
        for (const id of Array.isArray(ids) ? ids : [ids]) await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: id, meta: { category } });
      },
      async setClueStatus(ctx, ...args) {
        const [ids, status] = args as [string | string[], ClueStatus];
        for (const id of Array.isArray(ids) ? ids : [ids]) await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: id, meta: { status } });
      },
      async setClueSeverity(ctx, ...args) {
        const [ids, severity] = args as [string | string[], FindingSeverity];
        for (const id of Array.isArray(ids) ? ids : [ids]) await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: id, meta: { severity } });
      },
      async setClueReadiness(ctx, ...args) {
        const [ids, readiness] = args as [string | string[], ReadinessLevel];
        for (const id of Array.isArray(ids) ? ids : [ids]) {
          await postCommand(ctx.taskId, "src.finding.review.record", {
            finding_id: id, validation_type: "manual_review", readiness,
            reviewer: ctx.actor,
          });
        }
      },
      async mergeRecords(ctx, ...args) {
        const [input] = args as [{ kind: RecordKind; primaryId: string; memberIds: string[]; reason: string }];
        if (input.kind !== "clue") return;
        await postCommand(ctx.taskId, "src.workbench.finding.merge", { kind: "clue", primary_id: input.primaryId, member_ids: input.memberIds, reason: input.reason });
        await recordHistory(ctx, { op: "unmerge_finding", params: { member_ids: input.memberIds, primary_id: input.primaryId } }, { action: "merge", summary: `归并发现到 ${input.primaryId}` });
      },
      async undoMerge(ctx, ...args) {
        const [mergeId] = args as [string];
        const merge = ctx.state().merges.find((m) => m.id === mergeId);
        if (!merge || merge.kind !== "clue") return;
        await postCommand(ctx.taskId, "src.workbench.finding.unmerge", { merge_id: mergeId });
      },
      async dismissDuplicates(ctx, ...args) {
        const [kind, ids] = args as [RecordKind, string[]];
        if (kind !== "clue") return;
        const key = `clue:${[...ids].sort().join(",")}`;
        await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key, kind: "clue", ids, state: "dismissed" });
      },
      async markDuplicatesUndetermined(ctx, ...args) {
        const [kind, ids, reason, missingNote] = args as [RecordKind, string[], string, string];
        if (kind !== "clue") return;
        const key = `clue:${[...ids].sort().join(",")}`;
        await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key, kind: "clue", ids, state: "undetermined", reason, missing_note: missingNote });
      },
      async batchEditTags(ctx, ...args) {
        const [kind, ids, mode, tags] = args as [RecordKind, string[], string, string[]];
        if (kind !== "clue") return;
        for (const id of ids) {
          const current = new Set(ctx.state().clues.find((c) => c.id === id)?.tags ?? []);
          if (mode === "add") tags.forEach((t) => current.add(t));
          if (mode === "remove") tags.forEach((t) => current.delete(t));
          const next = mode === "replace" ? tags : [...current];
          await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: id, meta: { tags: next } });
        }
      },
      async batchLinkRecords(ctx, ...args) {
        const [kind, ids, target] = args as [RecordKind, string[], StudioRef];
        if (kind !== "clue") return;
        const refKind = target.kind === "task" ? "task" : target.kind === "run" ? "run" : target.kind === "report" ? "report" : "";
        if (!refKind) return;
        await postCommand(ctx.taskId, "src.workbench.object.link", { ref_kind: refKind, ref_id: target.id, object_ids: ids, object_kind: "clue" });
      },
      async batchDeleteRecords(ctx, ...args) {
        const [kind, ids] = args as ["source" | "clue" | "task", string[]];
        if (kind !== "clue") return;
        for (const id of ids) {
          await postCommand(ctx.taskId, "src.workbench.finding.stale", { finding_id: id, reason: "工作台删除（标记失效，可撤销）" });
        }
        await recordHistory(ctx, { op: "stale_clear", params: { finding_ids: ids } }, { action: "delete", summary: `批量删除 ${ids.length} 条发现（标记失效，可撤销）`, refs: ids.map((i) => ({ kind: "clue", id: i })) });
      },
      async verifyEvidenceDigest(ctx, ...args) {
        const [evidenceId] = args as [string];
        const ref = evidenceRefs.get(evidenceId);
        if (!ref || !ref.sha256) throw new Error(`证据 ${evidenceId} 没有可校验的 artifact 摘要`);
        const result = await postCommand<Api>(ctx.taskId, "src.workbench.evidence.verify", { ref_data: ref, evidence_id: evidenceId });
        if (text(result.state) === "unreadable") throw new Error(text(result.reason) || "artifact unreadable");
      },
      async markEvidenceImageReviewed(ctx, ...args) {
        const [evidenceId, reviewer] = args as [string, string?];
        await postCommand(ctx.taskId, "src.browser.image.review", { read_id: evidenceId, verdict: "reviewed", reviewer: reviewer || ctx.actor });
      },
      async replayHttpEvidence(ctx, ...args) {
        const [evidenceId, customBody, actorRef] = args as [string, string, string?];
        // Real policy path: save a draft derived from the exchange, then execute.
        const draft = await postCommand<Api>(ctx.taskId, "src.exchange.draft.save", {
          source_exchange_id: evidenceId, base_revision: 0,
          raw_request_b64: customBody ? btoa(unescape(encodeURIComponent(customBody))) : undefined,
          editor: actorRef || ctx.actor, idempotency_key: `wb-replay-${evidenceId}-${Date.now().toString(36)}`,
        });
        const draftId = text(draft.draft_id);
        if (draftId) {
          await postCommand(ctx.taskId, "src.exchange.draft.execute", {
            draft_id: draftId, draft_revision: num(draft.revision), identity_ref: ctx.actor,
            authorization_version: 0, idempotency_key: `wb-exec-${draftId}`, actor_ref: actorRef || ctx.actor,
          });
        }
      },
      async revertHistory(ctx, ...args) {
        const [entryId] = args as [string];
        const entry = ctx.state().history.find((e) => e.id === entryId || e.serverId === entryId);
        if (!entry || (entry.serverDomain && entry.serverDomain !== "findings")) return;
        const undo = entry.serverUndo;
        if (!undo) return;
        const params = (undo.params ?? {}) as Api;
        switch (undo.op) {
          case "stale_clear":
            for (const fid of (params.finding_ids as string[]) ?? []) await postCommand(ctx.taskId, "src.workbench.finding.stale.clear", { finding_id: fid });
            break;
          case "unmerge_finding": {
            const merge = ctx.state().merges.find((m) => m.kind === "clue" && m.primaryId === params.primary_id);
            if (merge) await postCommand(ctx.taskId, "src.workbench.finding.unmerge", { merge_id: merge.id });
            break;
          }
          case "verdict_clear":
            await postCommand(ctx.taskId, "src.workbench.duplicate.verdict", { key: params.key, kind: "clue", ids: params.ids ?? [], state: "clear" });
            break;
          default:
            break;
        }
        await postCommand(ctx.taskId, "src.workbench.history.revert", { entry_id: entry.serverId ?? entryId });
      },
    },
  };
}
