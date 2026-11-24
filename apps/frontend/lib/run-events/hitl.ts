import {
  hitlDeliveryState,
  type ControlCommandStatus,
  type HitlHistoryEntry,
  type HitlNeedKind,
  type HitlRequest,
} from "./types";

export const HITL_HISTORY_CAP = 200;

export type HitlBucket = "pending" | "recovering" | "auto";

export interface HitlTopic {
  key: string;
  label?: string;
  detail?: string;
}

export interface HitlRequestGroup {
  key: string;
  needKind: HitlNeedKind;
  topic: HitlTopic;
  items: HitlRequest[];
  workers: string[];
  latestTs: number;
}

export interface HitlDeckPartition {
  pending: HitlRequest[];
  recovering: HitlRequest[];
  auto: HitlRequest[];
  history: HitlHistoryEntry[];
  pendingGroups: HitlRequestGroup[];
  recoveringGroups: HitlRequestGroup[];
  pendingCount: number;
  recoveringCount: number;
  recoveringNeedsAttentionCount: number;
  historyCount: number;
}

const URL_RE = /https?:\/\/[^\s<>"'`()[\]{}，。；：、！？）】》]+/gi;
const TRAILING_PUNCT_RE = /[.,;:!?)\]}>`。，；：、）】》]+$/;
const EDGE_PUNCT_RE = /^[\s.,;:!?()[\]{}"'`。，；：、！？（）【】《》]+|[\s.,;:!?()[\]{}"'`。，；：、！？（）【】《》]+$/g;
const RECOVERY_ATTENTION_PHASES = new Set(["partial", "failed", "unknown", "rejected"]);

export function classifyHitlRequest(req: HitlRequest): HitlBucket {
  if (req.pausesBehavior === false) return "auto";
  return hitlDeliveryState(req).phase === "open" ? "pending" : "recovering";
}

export function isHitlRecoveryAttention(req: HitlRequest): boolean {
  if (req.pausesBehavior === false) return false;
  return RECOVERY_ATTENTION_PHASES.has(hitlDeliveryState(req).phase);
}

export function extractHitlUrls(text: string): string[] {
  if (!text) return [];
  const seen = new Set<string>();
  const out: string[] = [];
  for (const match of text.matchAll(URL_RE)) {
    const cleaned = match[0].replace(TRAILING_PUNCT_RE, "");
    if (!cleaned) continue;
    try {
      const parsed = new URL(cleaned);
      if (parsed.protocol !== "http:" && parsed.protocol !== "https:") continue;
      const path = parsed.pathname.replace(/\/+$/, "") || "/";
      const normalized = `${parsed.origin}${path}`;
      if (seen.has(normalized)) continue;
      seen.add(normalized);
      out.push(normalized);
    } catch {
    }
  }
  return out;
}

function urlSpecificity(url: string): { depth: number; pathLen: number } {
  try {
    const parsed = new URL(url);
    const segments = parsed.pathname.split("/").filter(Boolean);
    return { depth: segments.length, pathLen: parsed.pathname.length };
  } catch {
    return { depth: 0, pathLen: 0 };
  }
}

export function selectPrimaryHitlUrl(urls: string[]): string | undefined {
  let best: string | undefined;
  let bestScore = { depth: -1, pathLen: -1 };
  for (const url of urls) {
    const score = urlSpecificity(url);
    if (
      score.depth > bestScore.depth
      || (score.depth === bestScore.depth && score.pathLen >= bestScore.pathLen)
    ) {
      best = url;
      bestScore = score;
    }
  }
  return best;
}

export function hitlDisplayTopic(req: HitlRequest): HitlTopic {
  const kind: HitlNeedKind = req.needKind ?? "external_blocker";
  const sourceText = req.prompt.trim() ? req.prompt : (req.promptZh || "");
  const primaryUrl = selectPrimaryHitlUrl(extractHitlUrls(sourceText));
  let urlLabel: string | undefined;
  if (primaryUrl) {
    urlLabel = primaryUrl;
    try {
      const parsed = new URL(primaryUrl);
      urlLabel = parsed.pathname !== "/" ? parsed.pathname : parsed.host;
    } catch {
    }
  }
  const route = req.routeHash?.trim();
  if (route) {
    return {
      key: `${kind}|route:${route}`,
      label: urlLabel || route,
      detail: primaryUrl,
    };
  }
  if (primaryUrl) {
    return {
      key: `${kind}|url:${primaryUrl}`,
      label: urlLabel,
      detail: primaryUrl,
    };
  }
  const normalized = sourceText
    .replace(/\s+/g, " ")
    .trim()
    .replace(EDGE_PUNCT_RE, "")
    .toLowerCase();
  return {
    key: normalized ? `${kind}|text:${normalized}` : `${kind}|id:${req.id}`,
  };
}

export function groupHitlRequests(requests: HitlRequest[]): HitlRequestGroup[] {
  const groups: HitlRequestGroup[] = [];
  const byKey = new Map<string, HitlRequestGroup>();
  for (const req of requests) {
    const topic = hitlDisplayTopic(req);
    const existing = byKey.get(topic.key);
    if (existing) {
      existing.items.push(req);
      if (req.worker && !existing.workers.includes(req.worker)) {
        existing.workers.push(req.worker);
      }
      if (req.ts > existing.latestTs) existing.latestTs = req.ts;
    } else {
      const group: HitlRequestGroup = {
        key: topic.key,
        needKind: req.needKind ?? "external_blocker",
        topic,
        items: [req],
        workers: req.worker ? [req.worker] : [],
        latestTs: req.ts,
      };
      byKey.set(topic.key, group);
      groups.push(group);
    }
  }
  return groups;
}

export function partitionHitlDeck(
  hitlRequests: HitlRequest[],
  hitlHistory: HitlHistoryEntry[] = [],
): HitlDeckPartition {
  const pending: HitlRequest[] = [];
  const recovering: HitlRequest[] = [];
  const auto: HitlRequest[] = [];
  let recoveringNeedsAttentionCount = 0;
  for (const req of hitlRequests) {
    const bucket = classifyHitlRequest(req);
    if (bucket === "auto") {
      auto.push(req);
    } else if (bucket === "pending") {
      pending.push(req);
    } else {
      recovering.push(req);
      if (isHitlRecoveryAttention(req)) recoveringNeedsAttentionCount += 1;
    }
  }
  return {
    pending,
    recovering,
    auto,
    history: hitlHistory,
    pendingGroups: groupHitlRequests(pending),
    recoveringGroups: groupHitlRequests(recovering),
    pendingCount: pending.length,
    recoveringCount: recovering.length,
    recoveringNeedsAttentionCount,
    historyCount: hitlHistory.length,
  };
}

export function archiveHitlRequest(
  history: HitlHistoryEntry[],
  req: Pick<
    HitlRequest,
    | "id"
    | "prompt"
    | "promptZh"
    | "worker"
    | "intentId"
    | "routeHash"
    | "needKind"
    | "pausesBehavior"
    | "deliveryCommandId"
    | "deliveryStatus"
    | "deliveryDetail"
    | "ts"
  >,
  meta: {
    resolvedTs: number;
    resolutionAction: string;
    deliveryCommandId?: string;
    deliveryStatus?: ControlCommandStatus;
    deliveryDetail?: string;
    deliveryMode?: string;
    continuationIntentId?: string;
  },
): HitlHistoryEntry[] {
  const existing = history.find((item) => item.id === req.id);
  const entry: HitlHistoryEntry = {
    id: req.id,
    prompt: req.prompt || existing?.prompt || "",
    promptZh: req.promptZh || existing?.promptZh,
    worker: req.worker || existing?.worker,
    intentId: req.intentId || existing?.intentId,
    routeHash: req.routeHash || existing?.routeHash,
    needKind: req.needKind || existing?.needKind,
    pausesBehavior: req.pausesBehavior ?? existing?.pausesBehavior,
    ts: req.ts || existing?.ts || meta.resolvedTs,
    resolvedTs: meta.resolvedTs || existing?.resolvedTs || req.ts,
    resolutionAction: meta.resolutionAction || existing?.resolutionAction || "answer_decision",
    deliveryCommandId: meta.deliveryCommandId || req.deliveryCommandId || existing?.deliveryCommandId,
    deliveryStatus: meta.deliveryStatus || req.deliveryStatus || existing?.deliveryStatus,
    deliveryDetail: meta.deliveryDetail || req.deliveryDetail || existing?.deliveryDetail,
    deliveryMode: meta.deliveryMode || existing?.deliveryMode,
    continuationIntentId: meta.continuationIntentId || existing?.continuationIntentId,
  };
  const next = existing
    ? history.map((item) => (item.id === req.id ? entry : item))
    : [...history, entry];
  return next.slice(-HITL_HISTORY_CAP);
}
