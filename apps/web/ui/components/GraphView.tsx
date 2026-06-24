"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import cytoscape, { type Core, type ElementDefinition } from "cytoscape";
import cola from "cytoscape-cola";
import { GraphModel, GraphNode } from "@/lib/events";
import { useT } from "@/lib/i18n";
import { workerShortLabel, workerColor } from "@/lib/workers";
import { Icon } from "@/components/Icon";
import { ChipFilterBar } from "@/components/ChipFilterBar";

/**
 * The live, evolving fact-graph, fed by a real push stream instead of polling.
 * Laid out as an INTENT RADAR: each intent is a hub with the facts/candidates/
 * dead-ends it produced ringed around it; cross-intent fact→intent loop edges
 * dim. Solver + challenge scaffolding is hidden (intents are the hubs, so the
 * "who ran it" nodes are pure clutter — worker identity lives in node colour).
 *
 * We add elements incrementally (never rebuild) so positions stay stable, and
 * re-run the radar layout only on structural change (model.version bump). Click
 * a node to inspect it / focus its causal chain.
 */

const NODE_STYLE: Record<string, { bg: string; shape: string; border?: string }> = {
  challenge: { bg: "#7c3aed", shape: "round-rectangle" },
  solver: { bg: "#2563eb", shape: "round-rectangle" },
  fact: { bg: "#16a34a", shape: "ellipse" },
  candidate: { bg: "#d97706", shape: "ellipse", border: "#f59e0b" },
  intent: { bg: "#0ea5e9", shape: "diamond" },
  dead_end: { bg: "#b91c1c", shape: "octagon", border: "#ef4444" },
  flag: { bg: "#d4a017", shape: "star" },
};

// ── intent-centric "radar" layout ───────────────────────────────────────────
// Computes explicit positions so each INTENT is a hub with the facts/candidates/
// dead-ends it produced ringed around it, and the intent hubs themselves flow
// left→right by causal depth. Fed to cytoscape's `preset` layout. This groups
// nodes by their causal OWNER (the intent that produced them) instead of by
// topological rank — which is what kills the dagre "one tall column per rank +
// crossing fan" spaghetti. Childless nodes (lone solvers, orphan facts) tuck into
// a tail lane so they don't distort the hub grid.
function computeRadarPositions(cy: Core): Record<string, { x: number; y: number }> {
  const pos: Record<string, { x: number; y: number }> = {};
  // a node is "out" of the radar if collapsed (.gone) or it's hidden scaffolding
  // (.radar-hidden = solver/challenge). Don't reserve grid space for either.
  const isOut = (n: any) => n.hasClass("gone") || n.hasClass("radar-hidden");
  const intents = cy.nodes("[type='intent']").filter((n) => !isOut(n));
  const flag = cy.nodes("[type='flag']").filter((n) => !isOut(n));

  // children of an intent = the fact/candidate/dead-end nodes it produced
  // (produces / verifies / claims / refutes edges out of the intent).
  const childrenOf = (intent: any) =>
    intent.outgoers("edge").targets()
      .filter((t: any) => !isOut(t) &&
        ["fact", "candidate", "dead_end", "poc"].includes(t.data("type")));

  // Most of the 69 intents have FEW or zero children (median worker subtree ≈ 1),
  // so giving every intent a full radar cell wastes enormous space. Split into:
  //   • "rich" hubs (have produced children) → a spacious grid with rings
  //   • "lean" intents (no children) → a tight chip grid in a side lane
  // This keeps the populated radars readable while the long tail of empty intents
  // packs into a corner instead of stretching the canvas to thousands of px.
  const rich: any[] = [], lean: any[] = [];
  intents.forEach((intent: any) => { (childrenOf(intent).length > 0 ? rich : lean).push(intent); });

  const COL_GAP = 280, ROW_GAP = 240, PER_COL = 3;
  const RING_R = 78;
  rich.forEach((intent: any, idx: number) => {
    const col = Math.floor(idx / PER_COL);
    const row = idx % PER_COL;
    const hx = 200 + col * COL_GAP;
    const hy = 150 + row * ROW_GAP;
    pos[intent.id()] = { x: hx, y: hy };
    const kids = childrenOf(intent);
    const n = kids.length;
    const r = RING_R + Math.max(0, n - 6) * 6;
    kids.forEach((kid: any, i: number) => {
      const ang = (2 * Math.PI * i) / Math.max(1, n) - Math.PI / 2;
      pos[kid.id()] = { x: hx + r * Math.cos(ang), y: hy + r * Math.sin(ang) };
    });
  });
  const richCols = Math.max(1, Math.ceil(rich.length / PER_COL));
  const richRight = 200 + richCols * COL_GAP;

  // lean intents: a compact chip grid to the right of the rich hubs
  const LEAN_GX = 132, LEAN_GY = 56, LEAN_PER_COL = 8;
  lean.forEach((intent: any, i: number) => {
    pos[intent.id()] = {
      x: richRight + 80 + Math.floor(i / LEAN_PER_COL) * LEAN_GX,
      y: 150 + (i % LEAN_PER_COL) * LEAN_GY,
    };
  });

  // challenge top-left origin; flag(s) in a dedicated lane far right.
  const leanCols = lean.length ? Math.ceil(lean.length / LEAN_PER_COL) : 0;
  const farRight = richRight + 80 + leanCols * LEAN_GX;
  // challenge/solver scaffolding is radar-hidden, so it isn't placed. flag(s) get a
  // dedicated lane far right (the causal endpoint).
  flag.forEach((f: any, i: number) => { pos[f.id()] = { x: farRight + 120, y: 180 + i * 140 }; });

  // genuine orphans (facts with no owning intent, not scaffolding) → a tail row
  // under the rich grid. radar-hidden nodes are skipped (isOut) so the lane stays
  // clean.
  let tail = 0;
  const tailTop = 150 + PER_COL * ROW_GAP + 60;
  cy.nodes().filter((n: any) => !isOut(n) && !pos[n.id()]).forEach((n: any) => {
    pos[n.id()] = { x: 120 + (tail % 10) * 130, y: tailTop + Math.floor(tail / 10) * 80 };
    tail++;
  });
  return pos;
}

let colaRegistered = false;
function ensureColaRegistered() {
  if (colaRegistered) return;
  try { cytoscape.use(cola); } catch { /* already registered (HMR) */ }
  colaRegistered = true;
}

// Read a :root CSS variable at runtime so the Cytoscape canvas (which can't use
// CSS vars in its own stylesheet) follows the active theme — single source of
// truth in globals.css. Falls back if the var is missing (SSR/first paint).
function cssVar(name: string, fallback: string): string {
  if (typeof window === "undefined") return fallback;
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

export function GraphView({
  model,
  onSelect,
}: {
  model: GraphModel;
  onSelect?: (node: GraphNode | null) => void;
}) {
  const t = useT();
  const boxRef = useRef<HTMLDivElement>(null);
  const cyRef = useRef<Core | null>(null);
  const seenNodes = useRef<Set<string>>(new Set());
  const seenEdges = useRef<Set<string>>(new Set());
  const lastVersion = useRef<number>(-1);
  const didFit = useRef<boolean>(false);
  const [ready, setReady] = useState(false);
  const [query, setQuery] = useState("");
  // focus = a clicked node whose causal chain (predecessors + successors) stays
  // lit while everything else dims. collapsed = hide leaf facts/candidates/dead-
  // ends to show the skeleton (challenge → solver → intent → flag) when a busy
  // graph gets unreadable. Both address the "graph spirals out of control with
  // many nodes" problem without a full re-layout.
  const [focusId, setFocusId] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState(false);

  // one-time cytoscape init. Cytoscape is bundled with the client chunk instead
  // of late-loaded at panel-open time; that avoids dev-server stale chunk 404s
  // that left the graph pane blank after long live-test sessions.
  useEffect(() => {
    let ro: ResizeObserver | null = null;
    ensureColaRegistered();
    if (!boxRef.current) return;
    const cy = cytoscape({
        container: boxRef.current,
        elements: [],
        // INTENTIONAL slow wheel-zoom for controlled navigation of busy graphs.
        // Cytoscape logs a one-time info-level warning for ANY non-default
        // wheelSensitivity — that warning is expected and harmless here; we keep
        // the slower zoom feel rather than silence it by reverting to default.
        wheelSensitivity: 0.2,
        // CONTROLLED zoom: clamp so the graph can't shrink to a dot or zoom to
        // infinity (the "uncontrollable once there are many nodes" complaint).
        // 0.18 floor lets the wider radar layout fit fully on screen while still
        // preventing a shrink-to-nothing.
        minZoom: 0.18,
        maxZoom: 2.5,
        style: [
          {
            selector: "node",
            style: {
              label: "data(label)",
              color: cssVar("--text", "#1d2530"),
              "font-size": "11px",
              // ellipsis (single line) keeps busy graphs from bloating into tall
              // overlapping labels — the full text lives in the node inspector.
              "text-wrap": "ellipsis",
              "text-max-width": "120px",
              "text-valign": "bottom",
              "text-margin-y": 7,
              width: 34,
              height: 34,
              "border-width": 2,
              "border-color": cssVar("--panel", "#ffffff"),
            },
          },
          ...Object.entries(NODE_STYLE).map(([type, st]) => ({
            selector: `node[type='${type}']`,
            style: {
              "background-color": st.bg,
              shape: st.shape as any,
              ...(st.border ? { "border-color": st.border } : {}),
            },
          })),
          { selector: "node[type='flag']", style: { width: 46, height: 46, "border-color": "#d4a017" } },
          { selector: "node[type='challenge']", style: { width: 48, height: 34 } },
          { selector: "node[type='solver']", style: { width: 44, height: 32 } },
          { selector: "node[type='intent']", style: { width: 38, height: 38, "border-width": 2.5 } },
          // ── worker-coloured border ────────────────────────────────────────────
          // any node carrying a `wc` (its producing worker's engine colour) gets a
          // thicker ring in that colour, so the operator reads "who produced this"
          // at a glance. Applies to fact / candidate / dead_end / intent that have
          // an actor; un-attributed nodes keep their type border (neutral).
          { selector: "node[wc]", style: { "border-color": "data(wc)", "border-width": 3 } },
          // verified fact = SOLID filled green; candidate = HOLLOW (paper fill +
          // coloured ring) so verified-vs-unverified is legible without reading text.
          { selector: "node[type='candidate']", style: {
              "background-color": cssVar("--panel", "#ffffff"),
              "background-opacity": 1,
              "border-width": 3,
          } },
          // un-attributed candidate keeps the amber ring; attributed ones get `wc` above
          { selector: "node[type='candidate'][^wc]", style: { "border-color": "#d97706" } },
          // dead-ends read as "dead" via the red octagon shape + faded opacity; the
          // cytoscape canvas renderer doesn't support `text-decoration` (it warns
          // and ignores it), so we rely on the shape/colour instead of a strike.
          { selector: "node[type='dead_end']", style: { opacity: 0.6 } },
          {
            selector: "edge",
            style: {
              width: 1.5,
              "line-color": cssVar("--line2", "#b4bccd"),
              "target-arrow-color": cssVar("--line2", "#b4bccd"),
              "target-arrow-shape": "triangle",
              "curve-style": "bezier",
              label: "data(kind)",
              "font-size": "8px",
              color: cssVar("--muted", "#5a6478"),
              "text-rotation": "autorotate",
              "text-margin-y": -8,
            },
          },
          { selector: "edge[kind='verifies']", style: { "line-color": "#16a34a", "target-arrow-color": "#16a34a" } },
          { selector: "edge[kind='produces']", style: { "line-color": "#22c55e", "target-arrow-color": "#22c55e", width: 2.2 } },
          { selector: "edge[kind='plans']", style: { "line-color": "#38bdf8", "target-arrow-color": "#38bdf8", "line-style": "dashed" } },
          { selector: "edge[kind='claims']", style: { "line-color": "#f59e0b", "target-arrow-color": "#f59e0b", "line-style": "dashed" } },
          { selector: "edge[kind='refutes']", style: { "line-color": "#ef4444", "target-arrow-color": "#ef4444", "line-style": "dashed" } },
          { selector: "edge[kind='solves']", style: { "line-color": "#eab308", "target-arrow-color": "#eab308", width: 2.5 } },
          // radar mode: cross-cause (fact→intent loop) edges jump between hubs and
          // would otherwise tangle the rings — render them faint + violet + label-
          // less so the intra-hub produce edges stay readable. (applied only when
          // .crosscause is toggled on, i.e. in radar mode.)
          { selector: "edge.crosscause", style: {
              "line-color": "#7c3aed", "target-arrow-color": "#7c3aed",
              "line-style": "dashed", width: 1, opacity: 0.3, label: "",
              "curve-style": "unbundled-bezier", "control-point-distances": [40], "control-point-weights": [0.5],
          } },
          { selector: "edge.crosscause:selected", style: { opacity: 0.95, width: 2 } },
          { selector: "node:selected", style: { "border-color": cssVar("--bright", "#080c14"), "border-width": 3 } },
          // search/focus highlight: dim everything off-match, ring the matches.
          { selector: ".dim", style: { opacity: 0.12 } },
          { selector: ".gone", style: { display: "none" } as any },
          // radar mode hides the solver/challenge scaffolding (set in the layout-mode effect)
          { selector: ".radar-hidden", style: { display: "none" } as any },
          { selector: "node.hit", style: { "border-color": cssVar("--bright", "#080c14"), "border-width": 4 } },
          { selector: "node.focus", style: { "border-color": cssVar("--blue", "#2563eb"), "border-width": 4 } },
        ],
    });
    cy.on("tap", "node", (e) => {
      const d = e.target.data();
      onSelect?.({ id: d.id, type: d.type, label: d.label, meta: d.meta });
      // toggle focus on the tapped node's causal chain
      setFocusId((cur) => (cur === d.id ? null : d.id));
    });
    cy.on("tap", (e) => {
      if (e.target === cy) { onSelect?.(null); setFocusId(null); }
    });
    cyRef.current = cy;
    setReady(true);
    // Cytoscape doesn't react to container size changes on its own — when the
    // panel/window resizes the canvas dimensions go stale and the graph can land
    // off-screen (blank). Observe the container and resize + refit to keep it visible.
    if (typeof ResizeObserver !== "undefined") {
      ro = new ResizeObserver(() => {
        const c = cyRef.current;
        if (!c) return;
        c.resize();
        c.fit(c.elements().not(".gone"), 30);
      });
      ro.observe(boxRef.current!);
    }
    return () => {
      ro?.disconnect();
      cyRef.current?.destroy();
      cyRef.current = null;
      seenNodes.current.clear();
      seenEdges.current.clear();
      lastVersion.current = -1;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // run the intent-radar layout. The radar grid (computeRadarPositions) SEEDS the
  // positions, then cola's force simulation relaxes them into organic, live-floating
  // clusters (à la the cytoscape-cola compound demo) — structure + life. Scaffolding
  // (solver/challenge) is hidden and fact→intent loop edges dimmed here so live-
  // streamed nodes inherit it too. colaRef holds the running layout so a new delta
  // can stop the prior simulation before starting the next (no thrashing).
  const colaRef = useRef<any>(null);
  const runLayout = useCallback((cy: Core, fitNow: boolean) => {
    const scaffold = cy.nodes("[type='solver'], [type='challenge']");
    scaffold.addClass("radar-hidden");
    scaffold.connectedEdges().addClass("radar-hidden");
    cy.edges("[cc]").addClass("crosscause");

    // 1. seed: place nodes on the deterministic radar grid first so cola starts from
    //    a sane, structured arrangement instead of random chaos.
    const positions = computeRadarPositions(cy);
    cy.batch(() => {
      cy.nodes().forEach((n: any) => { if (positions[n.id()]) n.position(positions[n.id()]); });
    });

    // 2. relax: run cola force physics on the visible subset. Seeded (randomize:false)
    //    so it gently settles the radar into organic clusters rather than re-deriving
    //    the whole layout. Bounded sim time keeps a 350-node graph from churning CPU.
    colaRef.current?.stop();
    const eles = cy.elements().not(".gone").not(".radar-hidden");
    const layout = eles.layout({
      name: "cola",
      animate: true,
      randomize: false,
      fit: fitNow,
      padding: 50,
      maxSimulationTime: 2000,
      convergenceThreshold: 0.02,
      avoidOverlap: true,
      handleDisconnected: true,
      nodeSpacing: () => 12,
      edgeLength: (e: any) => (e.data("kind") === "produces" ? 80 : 150),
    } as any);
    colaRef.current = layout;
    layout.run();
  }, []);

  // incremental sync: add only new nodes/edges, then re-layout on version bump
  useEffect(() => {
    const cy = cyRef.current;
    if (!cy || !ready) return;
    const toAdd: ElementDefinition[] = [];
    for (const n of model.nodes) {
      if (!seenNodes.current.has(n.id)) {
        seenNodes.current.add(n.id);
        // wc = the producing worker's per-engine colour, surfaced as a plain data
        // attribute so the (CSS-var-less) cytoscape stylesheet can paint the node's
        // border by worker via `border-color: data(wc)`. meta.actor was wired in
        // events.ts; absent on historical/un-attributed nodes → falls back to the
        // type's own border in the stylesheet.
        const actor = n.meta?.actor as string | undefined;
        const wc = actor ? workerColor(actor) : undefined;
        toAdd.push({ data: { id: n.id, label: n.label, type: n.type, meta: n.meta, ...(wc ? { wc } : {}) } });
      } else {
        // node already on the graph — but its label may have changed (a zh gist
        // landed via node.summarized). Patch the live cy node in place so the
        // raw English flips to the short gist without a full rebuild.
        const cyNode = cy.getElementById(n.id);
        if (cyNode.nonempty() && cyNode.data("label") !== n.label) {
          cyNode.data("label", n.label);
          cyNode.data("meta", n.meta);
        }
      }
    }
    for (const e of model.edges) {
      if (!seenEdges.current.has(e.id) && seenNodes.current.has(e.source) && seenNodes.current.has(e.target)) {
        seenEdges.current.add(e.id);
        // cc = "cross-cause": a fact→intent loop edge (a fact spawning a new intent),
        // as opposed to a solver→intent planning edge — both carry kind='plans', so
        // we distinguish by the source node's type. The radar dims cc edges (they
        // jump between hubs) and keeps intra-hub produce edges crisp.
        const srcType = model.nodes.find((n) => n.id === e.source)?.type;
        const cc = e.kind === "plans" && srcType === "fact" ? 1 : undefined;
        toAdd.push({ data: { id: e.id, source: e.source, target: e.target, kind: e.kind, ...(cc ? { cc } : {}) } });
      } else if (!seenEdges.current.has(e.id) && (!seenNodes.current.has(e.source) || !seenNodes.current.has(e.target))) {
        console.warn("[muteki graph] cytoscape edge waiting for endpoint", {
          edge: e,
          nodes: Array.from(seenNodes.current),
        });
      }
    }
    if (toAdd.length) cy.add(toAdd);
    if (model.version !== lastVersion.current) {
      lastVersion.current = model.version;
      // Only auto-FIT the FIRST layout. On every later structural change we
      // re-layout but DON'T recentre — constant re-fit was the "graph jumps
      // around / feels out of control once there are many nodes" problem. The
      // manual fit button re-fits on demand.
      const fitNow = !didFit.current;
      didFit.current = true;
      runLayout(cy, fitNow);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [model, ready]);

  // combined visibility: collapse leaves (skeleton), dim off-search, and focus a
  // node's causal chain (predecessors + successors). Recomputed whenever any of
  // {query, focusId, collapsed, model} change so a busy graph stays readable.
  // ALL node types are shown by default — collapse is an OPT-IN skeleton toggle,
  // never automatic (auto-collapsing hid the facts/flag and read as "nodes missing").
  const prevCollapsed = useRef(collapsed);
  useEffect(() => {
    const cy = cyRef.current;
    if (!cy || !ready) return;
    cy.batch(() => {
      cy.elements().removeClass("dim hit focus gone");
      // 1. collapse: hide leaf fact/candidate/dead-end nodes + their edges.
      if (collapsed) {
        const leaves = cy.nodes("[type='fact'], [type='candidate'], [type='dead_end']");
        leaves.addClass("gone");
        leaves.connectedEdges().addClass("gone");
      }
      const live = cy.elements().not(".gone");
      // 2. focus: a clicked node's whole causal chain stays lit.
      let keep = cy.collection();
      if (focusId) {
        const node = cy.getElementById(focusId);
        if (node.nonempty() && !node.hasClass("gone")) {
          keep = node.union(node.successors()).union(node.predecessors());
          node.addClass("focus");
        }
      }
      // 3. search: ring matches + their neighbourhood.
      const q = query.trim().toLowerCase();
      if (q) {
        const matches = live.nodes().filter((n) => (n.data("label") || "").toLowerCase().includes(q));
        matches.addClass("hit");
        keep = keep.union(matches).union(matches.neighborhood());
        if (matches.length === 0 && !focusId) { live.addClass("dim"); return; }
      }
      if (keep.nonempty()) live.not(keep).addClass("dim");
    });
    // When the COLLAPSE toggle flips, re-lay-out the VISIBLE subset so the skeleton
    // settles cleanly + recentres — without it the kept nodes hang in their old
    // full-graph positions. Guarded to the collapse change so typing in search /
    // model deltas don't trigger a relayout.
    if (prevCollapsed.current !== collapsed) {
      prevCollapsed.current = collapsed;
      runLayout(cy, true);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query, focusId, collapsed, ready, model]);

  const fit = () => cyRef.current?.animate({ fit: { eles: cyRef.current.elements().not(".gone").not(".radar-hidden"), padding: 30 } }, { duration: 250 });

  // solver chips → click to focus that solver's whole subtree (causal chain).
  const solvers = model.nodes.filter((n) => n.type === "solver");
  const solverLabel = (n: GraphNode) => {
    const raw = n.label || n.id;
    return raw === "reason" || raw === "solver" ? raw : workerShortLabel(raw);
  };
  const focusedSolver = solvers.find((s) => s.id === focusId);

  return (
    <div className="graphview">
      <div className="graph-canvas" ref={boxRef} />
      <div className="graph-toolbar">
        <div className="graph-toolbar-actions">
          <input
            className="graph-search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("graph.search")}
          />
          <button className="graph-tool-btn" onClick={fit} title={t("graph.fit")} aria-label={t("graph.fit")}>
            <Icon name="crosshair" size={14} />
          </button>
          <button
            className={`graph-tool-btn ${collapsed ? "on" : ""}`}
            onClick={() => setCollapsed((v) => !v)}
            title={collapsed ? t("graph.expand") : t("graph.collapse")}
            aria-label={collapsed ? t("graph.expand") : t("graph.collapse")}
            aria-pressed={collapsed}
          >
            <Icon name={collapsed ? "rows" : "layers"} size={14} />
          </button>
          {focusId && (
            <button className="graph-tool-btn" onClick={() => setFocusId(null)} title={t("graph.clearFocus")} aria-label={t("graph.clearFocus")}>
              <Icon name="xCircle" size={14} />
            </button>
          )}
        </div>
        <div className="graph-toolbar-meta">
          {solvers.length > 1 ? (
            <ChipFilterBar
              id="graph-solver-chips"
              className="graph-solver-filter"
              variant="floating"
              label={t("graph.solvers")}
              summary={focusedSolver ? solverLabel(focusedSolver) : t("graph.solversAll")}
              title={t("graph.solversTitle")}
              expandTitle={t("graph.solversExpand")}
              collapseTitle={t("graph.solversCollapse")}
            >
              {solvers.map((s) => (
                <button key={s.id} className={`graph-solver-chip ${focusId === s.id ? "on" : ""}`}
                  onClick={() => setFocusId((cur) => (cur === s.id ? null : s.id))} aria-pressed={focusId === s.id} title={s.label}>{solverLabel(s)}</button>
              ))}
            </ChipFilterBar>
          ) : <span className="graph-solver-empty" aria-hidden="true" />}
          <span className="legend">
            <span className="legend-item"><i className="lg" style={{ background: NODE_STYLE.fact.bg }} /> {t("legend.verified")}</span>
            <span className="legend-item"><i className="lg" style={{ background: NODE_STYLE.candidate.bg }} /> {t("legend.candidate")}</span>
            <span className="legend-item"><i className="lg" style={{ background: NODE_STYLE.intent.bg }} /> {t("legend.intent")}</span>
            <span className="legend-item"><i className="lg" style={{ background: NODE_STYLE.dead_end.bg }} /> {t("legend.deadend")}</span>
            <span className="legend-item"><i className="lg" style={{ background: NODE_STYLE.flag.bg }} /> {t("legend.flag")}</span>
          </span>
        </div>
      </div>
    </div>
  );
}
