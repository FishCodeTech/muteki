import type { FindingClaimItem, StudioClue, StudioEvidence, StudioRef } from "../model";
import type {
  ClueCategory, ClueStatus, FindingSeverity, ReadinessLevel,
} from "../model";
import type {
  HistoryEntry, MergeRecord, RecordKind, SavedFilter, UndeterminedDuplicate,
} from "../records";
import { postCommand, querySrc } from "../../src/reviewPrimitives";
import { recordListAll, type DomainCtx, type DomainRecordRow, type RealDomain } from "./domain";

type Api = Record<string, unknown>;
const text = (v: unknown) => (typeof v === "string" ? v : "");
const num = (v: unknown, d = 0) => (Number.isFinite(Number(v)) ? Number(v) : d);
const iso = (v: unknown) => text(v) || "";
const readableArtifactRef = (ref: Api) => Boolean(text(ref.artifact_id));

const CLAIM_KIND: Record<FindingClaimItem["kind"], string> = {
  observed: "observed", inferred: "supported_inference", unverified: "unverified",
};
const VALID_CLUE_STATUS: ClueStatus[] = ["unverified", "verified", "doubtful"];
function validStatus(v: string): ClueStatus | null { return VALID_CLUE_STATUS.includes(v as ClueStatus) ? (v as ClueStatus) : null; }

const CLAIM_STATUS: Record<string, FindingClaimItem["kind"]> = {
  observed: "observed", supported_inference: "inferred", unverified: "unverified",
};

function decodeB64Envelope(raw: unknown): string {
  const env = (Array.isArray(raw) ? raw[0] : raw) as Api | undefined;
  if (env && env.encoding === "base64" && typeof env.data === "string") {
    try { return new TextDecoder("utf-8", { fatal: false }).decode(Uint8Array.from(atob(env.data), (c) => c.charCodeAt(0))); } catch { return ""; }
  }
  return "";
}

function encodeUtf8Base64(value: string): string {
  const bytes = new TextEncoder().encode(value);
  let binary = "";
  for (let i = 0; i < bytes.length; i += 1) binary += String.fromCharCode(bytes[i]);
  return btoa(binary);
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
    evidenceIds: [...((claim.exchange_refs as string[]) ?? []), ...((claim.evidence_refs as Api[]) ?? []).map((r) => text(r.artifact_id))].filter(Boolean),
  };
}

function exchangeToEvidence(row: Api, linkMap: Map<string, string[]>, projectId: string): StudioEvidence {
  const meta = (row.metadata ?? {}) as Api;
  const method = text(row.method) || text(meta.method);
  const target = text(row.target) || text(row.observed_url) || text(row.url) || text(meta.target);
  const reqRef = (row.request_raw_ref ?? {}) as Api;
  const respRef = (row.response_raw_ref ?? {}) as Api;
  const state = text(row.capture_state) || "complete";
  const sk = text(row.source_kind);
  const sourceKind: StudioEvidence["sourceKind"] =
    sk === "browser" ? "browser" : sk === "proxy" || sk === "proxy_bridge" ? "proxy" : sk === "import" ? "import" : sk === "connector" ? "connector" : "http_tool";
  return {
    id: text(row.exchange_id) || text(row.record_id),
    projectId,
    title: `${method || "HTTP"} ${target || text(row.exchange_id)}`.trim() || text(row.exchange_id),
    kind: "http_exchange",
    sourceKind,
    sourceRunId: text(row.source_run_id) || undefined,
    actorRef: text(row.actor_ref) || text(row.context_id) || text(row.captured_by) || "SRC 交换记录",
    capturedAt: iso(row.captured_at) || iso(row.observed_at) || iso(row.started_at) || iso(row.created_at) || iso(row._created_at),
    version: String(num(row._revision) || num(row.version) || num(row.revision) || 1),
    artifactId: text(reqRef.artifact_id) || text(respRef.artifact_id) || undefined,
    digestVerified: "unchecked",
    size: num(reqRef.size) || num(respRef.size),
    mediaType: "message/http",
    completeness: state === "complete" ? "complete" : state === "truncated" || state === "partial" ? "partial" : "unavailable",
    unreadableReason: state === "complete" ? undefined : `服务端捕获状态：${state}`,
    linkedSourceIds: [],
    linkedClueIds: linkMap.get(text(row.exchange_id)) ?? [],
    method,
    observedUrl: target,
    statusCode: num(meta.status_code) || num(row.status_code) || undefined,
    protocol: text(meta.protocol) || text(row.protocol) || undefined,
    requestHeaders: ((meta.request_headers ?? row.request_headers ?? []) as Array<[string, string]>),
    responseHeaders: ((meta.response_headers ?? row.response_headers ?? []) as Array<[string, string]>),
    requestArtifact: readableArtifactRef(reqRef) ? reqRef : undefined,
    responseArtifact: readableArtifactRef(respRef) ? respRef : undefined,
    baselineExchangeId: text(row.replay_of) || undefined,
  };
}

function screenshotToEvidence(row: Api, projectId: string): StudioEvidence {
  const artifact = (row.artifact ?? {}) as Api;
  const lastReview = (row.last_image_review ?? {}) as Api;
  const sid = text(row.screenshot_id) || text(row.record_id);
  const w = num(row.width);
  const h = num(row.height);
  return {
    id: sid,
    artifactId: text(artifact.artifact_id) || undefined,
    projectId,
    title: `截图 ${text(row.url) || sid}`,
    kind: "screenshot",
    sourceKind: "browser",
    actorRef: text(row.context_id) || "default",
    capturedAt: iso(row.captured_at) || iso(row.created_at) || iso(row._created_at),
    version: String(num(row.version) || num(row._revision) || 1),
    digestVerified: readableArtifactRef(artifact) ? "unchecked" : "mismatch",
    size: num(artifact.size),
    mediaType: text(artifact.media_type) || "image/png",
    completeness: readableArtifactRef(artifact) ? "complete" : "unavailable",
    linkedSourceIds: [],
    linkedClueIds: [],
    captureUrl: text(row.url) || undefined,
    dimensions: w && h ? `${w}×${h}` : undefined,
    browserGeneration: num(row.browser_generation) || undefined,
    imageReviewedBy: text(row.review_status) === "reviewed" ? (text(lastReview.reviewer) || "reviewed") : undefined,
    imageReviewedAt: text(row.review_status) === "reviewed" ? (iso(lastReview.reviewed_at) || undefined) : undefined,
  };
}

export async function readBrowserScreenshotImage(
  taskId: string,
  screenshotId: string,
  reviewer: string,
): Promise<{ dataUrl: string; readId: string; width: number; height: number; artifactId: string }> {
  const res = await querySrc<Api>(taskId, "src.browser.image.read", {
    screenshot_id: screenshotId,
    reviewer: reviewer || "local-user",
    confirm_delivery: true,
  });
  const imageBytes = (res.image_bytes ?? {}) as Api;
  const b64 = text(imageBytes.data);
  const mediaType = text(res.media_type) || "image/png";
  const receipt = (res.read_receipt ?? {}) as Api;
  return {
    dataUrl: b64 ? `data:${mediaType};base64,${b64}` : "",
    readId: text(receipt.read_id),
    width: num(res.width),
    height: num(res.height),
    artifactId: text(res.artifact_id),
  };
}

function buildReplayedRawRequest(originalRaw: string, customInput: string, observedUrl: string): string {
  const trimmed = customInput.trim();
  if (/^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+\S+\s+HTTP\/1\.[01]/i.test(trimmed)) {
    const parts = trimmed.split(/\r?\n\r?\n/);
    const head = parts[0].replace(/\r?\n/g, "\r\n");
    const body = parts.slice(1).join("\r\n\r\n");
    const hasCl = /^Content-Length:/im.test(head);
    const bodyBytes = new TextEncoder().encode(body).length;
    const nextHead = hasCl
      ? head.replace(/^Content-Length:.*$/im, `Content-Length: ${bodyBytes}`)
      : bodyBytes > 0 ? `${head}\r\nContent-Length: ${bodyBytes}` : head;
    return `${nextHead}\r\n\r\n${body}`;
  }
  const origParts = originalRaw.split(/\r?\n\r?\n/);
  let head = origParts[0];
  if (!head || !/HTTP\/1\.[01]/i.test(head.split(/\r?\n/)[0] ?? "")) {
    let host = "localhost";
    let path = "/";
    try {
      const parsed = new URL(observedUrl || "http://localhost/");
      host = parsed.host;
      path = `${parsed.pathname}${parsed.search}`;
    } catch {  }
    head = `${trimmed ? "POST" : "GET"} ${path} HTTP/1.1\r\nHost: ${host}\r\nContent-Type: application/json`;
  } else {
    head = head.replace(/\r?\n/g, "\r\n");
  }
  const body = customInput;
  const bodyBytes = new TextEncoder().encode(body).length;
  if (/^Content-Length:/im.test(head)) {
    head = head.replace(/^Content-Length:.*$/im, `Content-Length: ${bodyBytes}`);
  } else if (bodyBytes > 0 || /^(POST|PUT|PATCH)/i.test(head)) {
    head = `${head}\r\nContent-Length: ${bodyBytes}`;
  }
  return `${head}\r\n\r\n${body}`;
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
  const entry = (ctx.proposal?.() ?? ctx.state()).history[0];
  const payload = entry && entry.summary === fallback.summary
    ? { id: entry.id, action: entry.action, summary: entry.summary, scope: entry.scope, refs: entry.refs, at: entry.at }
    : { id: `wb-${crypto.randomUUID().slice(0, 8)}`, action: fallback.action, summary: fallback.summary, scope: fallback.scope, refs: fallback.refs ?? [], at: new Date().toISOString() };
  await postCommand(ctx.taskId, "src.workbench.history.record", { entry: { ...payload, undo, domain: "findings" } });
}

const META_FIELDS = ["title", "severity", "category", "importance", "missingMaterials", "targetVersion", "reviewer", "reviewedAt", "confirmedImpact", "conditionalImpact", "impactSummary", "alternativeExplanations", "counterClaim", "tags", "status", "quote", "impact", "linkedAssetIds", "linkedTaskIds", "linkedRunIds", "linkedReportIds", "stale", "staleReason", "excluded", "kind", "duplicatesKey", "readyNotes", "evidenceIds", "readiness"] as const;

const EVIDENCE_PAGE_SIZE = 100;

export interface EvidencePaginationState {
  pageLimit: number;
  loaded: number;
  total: number;
  hasMore: boolean;
  loadingMore: boolean;
}

const evidencePaginationByTask = new Map<string, EvidencePaginationState>();

export function getEvidencePagination(taskId: string): EvidencePaginationState | null {
  return taskId ? (evidencePaginationByTask.get(taskId) ?? null) : null;
}

export async function loadMoreFindingEvidences(taskId: string, reload: () => Promise<void>): Promise<void> {
  if (!taskId) return;
  const prev = evidencePaginationByTask.get(taskId) ?? {
    pageLimit: 1,
    loaded: 0,
    total: 0,
    hasMore: false,
    loadingMore: false,
  };
  evidencePaginationByTask.set(taskId, { ...prev, pageLimit: prev.pageLimit + 1, loadingMore: true });
  try {
    await reload();
  } catch (err) {
    evidencePaginationByTask.set(taskId, { ...prev, loadingMore: false });
    throw err;
  }
}

async function listExchangePages(
  taskId: string,
  kind: string,
  maxPages: number,
  pageSize = EVIDENCE_PAGE_SIZE,
): Promise<{ rows: DomainRecordRow[]; total: number; hasMore: boolean }> {
  const rows: DomainRecordRow[] = [];
  let cursor: string | null = null;
  let total = 0;
  let hasMore = false;
  for (let pageIndex = 0; pageIndex < maxPages; pageIndex += 1) {
    const page: {
      records?: Array<{
        payload: Record<string, unknown>;
        revision: number;
        record_id?: string;
        created_at?: string;
        updated_at?: string;
      }>;
      total?: number;
      next_cursor?: string | null;
      complete?: boolean;
    } = await querySrc(taskId, "src.record.list", {
      kind,
      limit: pageSize,
      ...(cursor ? { cursor } : {}),
    });
    if (Number.isFinite(Number(page.total))) total = Number(page.total);
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
    if (!cursor) {
      hasMore = false;
      break;
    }
    if (pageIndex === maxPages - 1) {
      hasMore = true;
    }
  }
  return { rows, total: Math.max(total, rows.length), hasMore };
}

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
      const pageLimit = Math.max(1, evidencePaginationByTask.get(taskId)?.pageLimit ?? 1);
      const [claims, metaRows, httpPage, importPage, screenshotRows, mergeRows, verdicts, histPage, savedQueries, staleRows, digestRows] = await Promise.all([
        recordListAll(taskId, "finding_claim"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.finding.meta", {}),
        listExchangePages(taskId, "http_exchange", pageLimit),
        listExchangePages(taskId, "import_exchange", pageLimit),
        recordListAll(taskId, "browser_screenshot"),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.finding.merges", { kind: "clue" }),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.duplicate.verdicts", {}),
        querySrc<{ items?: Api[] }>(taskId, "src.workbench.history", { limit: 1000 }),
        recordListAll(taskId, "saved_query"),
        recordListAll(taskId, "finding_stale"),
        recordListAll(taskId, "workbench_evidence"),
      ]);
      const seenExchangeIds = new Set<string>();
      const exchangeRows: DomainRecordRow[] = [];
      for (const row of [...httpPage.rows, ...importPage.rows]) {
        const key = text(row.exchange_id) || text(row._record_id);
        if (key && seenExchangeIds.has(key)) continue;
        if (key) seenExchangeIds.add(key);
        exchangeRows.push(row);
      }
      evidencePaginationByTask.set(taskId, {
        pageLimit,
        loaded: exchangeRows.length,
        total: httpPage.total + importPage.total,
        hasMore: httpPage.hasMore || importPage.hasMore,
        loadingMore: false,
      });
      const projectId = ctx.projectId;
      const meta = new Map((metaRows.items ?? []).map((m) => [text(m.finding_id), m]));
      const linkMap = new Map<string, string[]>();
      for (const claim of claims) {
        for (const ref of (claim.exchange_refs as string[]) ?? []) {
          linkMap.set(ref, [...(linkMap.get(ref) ?? []), text(claim.finding_id)]);
        }
      }
      const claimsByFinding = latestClaims(claims);
      const findingIds = [...claimsByFinding.keys(), ...[...meta.keys()].filter((id) => !claimsByFinding.has(id))].filter(Boolean);
      const statuses = new Map<string, Api>();
      await Promise.all(findingIds.map(async (fid) => {
        statuses.set(fid, await querySrc<Api>(taskId, "src.finding.status", { finding_id: fid }));
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
        const draftMissing = (claimsByFinding.get(fid) ?? []).some((c) => text(c.draft_state) === "needs_supplement");
        const readiness = (reviewReadiness || text(m.readiness) || (draftMissing ? "DRAFT" : "RED")) as ReadinessLevel;
        const clueStatus: ClueStatus = validStatus(text(m.status)) ?? (readiness === "GREEN" ? "verified" : status.state === "unreviewed" ? "unverified" : "doubtful");
        const cvss = (review.cvss ?? m.cvss) as Api | undefined;
        const cat = text(m.category);
        const rawMissing = Array.isArray(m.missingMaterials)
          ? m.missingMaterials
          : Array.isArray(review.missing)
            ? review.missing
            : undefined;
        const missingMaterials = rawMissing
          ?.map((item) => {
            if (typeof item === "string") return item;
            if (item && typeof item === "object") {
              const obj = item as Api;
              return text(obj.action) || text(obj.message) || text(obj.title) || text(obj.code);
            }
            return text(item);
          })
          .filter(Boolean);
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
          severity: (text(m.severity) || undefined) as FindingSeverity | undefined,
          cvssVector: text(cvss?.vector) || text(m.cvssVector) || undefined,
          cvssScore: num(cvss?.base_score ?? cvss?.score ?? m.cvssScore) || undefined,
          severityBasis: text(cvss?.basis) || text(m.severityBasis) || undefined,
          impactSummary: text(m.impactSummary) || text(m.impact) || undefined,
          conditionalImpact: text(m.conditionalImpact) || undefined,
          affectedSourceIds: (m.linkedAssetIds as string[]) ?? [],
          claims: clueClaims,
          supportingEvidenceIds: clueClaims.flatMap((c) => c.evidenceIds),
          counterEvidence: (m.counterEvidence as string[]) ?? (text(m.counterClaim) ? [text(m.counterClaim)] : undefined),
          missingMaterials,
          targetVersion: text(m.targetVersion) || text(review.target_version) || undefined,
          reviewer: text(m.reviewer) || text(review.reviewer) || undefined,
          reviewedAt: iso(review.reviewed_at) || iso(review.created_at) || iso(m.reviewedAt) || undefined,
          readiness,
          readinessReasons: (review.reasons as string[]) ?? undefined,
          stale,
          staleReason: openStale.get(fid) || (status.stale ? ((status.stale_reasons as string[]) ?? []).join("；") : undefined),
          sourceRunId: text(m.sourceRunId) || undefined,
        } as StudioClue;
      });
      const digestState = new Map<string, Api>();
      for (const ev of digestRows) digestState.set(text(ev.evidence_id), ev);
      const httpEvidences = exchangeRows
        .sort((a, b) =>
          iso(b.captured_at || b.observed_at || b.started_at || b.created_at || b._created_at)
            .localeCompare(iso(a.captured_at || a.observed_at || a.started_at || a.created_at || a._created_at)),
        )
        .map((row) => {
          const ev = exchangeToEvidence(row, linkMap, projectId);
          const ref = ev.requestArtifact ?? ev.responseArtifact;
          if (ref) evidenceRefs.set(ev.id, ref);
          const checked = digestState.get(ev.id);
          if (checked) ev.digestVerified = text(checked.state) === "verified" ? "verified" : "mismatch";
          return ev;
        });
      const shotEvidences: StudioEvidence[] = screenshotRows.map((row) => {
        const ev = screenshotToEvidence(row, projectId);
        const artifact = (row.artifact ?? {}) as Api;
        if (readableArtifactRef(artifact)) evidenceRefs.set(ev.id, artifact);
        return ev;
      });
      const evidences = [...httpEvidences, ...shotEvidences];
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
      const savedFilters: SavedFilter[] = savedQueries.filter((q) => text(q.scope) === "findings").map((q) => ({
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
        const passage = ctx.state().sources.find((source) => source.id === input.sourceId)
          ?.passages.find((item) => item.id === input.passageId);
        let exchangeRefs: string[] = [];
        if (passage) {
          try {
            const raw = JSON.parse(passage.text) as Api;
            const exchangeId = text(raw.exchange_id);
            if (exchangeId.startsWith("sexchange-") || exchangeId.startsWith("sex_")) exchangeRefs = [exchangeId];
          } catch {  }
        }
        const created = await postCommand<Api>(ctx.taskId, "src.finding.claim.create", {
          text: input.quote, status: "unverified", finding_id: "",
          exchange_refs: exchangeRefs,
          idempotency_key: `wb-clue-${Date.now().toString(36)}`,
        });
        const findingId = text(created.finding_id);
        await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: findingId, meta: { title: input.quote, quote: input.quote, category: input.category, sourceId: input.sourceId, passageId: input.passageId, linkedAssetIds: input.sourceId ? [input.sourceId] : [] } });
        const local = ctx.proposal?.().clues.find((c) => c.quote === input.quote && c.id !== findingId);
        return { createdId: findingId, idMap: local ? { [local.id]: findingId } : undefined };
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
          idempotency_key: `wb-claim-${clueId}-${crypto.randomUUID()}`,
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
          if (readiness === "DRAFT") {
            await postCommand(ctx.taskId, "src.workbench.finding.meta.save", { finding_id: id, meta: { readiness: "DRAFT" } });
            continue;
          }
          await postCommand(ctx.taskId, "src.finding.review.record", {
            finding_id: id, validation_type: "artifact_review", readiness,
            reviewer: ctx.actor,
            idempotency_key: `wb-review-${id}-${crypto.randomUUID()}`,
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
        if (!ref || !readableArtifactRef(ref)) throw new Error(`证据 ${evidenceId} 没有可读取的工件引用`);
        const result = await querySrc<Api>(ctx.taskId, "src.workbench.evidence.verify", { ref_data: ref, evidence_id: evidenceId });
        if (text(result.state) === "unreadable") throw new Error(text(result.reason) || "artifact unreadable");
      },
      async markEvidenceImageReviewed(ctx, ...args) {
        const [evidenceId, reviewerArg, judgmentArg, readIdArg] = args as [string, string?, string?, string?];
        const reviewer = reviewerArg || ctx.actor || "local-user";
        let reviewReadId = readIdArg || "";
        if (!reviewReadId) {
          const readRes = await readBrowserScreenshotImage(ctx.taskId, evidenceId, reviewer);
          reviewReadId = readRes.readId;
        }
        const judgment = judgmentArg?.trim() || "人工已核验原图";
        await postCommand(ctx.taskId, "src.browser.image.review", {
          screenshot_id: evidenceId,
          reviewer,
          review_read_id: reviewReadId,
          judgment,
        });
      },
      async replayHttpEvidence(ctx, ...args) {
        const [evidenceId, customBodyOrRaw, actorRef] = args as [string, string, string?];
        const [exchangeRes, policy, drafts] = await Promise.all([
          querySrc<Api>(ctx.taskId, "src.exchange.read", { exchange_id: evidenceId, include_bytes: true }),
          querySrc<Api>(ctx.taskId, "src.policy.current", {}),
          recordListAll(ctx.taskId, "request_draft"),
        ]);
        const exchangeObj = (exchangeRes.exchange ?? {}) as Api;
        const origReq = decodeB64Envelope(exchangeRes.request_raw);
        const observedUrl = text(exchangeObj.observed_url) || text(exchangeObj.url);
        const matchingDrafts = drafts.filter((d) => text(d.source_exchange_id) === evidenceId);
        const baseRevision = matchingDrafts.length
          ? Math.max(...matchingDrafts.map((d) => num(d.revision, 1)))
          : num(exchangeRes.record_revision ?? exchangeObj.revision ?? exchangeRes.revision, 1);
        const rawHttp = buildReplayedRawRequest(origReq, customBodyOrRaw || "", observedUrl);
        const draft = await postCommand<Api>(ctx.taskId, "src.exchange.draft.save", {
          source_exchange_id: evidenceId,
          base_revision: baseRevision,
          raw_request_b64: encodeUtf8Base64(rawHttp),
          editor: actorRef || ctx.actor,
          idempotency_key: `wb-replay-${evidenceId}-${Date.now().toString(36)}`,
        });
        const draftRecord = (draft.draft ?? draft) as Api;
        const draftId = text(draftRecord.draft_id) || text(draft.draft_id);
        const draftRev = num(draftRecord.revision ?? draft.revision, 1);
        if (!draftId) throw new Error("未能保存重放请求草稿");
        const exec = await postCommand<Api>(ctx.taskId, "src.exchange.draft.execute", {
          draft_id: draftId,
          draft_revision: draftRev,
          identity_ref: text(exchangeObj.context_id) || ctx.actor,
          context_id: text(exchangeObj.context_id) || "default",
          authorization_version: num(policy.version, 1),
          idempotency_key: `wb-exec-${draftId}`,
          actor_ref: actorRef || ctx.actor,
          wait_for_approval: false,
        });
        const createdId = text(exec.exchange_id);
        const local = ctx.proposal?.().evidences.find((e) => e.id.startsWith("EX-R"));
        return {
          createdId: createdId || undefined,
          idMap: local && createdId ? { [local.id]: createdId } : undefined,
        };
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
