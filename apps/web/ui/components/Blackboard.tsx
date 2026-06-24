"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent } from "react";
import {
  ReactFlow, ReactFlowProvider, Background, BackgroundVariant, Controls, MiniMap,
  Handle, Position,
  useNodesState, useEdgesState, type Node, type Edge, type NodeProps,
  type NodeChange, useReactFlow, useNodesInitialized,
} from "@xyflow/react";
import { BlackboardView } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { workerColor as engineColor, workerInitial, workerShortLabel } from "@/lib/workers";
import { Icon, type IconName } from "@/components/Icon";
import { CopyText } from "@/components/CopyText";
import { ChipFilterBar } from "@/components/ChipFilterBar";

/**
 * The shared knowledge blackboard — the swarm's COLLABORATION surface, rendered
 * as a FREE STICKY-NOTE CANVAS (OneNote/Miro-style) instead of fixed columns:
 *
 *   · every collaboration datum is a draggable sticky on a pan/zoom canvas:
 *     intents (open/claimed/done), provenance-gated facts (✓verified / ?candidate),
 *     dead-ends (✗ struck), and the flag (gold).
 *   · intents LINK to the facts/dead-ends they produced (light edges, toggleable).
 *   · drag a sticky anywhere; positions persist per-run in localStorage.
 *
 * Colour follows the brand verdict system: green=verified, yellow=candidate,
 * blue=claimed, dim=open, green=done, per-worker for claimants, red=dead-end,
 * gold=flag. React Flow gives us the canvas, drag, edges, pan/zoom for free.
 */

// Read a :root CSS variable at runtime so React Flow chrome (background dots,
// minimap) follows the active theme — single source of truth in globals.css.
function cssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

// worker accent is keyed by ENGINE (shared lib/workers palette) so a solver reads
// the same colour across the whole deck (graph / blackboard / inspector lanes).
function workerColor(_workers: string[], w?: string): string {
  if (!w) return "#6b7385";
  return engineColor(w);
}
function initials(w?: string): string {
  if (!w) return "·";
  return workerInitial(w);
}
function Avatar({ worker, workers, title }: { worker?: string; workers: string[]; title?: string }) {
  const c = workerColor(workers, worker);
  return (
    <span className="bb-avatar" style={{ borderColor: c, color: c }} title={title ?? worker}>
      {initials(worker)}
    </span>
  );
}

// ── the four sticky kinds (React Flow custom nodes) ──────────────────────────
// node.data carries the source datum + the workers list (for colouring).

type NodeData = { workers: string[]; [k: string]: any };

// reactflow needs handles to anchor edges; we keep them visually hidden (the
// board is about free placement, not wiring — edges are auto-drawn provenance links).
function Handles() {
  // 1px handles (measurable so React Flow can anchor edges) but visually invisible.
  const s = { width: 1, height: 1, minWidth: 1, minHeight: 1, opacity: 0, border: "none", background: "transparent" } as const;
  return (
    <>
      <Handle id="t" type="target" position={Position.Left} isConnectable={false} style={s} />
      <Handle id="s" type="source" position={Position.Right} isConnectable={false} style={s} />
    </>
  );
}

function IntentNode({ data }: NodeProps<Node<NodeData>>) {
  const t = useT();
  const d = data;
  const gist = (d.summary || "").trim();
  const hasGist = gist && gist !== d.goal;
  const childCount = Number(d.childCount || 0);
  // A/J: dispatch_state demotes an intent visually — resume (paused) and retired
  // (dropped) are dimmed so the board reads which directions are actually live.
  const ds: string = d.dispatchState || "active";
  const dimmed = ds === "resume" || ds === "retired";
  return (
    <div className={`bb-node intent ${d.status} ${dimmed ? ds : ""}`}
         style={d.status !== "open" ? ({ ["--wc" as any]: workerColor(d.workers, d.worker) }) : undefined}>
      <Handles />
      <div className="bb-node-top">
        {childCount > 0 && d.onToggle && (
          <button className="bb-collapse nodrag" title={d.collapsed ? t("bb.expand") : t("bb.collapse")}
            aria-label={d.collapsed ? t("bb.expand") : t("bb.collapse")} aria-expanded={!d.collapsed}
            onClick={(e) => { e.stopPropagation(); d.onToggle(d.id); }}>{d.collapsed ? "▸" : "▾"}</button>
        )}
        <span className="bb-iid">{d.id}</span>
        <span className={`bb-wc bb-wc-${d.workerClass}`}>{d.workerClass}</span>
        {ds === "resume" && <span className="bb-review-badge" title={d.closeReason || ""}>{t("meta.paused")}</span>}
        {ds === "retired" && <span className="bb-review-badge bad" title={d.closeReason || ""}>{t("meta.retired")}</span>}
        {/* folded summary: verified · candidate · dead breakdown so a collapsed
            group still tells you what's inside. expanded → just the total. */}
        {childCount > 0 && (d.collapsed
          ? <span className="bb-childbreak">
              {Number(d.nVerified) > 0 && <b className="v">{d.nVerified}✓</b>}
              {Number(d.nCandidate) > 0 && <b className="c">{d.nCandidate}?</b>}
              {Number(d.nDead) > 0 && <b className="d">{d.nDead}✕</b>}
            </span>
          : <span className="bb-childcount">{childCount}</span>)}
        {d.worker && <Avatar worker={d.worker} workers={d.workers} />}
      </div>
      <div className="bb-goal">{gist || d.goal}</div>
      {hasGist && (
        <details className="nodrag bb-raw-d">
          <summary className="bb-fact-more">{t("insp.raw")}</summary>
          <div className="bb-raw-t">{d.goal}</div>
        </details>
      )}
    </div>
  );
}

// fact/candidate render as a SMALL DOT (like the fact-graph), not a full-text
// card — verified = solid green, candidate = hollow, each ringed in its producing
// worker's engine colour. The full fact + provenance lives in a hover tooltip and
// (on click) the node-detail popover. This keeps a group's children compact so the
// board reads as a constellation of points, not a wall of text.
function FactNode({ data, selected }: NodeProps<Node<NodeData>>) {
  const d = data;
  const retired = d.state === "rejected" || d.state === "merged" || d.state === "superseded";
  const wc = d.actor ? engineColor(d.actor) : undefined;
  const text = (d.summary || "").trim() || d.fact || "";
  const tip = [
    d.verified ? "✓ 已验证" : `? 候选 (${Number(d.confidence).toFixed(1)})`,
    text,
    d.actor ? `— ${d.actor}` : "",
    d.witness ? `witness: ${d.witness}` : "",
  ].filter(Boolean).join("\n");
  return (
    <div
      className={`bb-dot fact ${d.verified ? "v" : "c"} ${d.challenged ? "challenged" : ""} ${retired ? "retired" : ""} ${selected ? "sel" : ""}`}
      style={wc ? ({ ["--wc" as any]: wc }) : undefined}
      title={tip}
    >
      <Handles />
    </div>
  );
}

function DeadNode({ data, selected }: NodeProps<Node<NodeData>>) {
  const d = data;
  const wc = d.actor ? engineColor(d.actor) : undefined;
  const tip = ["✕ 死路", d.reason || "", d.actor ? `— ${d.actor}` : ""].filter(Boolean).join("\n");
  return (
    <div className={`bb-dot dead ${selected ? "sel" : ""}`} style={wc ? ({ ["--wc" as any]: wc }) : undefined} title={tip}>
      <Handles />
    </div>
  );
}


// A '+N 死路' pill that stands in for a group's folded low-value dead-ends. Click
// toggles them open/closed (state lives in Canvas, not localStorage). `expanded`
// flips the label to a collapse affordance.
function DeadCapsuleNode({ data }: NodeProps<Node<NodeData>>) {
  const d = data;
  return (
    <button
      type="button"
      className="bb-dead-capsule nodrag"
      title={d.expanded ? "收起死路" : "展开折叠的死路"}
      onClick={(e) => { e.stopPropagation(); d.onToggleDead?.(d.groupId); }}
    >
      <Icon name={d.expanded ? "chevronDown" : "xCircle"} size={11} />
      {d.expanded ? "收起死路" : `+${d.count} 死路`}
    </button>
  );
}

// a dead-end is "important" (kept visible, not folded) when it carries real signal:
// multiple workers hit the same wall, the reason text reads like a hard negative,
// or it belongs to a hot intent (≥5 facts). Everything else folds into the capsule.
const NEGATIVE_DEAD_RE = /\b(not found|no such|invalid|failed|failure|error|timeout|refused|denied|permission|expired|unreachable|crash|segfault|404|403|500|cannot|can't|does not|wrong|false positive|no flag)\b/i;
function isImportantDeadEnd(reason: string, distinctActors: number, factCountForIntent: number): boolean {
  return distinctActors > 1 || NEGATIVE_DEAD_RE.test(reason || "") || factCountForIntent >= 5;
}

function FlagNode({ data }: NodeProps<Node<NodeData>>) {
  return (
    <div className="bb-node flag">
      <Handles />
      <div className="bb-node-top"><span className="bb-node-glyph"><Icon name="flag" size={13} /></span></div>
      <div className="bb-node-txt">{data.flag}</div>
    </div>
  );
}

// meta = coordinator notes (review findings / directives / pocs / routes /
// branches). Rendered as a small dot carrying the kind's icon, tinted by tone;
// full title + text live in the hover tooltip. Keeps the board a field of points.
function MetaNode({ data, selected }: NodeProps<Node<NodeData>>) {
  const d = data;
  const wc = d.actor ? engineColor(d.actor) : undefined;
  const tip = [d.kind ? `[${d.kind}]` : "", d.title || "", d.text || ""].filter(Boolean).join("\n");
  return (
    <div className={`bb-dot meta ${d.tone || ""} ${selected ? "sel" : ""}`}
      style={wc ? ({ ["--wc" as any]: wc }) : undefined} title={tip}>
      <Handles />
      <Icon name={(d.icon || "board") as IconName} size={11} />
    </div>
  );
}

// The group IS the intent now — one card (no separate intent node stacked inside).
// A compact header (id · worker-class · count · collapse) + a clamped goal line,
// then the child dots sit in the body below. The left accent bar is the intent's
// worker engine colour. Click the ▸/▾ to fold the dots.
function GroupNode({ data }: NodeProps<Node<NodeData>>) {
  const t = useT();
  const d = data;
  const ds: string = d.dispatchState || "active";
  const dimmed = ds === "resume" || ds === "retired";
  const childCount = Number(d.childCount || 0);
  return (
    <div className={`bb-group-node intent ${d.status || ""} ${dimmed ? ds : ""}`}
      style={{ ["--gc" as any]: d.accent || "#4ea1ff" }}>
      <Handles />
      <div className="bb-group-head">
        {childCount > 0 && d.onToggle && (
          <button className="bb-collapse nodrag" title={d.collapsed ? t("bb.expand") : t("bb.collapse")}
            aria-label={d.collapsed ? t("bb.expand") : t("bb.collapse")} aria-expanded={!d.collapsed}
            onClick={(e) => { e.stopPropagation(); d.onToggle(d.id); }}>{d.collapsed ? "▸" : "▾"}</button>
        )}
        <span className="bb-iid">{d.id}</span>
        {d.workerClass && <span className={`bb-wc bb-wc-${d.workerClass}`}>{d.workerClass}</span>}
        {ds === "resume" && <span className="bb-review-badge" title={d.closeReason || ""}>{t("meta.paused")}</span>}
        {ds === "retired" && <span className="bb-review-badge bad" title={d.closeReason || ""}>{t("meta.retired")}</span>}
        {childCount > 0 && (
          <span className="bb-childbreak">
            {Number(d.nVerified) > 0 && <b className="v">{d.nVerified}✓</b>}
            {Number(d.nCandidate) > 0 && <b className="c">{d.nCandidate}?</b>}
            {Number(d.nDead) > 0 && <b className="d">{d.nDead}✕</b>}
          </span>
        )}
        {d.worker && <Avatar worker={d.worker} workers={d.workers} />}
      </div>
      <div className="bb-group-goal" title={d.goal}>{(d.summary || "").trim() || d.goal}</div>
    </div>
  );
}

const NODE_TYPES = { group: GroupNode, fact: FactNode, dead: DeadNode, flag: FlagNode, meta: MetaNode };

// ── data → flow (deterministic causal layout; no Math.random) ────────────────
// The provenance protocol is source facts -> intent -> produced fact. We lay intents out
// in DEPTH columns (a compact layered layout), but the column X is now DYNAMIC —
// each column is only as wide as its widest group + a gap, instead of the old
// fixed 780px grid that sprawled and felt out of control once there were many
// intents. Groups collapse to a header; workers can be toggled off.
const SOURCE_X = 24;
const SOURCE_W = 206;
const COL_GAP = 84;
const TOP_Y = 64;
const ROW_GAP = 34;
const FACT_ROW = 122;
// children are now small DOTS, so a group is a compact card (a header + a dot
// grid), not a 690px wide wall of text cards.
const GROUP_W = 252;
const GROUP_W_COLLAPSED = 236;
const GROUP_PAD_TOP = 74; // header band: intent title + 3-line clamped goal, then dots

// ── masonry constants (dot grid) ─────────────────────────────────────────────
const DOT = 18;                 // dot diameter (matches .bb-dot)
const DOT_CELL = 26;            // dot + gap (room for the hover-scale)
const CHILD_GRID_X = 16;        // left inset of the dot grid inside a group
const CHILD_GRID_RIGHT = 16;
const CHILD_COLS = Math.max(1, Math.floor((GROUP_W - CHILD_GRID_X - CHILD_GRID_RIGHT) / DOT_CELL)); // ≈ 8
const DOT_ROW = 24;             // vertical step between dot rows
const GROUP_BOTTOM_PAD = 16;
const MASONRY_COLS = 5;         // how many group columns the wall packs into
const childSlot = (i: number): { x: number; y: number } => ({
  x: CHILD_GRID_X + (i % CHILD_COLS) * DOT_CELL,
  y: GROUP_PAD_TOP + Math.floor(i / CHILD_COLS) * DOT_ROW,
});
// group height as a function of child count (header + rows of dots + bottom pad).
function estimateGroupH(childCount: number): number {
  const rows = Math.max(1, Math.ceil(childCount / CHILD_COLS));
  return GROUP_PAD_TOP + rows * DOT_ROW + GROUP_BOTTOM_PAD;
}

interface FlowOpts {
  collapsed: Set<string>; hidden: Set<string>; onToggle?: (id: string) => void;
  // groups whose folded dead-ends are currently expanded + the toggle callback
  expandedDead: Set<string>; onToggleDead?: (groupId: string) => void;
}

type FactItem = { fact: BlackboardView["facts"][number]; index: number; nodeId: string };

function factNodeId(f: BlackboardView["facts"][number], index: number): string {
  return typeof f.factSeq === "number" && f.factSeq > 0 ? `f:${f.factSeq}` : `f:u${index}`;
}

function intentDepth(
  intent: BlackboardView["intents"][number],
  byId: Map<string, BlackboardView["intents"][number]>,
  producerByFact: Map<number, string>,
  seen = new Set<string>(),
): number {
  if (seen.has(intent.id)) return 0;
  seen.add(intent.id);
  let depth = 0;
  for (const seq of intent.fromFacts || []) {
    const parentIntentId = producerByFact.get(seq);
    const parent = parentIntentId ? byId.get(parentIntentId) : undefined;
    if (parent) depth = Math.max(depth, intentDepth(parent, byId, producerByFact, seen) + 1);
  }
  seen.delete(intent.id);
  return depth;
}

function owningIntent(
  bb: BlackboardView,
  fact: BlackboardView["facts"][number],
): BlackboardView["intents"][number] | undefined {
  // G0 authoritative: the fact carries the intent that produced it (intent_products).
  // Prefer it over the conclude-time single toFactSeq and over the time-window guess.
  if (fact.intentId) {
    const exact = bb.intents.find((it) => it.id === fact.intentId);
    if (exact) return exact;
  }
  if (fact.factSeq) {
    const exact = bb.intents.find((it) => it.toFactSeq === fact.factSeq);
    if (exact) return exact;
  }
  const within = (it: BlackboardView["intents"][number]) => {
    const lo = it.claimedTs ?? it.proposedTs;
    const hi = it.concludedTs ?? Infinity;
    return fact.ts >= lo - 1 && fact.ts <= hi + 1;
  };
  return [...bb.intents]
    .reverse()
    .find((it) => it.worker && it.worker === fact.actor && within(it));
}

function owningIntentForDead(
  bb: BlackboardView,
  dead: BlackboardView["deadEnds"][number],
): BlackboardView["intents"][number] | undefined {
  const within = (it: BlackboardView["intents"][number]) => {
    const lo = it.claimedTs ?? it.proposedTs;
    const hi = it.concludedTs ?? Infinity;
    return dead.ts >= lo - 1 && dead.ts <= hi + 1;
  };
  return [...bb.intents]
    .reverse()
    .find((it) => it.worker && it.worker === dead.actor && within(it));
}

function flattenToFlow(rawBb: BlackboardView, opts: FlowOpts): { nodes: Node<NodeData>[]; edges: Edge[] } {
  // worker lane toggle: drop a hidden worker's facts / dead-ends / claimed intents
  // (open intents with no claimant always stay).
  const visible = (actor?: string) => !actor || !opts.hidden.has(actor);
  const bb: BlackboardView = {
    ...rawBb,
    facts: rawBb.facts.filter((f) => visible(f.actor)),
    deadEnds: rawBb.deadEnds.filter((d) => visible(d.actor)),
    intents: rawBb.intents.filter((it) => !it.worker || visible(it.worker)),
    pocs: rawBb.pocs.filter((p) => visible(p.worker)),
    reviewFindings: rawBb.reviewFindings.filter((f) => visible(f.actor)),
    suppressedRoutes: rawBb.suppressedRoutes.filter((r) => visible(r.actor)),
    branches: rawBb.branches.filter((b) => visible(b.actor)),
    directives: rawBb.directives.filter((d) => visible(d.actor)),
  };
  const W = bb.workers;
  const nodes: Node<NodeData>[] = [];
  const edges: Edge[] = [];
  const factItems: FactItem[] = bb.facts.map((fact, index) => ({ fact, index, nodeId: factNodeId(fact, index) }));
  const factNodeBySeq = new Map<number, string>();
  for (const item of factItems) {
    if (item.fact.factSeq) factNodeBySeq.set(item.fact.factSeq, item.nodeId);
  }

  const intentById = new Map(bb.intents.map((it) => [it.id, it]));
  // producerByFact: fact seq → the intent that PRODUCED it. G0 attaches every
  // worker-produced fact to its intent (intent_products), surfaced per-fact as
  // fact.intentId — that's the authoritative multi-edge source. it.toFactSeq is
  // only the single fact pinned at conclude-time; keep it as a fallback for facts
  // that predate G0 wiring or arrived without an intent_id.
  const producerByFact = new Map<number, string>();
  for (const it of bb.intents) {
    if (it.toFactSeq) producerByFact.set(it.toFactSeq, it.id);
  }
  for (const fact of bb.facts) {
    if (fact.factSeq && fact.intentId && intentById.has(fact.intentId)) {
      producerByFact.set(fact.factSeq, fact.intentId);
    }
  }

  const outputsByIntent = new Map<string, FactItem[]>();
  const usedFactNodes = new Set<string>();
  for (const item of factItems) {
    const owner = owningIntent(bb, item.fact);
    if (!owner) continue;
    const arr = outputsByIntent.get(owner.id) ?? [];
    arr.push(item);
    outputsByIntent.set(owner.id, arr);
    usedFactNodes.add(item.nodeId);
  }

  const deadByIntent = new Map<string, Array<{ dead: BlackboardView["deadEnds"][number]; index: number; nodeId: string }>>();
  bb.deadEnds.forEach((dead, index) => {
    const owner = owningIntentForDead(bb, dead);
    if (!owner) return;
    const arr = deadByIntent.get(owner.id) ?? [];
    arr.push({ dead, index, nodeId: `d:${index}` });
    deadByIntent.set(owner.id, arr);
  });

  // loose nodes (orphan facts with no owning intent + coordinator meta notes) pack
  // into a compact dot grid in a narrow left "loose" lane, not a tall card column.
  const LOOSE_COLS = 6;
  const LOOSE_CELL = 26;
  let looseN = 0;
  const loosePos = () => ({
    x: SOURCE_X + (looseN % LOOSE_COLS) * LOOSE_CELL,
    y: TOP_Y + Math.floor(looseN / LOOSE_COLS) * LOOSE_CELL,
  });
  for (const item of factItems) {
    if (usedFactNodes.has(item.nodeId)) continue;
    nodes.push({
      id: item.nodeId, type: "fact", position: loosePos(),
      data: { workers: W, fact: item.fact.fact, summary: item.fact.summary, verified: item.fact.verified,
        confidence: item.fact.confidence, actor: item.fact.actor, verifier: item.fact.verifier,
        witness: item.fact.witness, artifactId: item.fact.artifactId,
        challenged: item.fact.challenged, challengeReason: item.fact.challengeReason,
        revalidated: item.fact.revalidated, state: item.fact.state, mergedInto: item.fact.mergedInto,
        ts: item.fact.ts },
    });
    looseN += 1;
  }

  const metaItems: Array<{
    id: string; kind: string; title: string; text?: string; actor?: string;
    icon: IconName; tone?: string;
  }> = [
    ...bb.reviewFindings.map((f) => ({
      id: `review:${f.id}`, kind: f.severity || f.kind, title: f.summary || f.kind,
      text: [f.routeHash, f.branchId].filter(Boolean).join(" · "), actor: f.actor,
      icon: "alert" as IconName, tone: f.severity,
    })),
    ...bb.suppressedRoutes.map((r) => ({
      id: `route:${r.routeHash}`, kind: r.reopened ? "route reopened" : "route suppressed",
      title: r.routeHash, text: r.reason, actor: r.actor,
      icon: "network" as IconName, tone: r.reopened ? "ok" : "bad",
    })),
    ...bb.branches.map((b) => ({
      id: `branch:${b.branchId}`, kind: b.status || "branch",
      title: b.title || b.branchId, actor: b.actor,
      icon: "layers" as IconName,
    })),
    ...bb.directives.map((d, idx) => ({
      id: `directive:${d.ts}:${idx}`, kind: d.action || "directive",
      title: d.directive, actor: d.actor, icon: "help" as IconName,
    })),
    ...bb.pocs.map((p) => ({
      id: `poc:${p.id}`, kind: p.status || "poc",
      title: p.name || p.id, text: p.note || p.entryCommand, actor: p.worker,
      icon: "terminal" as IconName,
      tone: p.status === "spent" || p.status === "rejected" ? "dim" : undefined,
    })),
  ];
  for (const item of metaItems) {
    nodes.push({
      id: `m:${item.id}`,
      type: "meta",
      position: loosePos(),
      data: { workers: W, ...item },
    });
    looseN += 1;
  }

  const orderedIntents = [...bb.intents].sort((a, b) => a.proposedTs - b.proposedTs);
  // pre-compute each intent's group size. Children flow in a fixed 3-column grid
  // inside the group (CHILD_COLS), so height is a clean function of child count —
  // see estimateGroupH. (depth is no longer used for layout; masonry packs by
  // height, not causal rank, which is what kills the "everything piles into
  // depth-0 → one endless tall strip" smear.)
  const meta = orderedIntents.map((it) => {
    const isCollapsed = opts.collapsed.has(it.id);
    const outputs = outputsByIntent.get(it.id) ?? [];
    const deads = deadByIntent.get(it.id) ?? [];
    // children are dots now, so every fact + dead renders (no capsule fold needed).
    const childCount = outputs.length + deads.length;
    const width = isCollapsed ? GROUP_W_COLLAPSED : GROUP_W;
    const height = isCollapsed ? GROUP_PAD_TOP + 36 : estimateGroupH(childCount);
    return { it, isCollapsed, outputs, deads, width, height };
  });

  // ── MASONRY layout ──────────────────────────────────────────────────────────
  // Pack the groups into N columns, each group dropped into the SHORTEST column so
  // far. Column width tracks the WIDEST visible group (collapsed groups are narrow,
  // so a default-folded board packs tight in narrow columns; expanding a group
  // widens its column band). When cards are narrow we also use MORE columns, so a
  // folded board of 69 summary cards reads as a compact grid instead of a few very
  // wide, sparse columns that force fitView to shrink everything to nothing.
  const maxGroupW = meta.reduce((mx, m) => Math.max(mx, m.width), GROUP_W_COLLAPSED);
  const colW = maxGroupW + COL_GAP;
  const cols = Math.max(MASONRY_COLS, Math.min(10, Math.round(Math.sqrt(meta.length))));
  const groupPos = new Map<string, { x: number; y: number }>();
  const colHeights = new Array(cols).fill(TOP_Y);
  // start the group wall to the right of the loose-notes lane so they don't overlap.
  const MASONRY_X0 = SOURCE_X + (looseN > 0 ? LOOSE_COLS * LOOSE_CELL + COL_GAP : 0);
  const colX = (c: number) => MASONRY_X0 + c * colW;
  for (const m of meta) {
    let c = 0;
    for (let i = 1; i < cols; i += 1) if (colHeights[i] < colHeights[c]) c = i;
    groupPos.set(m.it.id, { x: colX(c), y: colHeights[c] });
    colHeights[c] += m.height + ROW_GAP;
  }
  // flag lane sits just right of the last column.
  const flagX = colX(cols);

  for (const m of meta) {
    const { it, isCollapsed, outputs, deads, width, height } = m;
    const { x: groupX, y: groupY } = groupPos.get(it.id) ?? { x: SOURCE_X, y: TOP_Y };
    const accent = workerColor(W, it.worker);

    // ONE node per intent: the group card carries all the intent info in its header
    // (no separate stacked intent card). It's a compound parent so the child dots
    // nest inside; it stays non-draggable (masonry owns group placement).
    nodes.push({
      id: `g:${it.id}`,
      type: "group",
      position: { x: groupX, y: groupY },
      data: { workers: W, id: it.id, goal: it.goal, summary: it.summary, workerClass: it.workerClass,
        status: it.status, worker: it.worker, accent,
        dispatchState: it.dispatchState, closeReason: it.closeReason,
        childCount: outputs.length + deads.length,
        nVerified: outputs.filter((o) => o.fact.verified).length,
        nCandidate: outputs.filter((o) => !o.fact.verified).length,
        nDead: deads.length,
        collapsed: isCollapsed, onToggle: opts.onToggle },
      style: { width, height },
      draggable: false,
      selectable: false,
      zIndex: 0,
    });

    // collapsed groups hide their children (and the wiring edges to them).
    if (!isCollapsed) {
      let childIndex = 0;
      const childPos = (index: number) => childSlot(index);
      for (const item of outputs) {
        nodes.push({
          id: item.nodeId,
          type: "fact",
          parentId: `g:${it.id}`,
          extent: "parent",
          position: childPos(childIndex),
          data: { workers: W, fact: item.fact.fact, summary: item.fact.summary, verified: item.fact.verified,
            confidence: item.fact.confidence, actor: item.fact.actor, verifier: item.fact.verifier,
            witness: item.fact.witness, artifactId: item.fact.artifactId,
            challenged: item.fact.challenged, challengeReason: item.fact.challengeReason,
            revalidated: item.fact.revalidated, ts: item.fact.ts },
          zIndex: 2,
        });
        // no intent→child edge: the dot already sits INSIDE its intent's card, so
        // an arrow from the card to a dot within it is redundant clutter.
        childIndex += 1;
      }
      // dead-ends are now tiny red dots too — compact enough that there's no need
      // to fold them behind a capsule; render them all.
      for (const item of deads) {
        nodes.push({
          id: item.nodeId,
          type: "dead",
          parentId: `g:${it.id}`,
          extent: "parent",
          position: childPos(childIndex),
          data: { workers: W, reason: item.dead.reason, actor: item.dead.actor, ts: item.dead.ts },
          zIndex: 2,
        });
        // (no intent→dead edge — same containment reasoning as facts above.)
        childIndex += 1;
      }
    }

    // CROSS-GROUP causal edge: a fact in ANOTHER intent that spawned THIS intent
    // (provenance from_facts). Now targets the group card itself (the merged intent).
    for (const seq of it.fromFacts || []) {
      const src = factNodeBySeq.get(seq);
      if (src) edges.push({ id: `e:${src}->g:${it.id}`, source: src, target: `g:${it.id}`,
        sourceHandle: "s", targetHandle: "t", animated: false });
    }
  }

  // EVERY captured flag gets its own node in the flag lane (multi-flag), not just
  // the first. Falls back to the singular bb.flag for older snapshots.
  const allFlags = (bb.flags && bb.flags.length ? bb.flags : (bb.flag ? [bb.flag] : []));
  const lastFact = [...factItems].reverse().find((item) => item.fact.verified);
  allFlags.forEach((fl, i) => {
    const fid = `flag:${i}`;
    nodes.push({ id: fid, type: "flag", position: { x: flagX, y: TOP_Y + i * 92 },
      data: { workers: W, flag: fl }, draggable: false });
    if (lastFact) edges.push({ id: `e:${lastFact.nodeId}->${fid}`, source: lastFact.nodeId, target: fid,
      sourceHandle: "s", targetHandle: "t", animated: false });
  });
  return { nodes, edges };
}

// ── position persistence v3 (per run, localStorage) ──────────────────────────
// v2 was a flat Record<id,{x,y}> that couldn't tell a group's ABSOLUTE position
// from a child's position RELATIVE to its parent. v3 separates the three node
// kinds so applying a stored layout is unambiguous (and a parent drag doesn't
// double-offset its children).
type XY = { x: number; y: number };
interface LayoutState {
  groups: Record<string, XY>;   // "g:<intentId>" → absolute
  children: Record<string, XY>; // child node id → relative to its parent group
  free: Record<string, XY>;     // orphan nodes (flag, meta) → absolute
}
const emptyLayout = (): LayoutState => ({ groups: {}, children: {}, free: {} });
const layoutKey = (runId: string) => `muteki.bb.layout.v3.${runId || "default"}`;
function loadLayout(runId: string): LayoutState {
  try {
    const raw = JSON.parse(localStorage.getItem(layoutKey(runId)) || "null");
    if (raw && raw.groups && raw.children && raw.free) return raw as LayoutState;
  } catch { /* fall through */ }
  return emptyLayout();
}
function saveLayout(runId: string, s: LayoutState) {
  try { localStorage.setItem(layoutKey(runId), JSON.stringify(s)); } catch { /* quota / private mode */ }
}
// overlay a stored position onto a node by its kind. No record → keep the masonry
// position the layout computed.
function applyStoredPosition(n: Node<NodeData>, s: LayoutState): Node<NodeData> {
  if (n.id.startsWith("g:")) return s.groups[n.id] ? { ...n, position: s.groups[n.id] } : n;
  if (n.parentId) return s.children[n.id] ? { ...n, position: s.children[n.id] } : n;
  return s.free[n.id] ? { ...n, position: s.free[n.id] } : n;
}

// the searchable text of a sticky — across all node shapes (intent/fact/dead/flag/group)
function nodeText(n: Node<NodeData>): string {
  const d = (n.data ?? {}) as Record<string, unknown>;
  return [d.goal, d.summary, d.fact, d.reason, d.flag, d.label, d.id]
    .filter((v) => typeof v === "string")
    .join(" ");
}

// ── click-to-inspect detail ──────────────────────────────────────────────────
// the dots / cards only show a clamped preview; clicking one opens this panel with
// the FULL text + a provenance row. Built from the node's data by kind.
interface NodeDetail {
  kind: string; title: string; full: string; gist?: string;
  actor?: string; verified?: boolean; confidence?: number; verifier?: string; witness?: string;
}
function nodeToDetail(n: Node<NodeData>): NodeDetail {
  const d = (n.data ?? {}) as Record<string, any>;
  const type = (n.type || "").toString();
  if (type === "fact") return {
    kind: d.verified ? "已验证事实" : "候选事实", title: d.actor || "fact",
    full: d.fact || "", gist: (d.summary || "").trim() || undefined,
    actor: d.actor, verified: !!d.verified, confidence: d.confidence, verifier: d.verifier, witness: d.witness,
  };
  if (type === "dead") return { kind: "死路", title: d.actor || "dead-end", full: d.reason || "", actor: d.actor };
  if (type === "meta") return { kind: d.kind || "note", title: d.title || "", full: [d.title, d.text].filter(Boolean).join("\n\n"), actor: d.actor };
  if (type === "flag") return { kind: "FLAG", title: "flag", full: d.flag || "" };
  if (type === "group") return {
    kind: "意图", title: d.id || "intent", full: d.goal || "", gist: (d.summary || "").trim() || undefined, actor: d.worker,
  };
  return { kind: type, title: d.id || d.title || "", full: d.text || d.goal || d.fact || "" };
}
function DetailPanel({ detail, onClose }: { detail: NodeDetail; onClose: () => void }) {
  const t = useT();
  const d = detail;
  return (
    <div className="bb-detail" role="dialog">
      <div className="bb-detail-head">
        <span className="bb-detail-kind">{d.kind}</span>
        <span className="bb-detail-title">{d.title}</span>
        <button className="bb-detail-x" onClick={onClose} aria-label={t("common.close") || "关闭"}>
          <Icon name="x" size={14} />
        </button>
      </div>
      {d.gist && <div className="bb-detail-gist">{d.gist}</div>}
      <div className="bb-detail-full">{d.full || "—"}</div>
      <dl className="bb-detail-prov">
        {d.actor && <><dt>{t("insp.actor") || "actor"}</dt><dd>{d.actor}</dd></>}
        {d.verified !== undefined && <><dt>{t("insp.provenance") || "状态"}</dt>
          <dd className={d.verified ? "ok" : "warn"}>{d.verified ? (t("insp.verified") || "已验证") : (t("insp.unverified") || "未验证")}</dd></>}
        {typeof d.confidence === "number" && <><dt>{t("insp.confidence") || "置信"}</dt><dd>{Number(d.confidence).toFixed(2)}</dd></>}
        {d.verifier && d.verifier !== "none" && <><dt>{t("insp.verifier") || "verifier"}</dt><dd>{d.verifier}</dd></>}
        {d.witness && <><dt>{t("bb.witness") || "witness"}</dt><dd className="bb-witness">{d.witness}</dd></>}
      </dl>
    </div>
  );
}

function Canvas({ bb, runId }: { bb: BlackboardView; runId: string }) {
  const t = useT();
  const { fitView } = useReactFlow();
  const [showEdges, setShowEdges] = useState(true);
  const [query, setQuery] = useState("");
  const queryRef = useRef("");
  queryRef.current = query;
  const posRef = useRef<LayoutState>(emptyLayout());
  // collapsed intent groups + hidden workers (lane toggles). Refs mirror state so
  // the rebuild effect (keyed on bb) reads the latest without re-subscribing.
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [hidden, setHidden] = useState<Set<string>>(new Set());
  // groups whose folded (low-value) dead-ends are expanded. Visual density only —
  // intentionally NOT persisted to localStorage (it shouldn't taint drag layout).
  const [expandedDead, setExpandedDead] = useState<Set<string>>(new Set());
  const collapsedRef = useRef(collapsed); collapsedRef.current = collapsed;
  const hiddenRef = useRef(hidden); hiddenRef.current = hidden;
  const expandedDeadRef = useRef(expandedDead); expandedDeadRef.current = expandedDead;
  const didFit = useRef(false);
  const [nodes, setNodes, onNodesChange] = useNodesState<Node<NodeData>>([]);
  const [edges, setEdges] = useEdgesState<Edge>([]);
  // WHITE-SCREEN FIX: fitView used to run on setTimeout(…,0), which fires long
  // before React Flow has measured the 300+ nodes (measurement is async via the
  // browser ResizeObserver) → it fits against zero bounds and the viewport lands
  // off-screen, blank, with no recovery. Instead we GATE fitView behind three
  // conditions: the flow is initialised (onInit), the nodes are measured
  // (useNodesInitialized), and a fit is pending. pendingFit is set on every
  // (re)build; the gated effect below does the actual fit on the next frame.
  const nodesInitialized = useNodesInitialized({ includeHiddenNodes: false });
  const [flowReady, setFlowReady] = useState(false);
  const pendingFit = useRef(false);

  const toggleCollapse = useCallback((id: string) => {
    setCollapsed((prev) => { const n = new Set(prev); n.has(id) ? n.delete(id) : n.add(id); return n; });
  }, []);
  const toggleDeadGroup = useCallback((groupId: string) => {
    setExpandedDead((prev) => { const n = new Set(prev); n.has(groupId) ? n.delete(groupId) : n.add(groupId); return n; });
  }, []);
  const toggleWorker = useCallback((w: string) => {
    setHidden((prev) => { const n = new Set(prev); n.has(w) ? n.delete(w) : n.add(w); return n; });
  }, []);
  // click a dot / card → show its FULL content (untruncated) in a floating panel,
  // since the dots/cards only show a clamped preview. null = nothing selected.
  const [detail, setDetail] = useState<NodeDetail | null>(null);
  const onNodeClick = useCallback((_e: ReactMouseEvent, node: Node<NodeData>) => {
    setDetail(nodeToDetail(node));
  }, []);
  const workerFilterSummary = hidden.size === 0
    ? t("bb.workersAll")
    : t("bb.workersVisible", { n: Math.max(0, bb.workers.length - hidden.size), total: bb.workers.length });

  // dim a node when a non-empty search doesn't match its text (keeps position).
  const withDim = useCallback((n: Node<NodeData>): Node<NodeData> => {
    const q = queryRef.current.trim().toLowerCase();
    const match = !q || nodeText(n).toLowerCase().includes(q);
    return { ...n, style: { ...(n.style || {}), opacity: match ? 1 : 0.16 } };
  }, []);

  const buildOpts = useCallback((): FlowOpts => ({
    collapsed: collapsedRef.current, hidden: hiddenRef.current, onToggle: toggleCollapse,
    expandedDead: expandedDeadRef.current, onToggleDead: toggleDeadGroup,
  }), [toggleCollapse, toggleDeadGroup]);

  // hydrate stored positions once per run
  // load the stored layout synchronously when the run changes — keyed in a ref so
  // the rebuild below (which also depends on runId) reads the freshly-loaded
  // positions on the same mount rather than racing an async ref assignment.
  const loadedRun = useRef<string | null>(null);
  if (loadedRun.current !== runId) {
    loadedRun.current = runId;
    posRef.current = loadLayout(runId);
    didFit.current = false;
  }


  // rebuild from the live blackboard, overlaying any dragged (stored) positions.
  // We do NOT pass React Flow's `fitView` prop (it re-centred on every delta —
  // the "blackboard jumps around once there are many notes" problem). Instead we
  // fit ONCE on the first build per run; later deltas keep the viewport stable.
  useEffect(() => {
    const flow = flattenToFlow(bb, buildOpts());
    const stored = posRef.current;
    setNodes(flow.nodes.map((n) => withDim(applyStoredPosition(n, stored))));
    setEdges(flow.edges);
    if (!didFit.current) {
      didFit.current = true;
      pendingFit.current = true; // fit once the nodes are measured (gated effect below)
    }
  }, [bb, runId, collapsed, hidden, expandedDead, setNodes, setEdges, withDim, buildOpts]);

  // GATED fitView: runs only when the flow is initialised AND nodes are measured
  // AND a fit is pending. requestAnimationFrame defers it one frame past the
  // measurement commit so React Flow has real bounds to frame.
  useEffect(() => {
    if (!flowReady || !nodesInitialized || !pendingFit.current || nodes.length === 0) return;
    pendingFit.current = false;
    const raf = requestAnimationFrame(() => {
      void fitView({ duration: 200, padding: 0.16, minZoom: 0.12 });
    });
    return () => cancelAnimationFrame(raf);
  }, [flowReady, nodesInitialized, nodes.length, fitView]);

  // re-apply search dimming when the query changes (without rebuilding positions)
  useEffect(() => {
    setNodes((ns) => ns.map(withDim));
  }, [query, setNodes, withDim]);

  // persist a node's position when the user finishes dragging it, bucketed by kind:
  // groups → absolute, children (have a parentId) → relative-to-parent, others →
  // free/absolute. The capsule (dc:) is non-draggable so it never lands here.
  const nodesRef = useRef(nodes); nodesRef.current = nodes;
  const handleChange = useCallback((changes: NodeChange<Node<NodeData>>[]) => {
    onNodesChange(changes);
    let dirty = false;
    const s = posRef.current;
    for (const c of changes) {
      if (c.type !== "position" || !c.position) continue;
      if (c.id.startsWith("g:")) { s.groups[c.id] = c.position; dirty = true; }
      else {
        const n = nodesRef.current.find((x) => x.id === c.id);
        if (n?.parentId) s.children[c.id] = c.position;
        else s.free[c.id] = c.position;
        dirty = true;
      }
    }
    if (dirty) saveLayout(runId, s);
  }, [onNodesChange, runId]);

  const resetLayout = useCallback(() => {
    posRef.current = emptyLayout();
    try { localStorage.removeItem(layoutKey(runId)); } catch { /* ignore */ }
    const flow = flattenToFlow(bb, buildOpts());
    setNodes(flow.nodes);
    pendingFit.current = true; // re-fit once re-measured (gated effect handles timing)
  }, [bb, runId, setNodes, buildOpts]);

  return (
    <>
      <div className="bb-toolbar">
        <div className="bb-toolbar-main">
          <input
            className="bb-search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("bb.search")}
          />
          <button className={`bb-tool-btn ${showEdges ? "on" : ""}`} onClick={() => setShowEdges((v) => !v)} aria-pressed={showEdges}>
            <Icon name="network" size={14} /> {t("bb.links")}
          </button>
          <button className="bb-tool-btn" onClick={() => fitView({ duration: 250, padding: 0.16, minZoom: 0.12 })}><Icon name="crosshair" size={14} /> {t("bb.fit")}</button>
          <button className="bb-tool-btn" onClick={resetLayout}><Icon name="refresh" size={14} /> {t("bb.reset")}</button>
        </div>
        <ChipFilterBar
          id="bb-worker-rail"
          className="bb-worker-filter"
          variant="floating"
          label={t("bb.workers")}
          summary={workerFilterSummary}
          title={t("bb.workers")}
          expandTitle={t("bb.workersExpand")}
          collapseTitle={t("bb.workersCollapse")}
        >
          {bb.workers.map((w) => (
            <button key={w} className={`bb-worker-chip ${hidden.has(w) ? "off" : ""}`}
              title={`${w} · ${hidden.has(w) ? t("bb.showWorker") : t("bb.hideWorker")}`}
              aria-pressed={!hidden.has(w)}
              onClick={() => toggleWorker(w)}>
              <Avatar worker={w} workers={bb.workers} />
              <span className="bb-worker-label">{workerShortLabel(w)}</span>
            </button>
          ))}
          {bb.flag && (
            <CopyText value={bb.flag} className="bb-flagchip">
              <Icon name="flag" size={13} /> {bb.flag}
            </CopyText>
          )}
        </ChipFilterBar>
      </div>
      <ReactFlow
        className={`bb-flow ${showEdges ? "" : "rf-no-edges"}`}
        nodes={nodes}
        edges={edges}
        onNodesChange={handleChange}
        onNodeClick={onNodeClick}
        onPaneClick={() => setDetail(null)}
        onInit={() => setFlowReady(true)}
        nodeTypes={NODE_TYPES}
        defaultEdgeOptions={{ type: "default" }}
        nodesConnectable={false}
        nodesDraggable={false}
        elementsSelectable
        minZoom={0.1}
        maxZoom={2}
        proOptions={{ hideAttribution: true }}
      >
        <Background variant={BackgroundVariant.Dots} gap={22} size={1} color={cssVar("--line", "#d6dbe6")} />
        <Controls showInteractive={false} />
        <MiniMap pannable zoomable nodeStrokeWidth={2} maskColor="rgba(244,246,250,0.65)"
          style={{ background: cssVar("--panel2", "#f6f8fc"), border: `1px solid ${cssVar("--border", "#d6dbe6")}` }} />
      </ReactFlow>
      {detail && <DetailPanel detail={detail} onClose={() => setDetail(null)} />}
    </>
  );
}

export function Blackboard({ bb, runId }: { bb: BlackboardView; runId: string }) {
  const t = useT();
  const isEmpty = bb.intents.length === 0 && bb.facts.length === 0
    && bb.deadEnds.length === 0 && bb.pocs.length === 0
    && bb.reviewFindings.length === 0 && bb.suppressedRoutes.length === 0
    && bb.branches.length === 0 && bb.directives.length === 0 && !bb.flag;
  if (isEmpty)
    return (
      <div className="bb-canvas bb-blank">
        <div className="panel-empty">
          <span className="panel-empty-ico" aria-hidden="true"><Icon name="board" size={26} /></span>
          <span className="panel-empty-title">{t("bb.emptyTitle")}</span>
          <span className="panel-empty-hint">{t("bb.emptyHint")}</span>
        </div>
      </div>
    );
  return (
    <div className="bb-canvas">
      <ReactFlowProvider>
        <Canvas bb={bb} runId={runId} />
      </ReactFlowProvider>
    </div>
  );
}
