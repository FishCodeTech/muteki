"use client";

import {
  memo,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";
import { SpanIcon, TraceGlyph } from "./traceIcons";
import { useCopy, useTimelineEnv } from "./traceCopy";
import {
  formatCost,
  formatDuration,
  formatTick,
  formatTokens,
  isContainerKind,
  isLeafKind,
  sameNodeState,
  ticksFor,
  toRealTime,
  type AxisTick,
  type TraceEventSpec,
  type TraceNode,
  type TraceSnapshot,
} from "./traceModel";
import {
  ClockContext,
  SpanDuration,
  filterActive,
  nodeMatches,
  spanColor,
  usePresence,
  type FilterState,
} from "./traceShared";

export interface HoverState {
  spanId: string | null;
  time: number | null;
  source: "graph" | "ledger" | "scrubber";
}

export interface TimeWindow {
  start: number;
  range: number;
}

const nodeKey = (node: TraceNode) => node.spec.id;
const ROWS_PAD_TOP = 4;
/** Matches --rt-row and .rt-rows padding in runtimeLab.css. */
const ROW_HEIGHT = 30;
const ROWS_PAD = 14;
/** Above this many visible rows only those near the viewport mount; dense runs already skip height animations. */
const WINDOW_MIN_ROWS = 120;
const OVERSCAN_ROWS = 12;
const tickKey = (tick: AxisTick) => String(tick.value);

const varStyle = (vars: Record<string, string | number>) => vars as CSSProperties;

function leafDescendants(node: TraceNode, out: TraceNode[] = []): TraceNode[] {
  for (const child of node.children) {
    if (isLeafKind(child.spec.kind)) out.push(child);
    leafDescendants(child, out);
  }
  return out;
}

function countDescendants(node: TraceNode): number {
  let count = 0;
  for (const child of node.children) count += 1 + countDescendants(child);
  return count;
}

interface RowProps {
  node: TraceNode;
  leaving: boolean;
  leaveDelay: number;
  enterDelay: number;
  hidden: boolean;
  folded: boolean;
  selected: boolean;
  inScope: boolean;
  hovered: boolean;
  dim: boolean;
  slowest: boolean;
  pinTs: number | null;
  /** Changes when a folded row's child marks would change. */
  foldSig: string;
  focused: boolean;
  view: TimeWindow;
  onRowClick: (node: TraceNode) => void;
  onRowDoubleClick: (node: TraceNode) => void;
  onChevron: (node: TraceNode) => void;
  onEnter: (node: TraceNode, event: ReactMouseEvent) => void;
  onLeave: () => void;
  onKey: (event: ReactKeyboardEvent, node: TraceNode) => void;
  onFocusRow: (node: TraceNode) => void;
}

function rowPropsEqual(a: RowProps, b: RowProps): boolean {
  for (const key in b) {
    if (key !== "node" && a[key as keyof RowProps] !== b[key as keyof RowProps]) return false;
  }
  return sameNodeState(a.node, b.node);
}

const TraceRow = memo(function TraceRow(props: RowProps) {
  const { node, folded, view } = props;
  const { spec } = node;
  const copy = useCopy();
  const [enterDelay] = useState(props.enterDelay);
  const foldable = node.children.length > 0;
  const kind = isContainerKind(spec.kind) ? "agent" : spec.kind;
  const point = !!spec.point;
  const duration = node.end != null ? node.end - spec.start : point ? 0 : null;
  const parent = isContainerKind(spec.kind);
  // An instant on a container says nothing on its own; an untimed call still says "untimed".
  const showTag = !point || isLeafKind(spec.kind);
  let side: "after" | "inside" | "before" | "live" = "live";
  if (point) side = "after";
  else if (duration != null) {
    const startFraction = (spec.start - view.start) / view.range;
    const endFraction = (spec.start + duration - view.start) / view.range;
    const visible = Math.min(endFraction, 1) - Math.max(startFraction, 0);
    // Folded rows carry child marks inside the bar, so their tag prefers outside it.
    if (endFraction < 0.8) side = "after";
    else if (startFraction > 0.12 && (folded || visible <= 0.22)) side = "before";
    else side = "inside";
  }
  const leaves = folded ? leafDescendants(node) : [];
  const clock = useContext(ClockContext);
  // Live elements get --now from the frame loop; seed it so the first frame is already in place.
  const nowSeed = clock ? clock.cursor() : 0;
  const live = (running: boolean) => (running ? { "data-clock": "now" } : undefined);
  const label = `${spec.name}, ${copy.kind[spec.kind]}, ${copy.status[node.status]}`;
  return (
    <div
      className="rt-row"
      role="treeitem"
      aria-level={node.depth + 1}
      aria-selected={props.selected}
      aria-expanded={foldable ? !folded : undefined}
      aria-label={label}
      aria-hidden={props.hidden || props.leaving || undefined}
      tabIndex={props.focused && !props.hidden ? 0 : -1}
      data-span={spec.id}
      data-kind={kind}
      data-status={node.status}
      data-parent={parent || undefined}
      data-folded={folded || undefined}
      data-hidden={props.hidden || undefined}
      data-leaving={props.leaving || undefined}
      data-selected={props.selected || undefined}
      data-scope={props.inScope || undefined}
      data-hover={props.hovered || undefined}
      data-dim={props.dim || undefined}
      style={{ animationDelay: props.leaving ? `${props.leaveDelay}ms` : `${enterDelay}ms` }}
      onClick={() => props.onRowClick(node)}
      onDoubleClick={() => props.onRowDoubleClick(node)}
      onMouseEnter={(event) => props.onEnter(node, event)}
      onMouseLeave={props.onLeave}
      onKeyDown={(event) => props.onKey(event, node)}
      onFocus={() => props.onFocusRow(node)}
    >
      <div className="rt-name">
        <span className="rt-guides" style={{ width: node.depth * 14 }} />
        {foldable ? (
          <button
            type="button"
            className="rt-chev"
            tabIndex={-1}
            aria-label={folded ? copy.expandNode(spec.name) : copy.foldNode(spec.name)}
            onClick={(event) => {
              event.stopPropagation();
              props.onChevron(node);
            }}
          >
            <TraceGlyph name="chevron" size={12} />
          </button>
        ) : (
          <span className="rt-chev" data-leaf="" />
        )}
        <SpanIcon node={node} />
        <span className="rt-label">
          <span className="rt-title">{spec.name}</span>
          {spec.retryOf && <span className="rt-retry">{copy.retry}</span>}
          {spec.detail && <span className="rt-detail">{spec.detail}</span>}
        </span>
        {folded && <span className="rt-more">+{countDescendants(node)}</span>}
        <span className="rt-state" data-status={node.status}>
          {node.status === "running" ? (
            <svg className="rt-spin" width="12" height="12" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <circle cx="12" cy="12" r="8" stroke="currentColor" strokeOpacity=".22" strokeWidth="3" />
              <path d="M20 12a8 8 0 0 0-8-8" stroke="currentColor" strokeWidth="3" strokeLinecap="round" />
            </svg>
          ) : node.status === "failed" ? (
            <TraceGlyph name="alert" size={12} />
          ) : null}
        </span>
        <span className="rt-meta">
          <SpanDuration node={node} />
          {node.cost > 0 && <span className="rt-meta-cost"> · {formatCost(node.cost)}</span>}
        </span>
      </div>
      <div className="rt-track">
        <span
          className="rt-bar"
          data-kind={kind}
          data-status={node.status}
          data-point={point || undefined}
          data-estimated={spec.estimated || undefined}
          {...live(node.status === "running")}
          style={varStyle({ "--s": spec.start, "--d": duration ?? 0, ...(node.status === "running" && { "--now": nowSeed }) })}
        />
        {leaves.map((leaf) => (
          <span
            key={leaf.spec.id}
            className="rt-mark"
            data-kind={leaf.spec.kind}
            data-status={leaf.status}
            data-point={leaf.spec.point || undefined}
            data-estimated={leaf.spec.estimated || undefined}
            {...live(leaf.status === "running")}
            style={varStyle({ "--s": leaf.spec.start, "--d": leaf.end != null ? leaf.end - leaf.spec.start : 0, ...(leaf.status === "running" && { "--now": nowSeed }) })}
          />
        ))}
        {props.pinTs != null && <span className="rt-pin" style={varStyle({ "--t": props.pinTs })} />}
        {showTag && (
          <span className="rt-tag" data-side={side} data-point={point || undefined} {...live(side === "live")} style={varStyle({ "--s": spec.start, "--d": duration ?? 0, ...(side === "live" && { "--now": nowSeed }) })}>
            <span className={node.status === "running" ? "rt-live-num" : undefined}>
              <SpanDuration node={node} />
              {node.cost > 0 && ` · ${formatCost(node.cost)}`}
            </span>
            {props.slowest && <b className="rt-slow">{copy.slowest}</b>}
          </span>
        )}
      </div>
    </div>
  );
}, rowPropsEqual);

function foldSignature(node: TraceNode): string {
  let count = 0;
  let open = 0;
  let failed = 0;
  for (const leaf of leafDescendants(node)) {
    count++;
    if (leaf.status === "running") open++;
    else if (leaf.status === "failed") failed++;
  }
  return `${count}:${open}:${failed}`;
}

export interface WaterfallProps {
  snapshot: TraceSnapshot;
  view: TimeWindow;
  getWindow: () => TimeWindow;
  isFolded: (node: TraceNode) => boolean;
  scopeId: string | null;
  selectedEvent: TraceEventSpec | null;
  hover: HoverState | null;
  filter: FilterState;
  band: { from: number; to: number } | null;
  zoomLabel: string | null;
  pinNonce: number;
  reveal: { id: string; nonce: number } | null;
  onSelectSpan: (id: string | null) => void;
  onToggleFold: (node: TraceNode) => void;
  onHover: (hover: HoverState | null) => void;
  onZoomSpan: (node: TraceNode) => void;
  onResetZoom: () => void;
  onSeek: (time: number, phase: "start" | "move" | "end") => void;
  onWheelZoom: (anchor: number, factor: number) => void;
  onWheelPan: (fraction: number) => void;
}

/** How long a newly hidden row stays mounted so its fold animation can finish. */
const HIDE_LINGER_MS = 450;

export function TraceWaterfall(props: WaterfallProps) {
  const { snapshot, view, isFolded, scopeId, selectedEvent, hover, filter, band } = props;
  const { copy, gaps } = useTimelineEnv();
  const latest = useRef(props);
  latest.current = props;
  const wfRef = useRef<HTMLDivElement>(null);
  const bodyRef = useRef<HTMLDivElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);
  const rowsRef = useRef<HTMLDivElement>(null);
  const layerRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const programmaticTop = useRef<number | null>(null);
  const [focusId, setFocusId] = useState<string | null>(null);
  const [tip, setTip] = useState<{ id: string; x: number; y: number } | null>(null);
  const tipTimer = useRef<number | null>(null);
  const [viewport, setViewport] = useState({ top: 0, height: 0 });
  const viewportFrame = useRef(0);
  const pendingFocus = useRef<string | null>(null);

  const readViewport = useCallback(() => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const top = scroller.scrollTop;
    const height = scroller.clientHeight;
    setViewport((current) => (current.top === top && current.height === height ? current : { top, height }));
  }, []);
  const scheduleViewport = useCallback(() => {
    if (viewportFrame.current) return;
    viewportFrame.current = requestAnimationFrame(() => {
      viewportFrame.current = 0;
      readViewport();
    });
  }, [readViewport]);
  useEffect(() => () => cancelAnimationFrame(viewportFrame.current), []);

  const present = usePresence(snapshot.order, nodeKey, 320);
  const visibleGaps = useMemo(
    () => gaps.filter((gap) => gap.at + gap.width >= view.start && gap.at <= view.start + view.range),
    [gaps, view.start, view.range],
  );
  const [trackWidth, setTrackWidth] = useState(0);
  const ticks = usePresence(
    useMemo(() => {
      // A tick label next to an idle break would collide with the break's own label.
      const clearance = (gap: (typeof visibleGaps)[number]) => {
        if (!trackWidth) return view.range * 0.07;
        let labelPx = 14;
        for (const char of copy.idle(formatDuration(gap.end - gap.start))) labelPx += char.charCodeAt(0) > 0x2e80 ? 10 : 6;
        return ((labelPx / 2 + 30) / trackWidth) * view.range;
      };
      return ticksFor(view.start, view.range, gaps).filter((tick) => !visibleGaps.some((gap) => Math.abs(tick.at - (gap.at + gap.width / 2)) < clearance(gap)));
    }, [view.start, view.range, gaps, visibleGaps, trackWidth, copy]),
    tickKey,
    420,
  );

  const prevKeys = useRef<Set<string>>(new Set());
  useEffect(() => {
    prevKeys.current = new Set(present.filter((entry) => !entry.leaving).map((entry) => entry.key));
  }, [present]);
  /** Rows that were on screen last commit; only those have a fold animation worth lingering for. */
  const prevShown = useRef<Set<string>>(new Set());

  const hiddenIds = useMemo(() => {
    const hidden = new Set<string>();
    for (const node of snapshot.order) {
      const parent = node.parent;
      if (parent && (hidden.has(parent.spec.id) || isFolded(parent))) hidden.add(node.spec.id);
    }
    return hidden;
  }, [snapshot, isFolded]);

  const visibleAncestor = useCallback(
    (id: string | null | undefined): TraceNode | null => {
      let node = id ? snapshot.byId.get(id) ?? null : null;
      while (node && hiddenIds.has(node.spec.id)) node = node.parent;
      return node;
    },
    [snapshot, hiddenIds],
  );

  const keep = useMemo(() => {
    if (!filterActive(filter)) return null;
    const set = new Set<string>();
    for (const node of snapshot.order) {
      if (!nodeMatches(node, filter)) continue;
      for (let current: TraceNode | null = node; current && !set.has(current.spec.id); current = current.parent) set.add(current.spec.id);
    }
    return set;
  }, [snapshot, filter]);

  const scopeNode = scopeId ? snapshot.byId.get(scopeId) ?? null : null;
  const hoverTarget = visibleAncestor(hover?.spanId)?.spec.id ?? null;
  const pinTarget = selectedEvent ? visibleAncestor(selectedEvent.spanId)?.spec.id ?? null : null;
  const guideTime = hover && hover.source !== "graph" ? hover.time : null;
  const visibleRows = present.filter((entry) => !entry.leaving && !hiddenIds.has(entry.key));
  const visibleRowsRef = useRef(visibleRows);
  visibleRowsRef.current = visibleRows;
  const effectiveFocus = focusId && visibleRows.some((entry) => entry.key === focusId) ? focusId : visibleRows[0]?.key ?? null;

  const scrollToRow = useCallback((id: string, behavior: ScrollBehavior = "smooth") => {
    const scroller = scrollRef.current;
    const index = visibleRowsRef.current.findIndex((entry) => entry.key === id);
    if (!scroller || index < 0) return;
    // Rows are a fixed height; measuring offsetTop would be wrong while unfolded rows animate open.
    const top = ROWS_PAD_TOP + index * ROW_HEIGHT;
    const bottom = top + ROW_HEIGHT;
    if (top >= scroller.scrollTop + 8 && bottom <= scroller.scrollTop + scroller.clientHeight - 8) return;
    const target = Math.max(0, top - scroller.clientHeight * 0.35);
    programmaticTop.current = target;
    pinnedRef.current = false;
    scroller.scrollTo({ top: target, behavior });
  }, []);

  useEffect(() => {
    const scroller = scrollRef.current;
    const rows = rowsRef.current;
    if (!scroller || !rows) return;
    const stick = () => {
      scheduleViewport();
      if (!pinnedRef.current) return;
      const target = scroller.scrollHeight - scroller.clientHeight;
      if (Math.abs(scroller.scrollTop - target) < 1) return;
      programmaticTop.current = target;
      scroller.scrollTop = target;
    };
    const observer = new ResizeObserver(stick);
    observer.observe(rows);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [scheduleViewport]);

  useEffect(() => {
    const id = pendingFocus.current;
    if (!id) return;
    const row = scrollRef.current?.querySelector<HTMLElement>(`[data-span="${CSS.escape(id)}"]`);
    if (!row) return;
    pendingFocus.current = null;
    row.focus({ preventScroll: true });
  });

  useEffect(() => {
    pinnedRef.current = true;
    const scroller = scrollRef.current;
    if (scroller) {
      programmaticTop.current = scroller.scrollHeight - scroller.clientHeight;
      scroller.scrollTo({ top: scroller.scrollHeight, behavior: "smooth" });
    }
  }, [props.pinNonce]);

  useEffect(() => {
    if (!props.reveal) return;
    const id = props.reveal.id;
    pinnedRef.current = false;
    // Unfolding happens in the same commit; wait a frame so the row has its height.
    const frame = requestAnimationFrame(() => scrollToRow(visibleAncestor(id)?.spec.id ?? id));
    return () => cancelAnimationFrame(frame);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.reveal?.nonce]);

  useEffect(() => {
    const wf = wfRef.current;
    const scroller = scrollRef.current;
    if (!wf || !scroller) return;
    const measure = () => {
      wf.style.setProperty("--rt-sb", `${scroller.offsetWidth - scroller.clientWidth}px`);
      setTrackWidth(layerRef.current?.clientWidth ?? 0);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(scroller);
    if (layerRef.current) observer.observe(layerRef.current);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const wheel = (event: WheelEvent) => {
      const layer = layerRef.current?.getBoundingClientRect();
      if (!layer) return;
      if (event.ctrlKey || event.metaKey) {
        event.preventDefault();
        const anchor = Math.max(0, Math.min(1, (event.clientX - layer.left) / layer.width));
        latest.current.onWheelZoom(anchor, Math.exp(event.deltaY * 0.004));
        return;
      }
      if (event.shiftKey || Math.abs(event.deltaX) > Math.abs(event.deltaY)) {
        const delta = event.shiftKey && !event.deltaX ? event.deltaY : event.deltaX;
        if (!delta) return;
        event.preventDefault();
        latest.current.onWheelPan(delta / layer.width);
      }
    };
    scroller.addEventListener("wheel", wheel, { passive: false });
    return () => scroller.removeEventListener("wheel", wheel);
  }, []);

  const onScroll = () => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    scheduleViewport();
    if (programmaticTop.current != null && Math.abs(scroller.scrollTop - programmaticTop.current) < 2) {
      programmaticTop.current = null;
      return;
    }
    if (programmaticTop.current != null) return;
    pinnedRef.current = scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 6;
    setTip(null);
  };

  const onRowClick = useCallback((node: TraceNode) => {
    const current = latest.current;
    setFocusId(node.spec.id);
    const leaf = !isContainerKind(node.spec.kind);
    current.onSelectSpan(!leaf && current.scopeId === node.spec.id ? null : node.spec.id);
  }, []);
  // Unfolding grows the list; staying pinned to the tail would scroll the row the operator just opened out of view.
  const onRowDoubleClick = useCallback((node: TraceNode) => {
    pinnedRef.current = false;
    latest.current.onZoomSpan(node);
  }, []);
  const onChevron = useCallback((node: TraceNode) => {
    setFocusId(node.spec.id);
    pinnedRef.current = false;
    latest.current.onToggleFold(node);
  }, []);
  const onEnter = useCallback((node: TraceNode, event: ReactMouseEvent) => {
    latest.current.onHover({ spanId: node.spec.id, time: null, source: "graph" });
    if (tipTimer.current) window.clearTimeout(tipTimer.current);
    const x = event.clientX;
    const row = event.currentTarget as HTMLElement;
    tipTimer.current = window.setTimeout(() => {
      const body = bodyRef.current?.getBoundingClientRect();
      const rect = row.getBoundingClientRect();
      if (!body) return;
      setTip({ id: node.spec.id, x: Math.min(body.width - 250, Math.max(8, x - body.left - 20)), y: rect.bottom - body.top + 4 });
    }, 420);
  }, []);
  const onLeave = useCallback(() => {
    latest.current.onHover(null);
    if (tipTimer.current) window.clearTimeout(tipTimer.current);
    setTip(null);
  }, []);
  const onFocusRow = useCallback((node: TraceNode) => setFocusId(node.spec.id), []);
  const onKey = useCallback(
    (event: ReactKeyboardEvent, node: TraceNode) => {
      const current = latest.current;
      const list = visibleRowsRef.current;
      const index = list.findIndex((entry) => entry.key === node.spec.id);
      let target = index;
      const folded = current.isFolded(node);
      switch (event.key) {
        case "ArrowDown": target = index + 1; break;
        case "ArrowUp": target = index - 1; break;
        case "Home": target = 0; break;
        case "End": target = list.length - 1; break;
        case "ArrowRight":
          if (node.children.length && folded) {
            pinnedRef.current = false;
            current.onToggleFold(node);
          }
          else if (node.children.length) target = index + 1;
          break;
        case "ArrowLeft":
          if (node.children.length && !folded) current.onToggleFold(node);
          else if (node.parent) target = list.findIndex((entry) => entry.key === node.parent?.spec.id);
          break;
        case "Enter":
        case " ": {
          const leaf = !isContainerKind(node.spec.kind);
          current.onSelectSpan(!leaf && current.scopeId === node.spec.id ? null : node.spec.id);
          break;
        }
        case "z":
          current.onZoomSpan(node);
          break;
        default:
          return;
      }
      event.preventDefault();
      event.stopPropagation();
      const next = list[Math.max(0, Math.min(list.length - 1, target))];
      if (!next || next.key === node.spec.id) return;
      setFocusId(next.key);
      // The target row may sit outside the mounted window until the scroll lands.
      pendingFocus.current = next.key;
      scrollToRow(next.key, "auto");
    },
    [scrollToRow],
  );

  const rulerDrag = useRef(false);
  const timeAt = (clientX: number) => {
    const layer = layerRef.current?.getBoundingClientRect();
    const win = props.getWindow();
    if (!layer) return 0;
    return win.start + Math.max(0, Math.min(1, (clientX - layer.left) / layer.width)) * win.range;
  };
  const rulerDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    rulerDrag.current = true;
    event.currentTarget.setPointerCapture(event.pointerId);
    props.onSeek(timeAt(event.clientX), "start");
  };
  const rulerMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (rulerDrag.current) props.onSeek(timeAt(event.clientX), "move");
    else props.onHover({ spanId: null, time: timeAt(event.clientX), source: "scrubber" });
  };
  const rulerUp = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!rulerDrag.current) return;
    rulerDrag.current = false;
    props.onSeek(timeAt(event.clientX), "end");
  };

  let leavingBelow = 0;
  const leaveDelays = new Map<string, number>();
  for (let index = present.length - 1; index >= 0; index--) {
    if (present[index].leaving) leaveDelays.set(present[index].key, Math.min(leavingBelow++, 4) * 22);
  }
  let freshIndex = 0;
  const tipNode = tip ? snapshot.byId.get(tip.id) ?? null : null;
  const windowed = visibleRows.length > WINDOW_MIN_ROWS;
  const viewHeight = viewport.height || scrollRef.current?.clientHeight || 720;
  const firstRow = windowed ? Math.max(0, Math.floor((viewport.top - ROWS_PAD_TOP) / ROW_HEIGHT) - OVERSCAN_ROWS) : 0;
  const lastRow = windowed ? Math.ceil((viewport.top + viewHeight) / ROW_HEIGHT) + OVERSCAN_ROWS : Infinity;
  const skippedHeight = Math.min(firstRow, visibleRows.length) * ROW_HEIGHT;
  let rowIndex = 0;

  // Folded subtrees of long runs hold most rows; a row that has been hidden for a while is not mounted.
  const hiddenAt = useRef<Map<string, number> | null>(null);
  const [, setLingerTick] = useState(0);
  const clockNow = typeof performance === "undefined" ? 0 : performance.now();
  if (!hiddenAt.current) hiddenAt.current = new Map([...hiddenIds].map((id) => [id, -Infinity]));
  const lingering = hiddenAt.current;
  for (const id of lingering.keys()) if (!hiddenIds.has(id)) lingering.delete(id);
  let lingerUntil = 0;
  for (const id of hiddenIds) {
    // Jumping ahead can reveal and fold thousands of rows at once; mounting them just to animate out stalls the page.
    const since = lingering.get(id) ?? (prevShown.current.has(id) ? clockNow : -Infinity);
    if (!lingering.has(id)) lingering.set(id, since);
    if (clockNow - since < HIDE_LINGER_MS) lingerUntil = Math.max(lingerUntil, since + HIDE_LINGER_MS);
  }
  useEffect(() => {
    if (!lingerUntil) return;
    const timer = window.setTimeout(() => setLingerTick((n) => n + 1), Math.max(16, lingerUntil - performance.now() + 16));
    return () => window.clearTimeout(timer);
  }, [lingerUntil]);
  const mounted = (id: string) => !hiddenIds.has(id) || clockNow - (lingering.get(id) ?? clockNow) < HIDE_LINGER_MS;
  useEffect(() => {
    prevShown.current = new Set(visibleRows.map((entry) => entry.key));
  });

  return (
    <div ref={wfRef} className="rt-wf" data-clock="view">
      <div className="rt-ruler">
        <div className="rt-ruler-name">
          {props.zoomLabel ? (
            <span className="rt-crumbs">
              <button type="button" onClick={props.onResetZoom}>{copy.wholeRun}</button>
              <TraceGlyph name="chevron" size={10} className="rt-crumb-sep" />
              <span title={props.zoomLabel}>{props.zoomLabel}</span>
            </span>
          ) : (
            <span>{copy.unitColumn}</span>
          )}
        </div>
        <div
          className="rt-ruler-track"
          data-clock="now"
          role="slider"
          aria-label={copy.seekRuler}
          aria-valuemin={0}
          aria-valuemax={Math.round(view.start + view.range)}
          aria-valuenow={0}
          onPointerDown={rulerDown}
          onPointerMove={rulerMove}
          onPointerUp={rulerUp}
          onPointerCancel={rulerUp}
          onPointerLeave={() => !rulerDrag.current && props.onHover(null)}
          onDoubleClick={props.onResetZoom}
        >
          {ticks.map((tick) => (
            <span key={tick.key} className="rt-tick-label" data-leaving={tick.leaving || undefined} style={varStyle({ "--t": tick.item.at })}>
              {formatTick(tick.item.value)}
            </span>
          ))}
          {visibleGaps.map((gap) => (
            <span key={gap.start} className="rt-gap-label" style={varStyle({ "--a": gap.at, "--b": gap.at + gap.width })}>
              {copy.idle(formatDuration(gap.end - gap.start))}
            </span>
          ))}
          <span className="rt-ruler-now" />
        </div>
      </div>
      <div ref={bodyRef} className="rt-wf-body">
        <div ref={layerRef} className="rt-layer" data-clock="now" aria-hidden="true">
          {ticks.map((tick) => (
            <span key={tick.key} className="rt-tick" data-zero={tick.item.at === 0 || undefined} data-leaving={tick.leaving || undefined} style={varStyle({ "--t": tick.item.at })} />
          ))}
          {band && <span className="rt-band" style={varStyle({ "--a": band.from, "--b": band.to })} />}
          {selectedEvent && <span className="rt-evline" style={varStyle({ "--t": selectedEvent.ts })} />}
          {guideTime != null && <span className="rt-guide" style={varStyle({ "--t": guideTime })} />}
          <span className="rt-now" />
        </div>
        {visibleGaps.length > 0 && (
          <div className="rt-layer rt-gaps" aria-hidden="true">
            {visibleGaps.map((gap) => (
              <span key={gap.start} className="rt-gap" style={varStyle({ "--a": gap.at, "--b": gap.at + gap.width })} />
            ))}
          </div>
        )}
        <div ref={scrollRef} className="rt-wf-scroll" role="tree" aria-label={copy.graph} onScroll={onScroll}>
          <div ref={rowsRef} className="rt-rows" style={{ minHeight: visibleRows.length * ROW_HEIGHT + ROWS_PAD }}>
            {skippedHeight > 0 && <div aria-hidden="true" style={{ height: skippedHeight }} />}
            {present.map((entry) => {
              const node = entry.item;
              const id = entry.key;
              const takesSpace = !entry.leaving && !hiddenIds.has(id);
              const index = takesSpace ? rowIndex++ : rowIndex;
              if (windowed && (index < firstRow || index > lastRow)) return null;
              if (!entry.leaving && !mounted(id)) return null;
              const fresh = !entry.leaving && !prevKeys.current.has(id);
              const enterDelay = fresh && prevKeys.current.size > 0 ? Math.min(freshIndex++, 10) * 34 : 0;
              const folded = node.children.length > 0 && isFolded(node);
              return (
                <TraceRow
                  key={id}
                  node={node}
                  leaving={entry.leaving}
                  leaveDelay={leaveDelays.get(id) ?? 0}
                  enterDelay={enterDelay}
                  hidden={hiddenIds.has(id)}
                  folded={folded}
                  foldSig={folded ? foldSignature(node) : ""}
                  selected={scopeId === id || selectedEvent?.spanId === id}
                  inScope={!!scopeNode && scopeId !== id && isWithin(scopeNode, node)}
                  hovered={hoverTarget === id}
                  dim={!!keep && !keep.has(id)}
                  slowest={snapshot.slowestId === id}
                  pinTs={pinTarget === id && selectedEvent ? selectedEvent.ts : null}
                  focused={effectiveFocus === id}
                  view={view}
                  onRowClick={onRowClick}
                  onRowDoubleClick={onRowDoubleClick}
                  onChevron={onChevron}
                  onEnter={onEnter}
                  onLeave={onLeave}
                  onKey={onKey}
                  onFocusRow={onFocusRow}
                />
              );
            })}
          </div>
        </div>
        {tipNode && tip && (
          <div className="rt-tip" style={{ left: tip.x, top: tip.y }} role="tooltip">
            <div className="rt-tip-head">
              <SpanIcon node={tipNode} size={18} />
              <b>{tipNode.spec.name}</b>
              <span className="rt-pill" data-status={tipNode.status}>{copy.status[tipNode.status]}</span>
            </div>
            {tipNode.spec.detail && <p className="rt-tip-detail">{tipNode.spec.detail}</p>}
            <dl>
              <div><dt>{copy.statDuration}</dt><dd><SpanDuration node={tipNode} /></dd></div>
              <div><dt>{copy.tipStart}</dt><dd>+{formatDuration(toRealTime(gaps, tipNode.spec.start))}</dd></div>
              {tipNode.tokens > 0 && <div><dt>{copy.statTokens}</dt><dd>{formatTokens(tipNode.tokens)}</dd></div>}
              {tipNode.cost > 0 && <div><dt>{copy.statCost}</dt><dd>{formatCost(tipNode.cost)}</dd></div>}
            </dl>
            {tipNode.status === "failed" && tipNode.spec.error && <p className="rt-tip-error">{tipNode.spec.error}</p>}
            {tipNode.spec.estimated && <p className="rt-tip-note">{copy.estimatedHint}</p>}
            <small>{copy.tipHint}</small>
          </div>
        )}
      </div>
    </div>
  );
}

function isWithin(ancestor: TraceNode, node: TraceNode): boolean {
  for (let current: TraceNode | null = node.parent; current; current = current.parent) if (current === ancestor) return true;
  return false;
}

export function TraceOverview({
  snapshot,
  scopeId,
  band,
  onSelectSpan,
  onExpand,
}: {
  snapshot: TraceSnapshot;
  scopeId: string | null;
  band: { from: number; to: number } | null;
  onSelectSpan: (id: string | null) => void;
  onExpand: () => void;
}) {
  const copy = useCopy();
  const root = snapshot.root;
  if (!root) return <div className="rt-overview rt-overview-empty">{copy.waitingStart}</div>;
  const lanes: { node: TraceNode; leaves: TraceNode[] }[] = [
    { node: root, leaves: root.children.filter((child) => isLeafKind(child.spec.kind)) },
    ...root.children.filter((child) => isContainerKind(child.spec.kind)).map((node) => ({ node, leaves: leafDescendants(node) })),
  ];
  return (
    <div className="rt-overview" data-clock="all" onDoubleClick={onExpand}>
      <div className="rt-overview-layer" aria-hidden="true">
        {band && <span className="rt-band" style={varStyle({ "--a": band.from, "--b": band.to })} />}
        <span className="rt-now" />
      </div>
      {lanes.map(({ node, leaves }) => (
        <button
          type="button"
          key={node.spec.id}
          className="rt-lane"
          data-selected={scopeId === node.spec.id || undefined}
          onClick={() => onSelectSpan(scopeId === node.spec.id ? null : node.spec.id)}
          title={node.spec.name}
          style={varStyle({ "--wc": spanColor(node) })}
        >
          <span className="rt-lane-name">{node.spec.name}</span>
          <span className="rt-lane-track">
            <span className="rt-lane-span" data-status={node.status} data-point={node.spec.point || undefined} style={varStyle({ "--s": node.spec.start, "--d": node.end != null ? node.end - node.spec.start : 0 })} />
            {leaves.map((leaf) => (
              <span
                key={leaf.spec.id}
                className="rt-mark"
                data-kind={leaf.spec.kind}
                data-status={leaf.status}
                data-point={leaf.spec.point || undefined}
                style={varStyle({ "--s": leaf.spec.start, "--d": leaf.end != null ? leaf.end - leaf.spec.start : 0 })}
              />
            ))}
          </span>
        </button>
      ))}
    </div>
  );
}
