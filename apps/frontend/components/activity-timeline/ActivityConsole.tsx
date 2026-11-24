"use client";

import "@/app/styles/activity-timeline.css";
import {
  startTransition,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { usePauseOffscreen } from "@/lib/usePauseOffscreen";
import { usePrefersReducedMotion } from "@/lib/usePrefersReducedMotion";
import { TraceGlyph } from "./traceIcons";
import { TIMELINE_COPY, TimelineEnvContext, useTimelineEnv, type TimelineCopy, type TimelineLang } from "./traceCopy";
import { TraceLedger } from "./TraceLedger";
import { TracePlayback, type PlaybackControls } from "./TracePlayback";
import { TraceOverview, TraceWaterfall, type HoverState, type TimeWindow } from "./TraceWaterfall";
import {
  epochAt,
  formatCost,
  formatDuration,
  formatTokens,
  isAncestor,
  isContainerKind,
  niceRange,
  snapshotAt,
  spanTimeAt,
  specEdge,
  toAxisTime,
  toRealTime,
  type TraceEventSpec,
  type TraceNode,
  type TraceScenario,
} from "./traceModel";
import {
  ClockContext,
  EMPTY_FILTER,
  clamp,
  eventMatches,
  filterActive,
  kindOf,
  useClockValue,
  type ClockStore,
  type FilterState,
  type KindFilter,
} from "./traceShared";

export type ConsoleLayout = "stack" | "side";

const DENSE_GLIDE_LIMIT = 400;

export type ConsoleClock =
  | { mode: "live"; origin?: number; cap?: number }
  | { mode: "replay"; startAt?: "begin" | "end" };

export interface ActivityConsoleProps {
  scenario: TraceScenario;
  clock: ConsoleClock;
  resetKey: string;
  autoFold: boolean;
  initialLayout?: ConsoleLayout;
  reducedMotion?: boolean;
  lang?: TimelineLang;
  timeOrigin?: number | null;
  task?: string;
  summary?: { cost?: ReactNode; tokens?: number | null; tokenLabel?: ReactNode };
  focus?: { spanId: string; nonce: number } | null;
  className?: string;
  compact?: boolean;
}

function RunDuration({ root }: { root: TraceNode | null }) {
  const { gaps } = useTimelineEnv();
  const live = root?.status === "running";
  const startReal = root ? toRealTime(gaps, root.spec.start) : 0;
  const liveText = useClockValue(
    (clock) => {
      const end = Math.floor(clock.cursor() / 100) * 100;
      return formatDuration(Math.max(0, toRealTime(gaps, end) - startReal));
    },
    "0ms",
    live,
  );
  if (!root) return <>—</>;
  return <>{live ? liveText : formatDuration(Math.max(0, toRealTime(gaps, root.end ?? root.spec.start) - startReal))}</>;
}

function Stat({ label, children, live, active, tone, onClick, copy }: { label: string; children: ReactNode; live?: boolean; active?: boolean; tone?: "failed"; onClick?: () => void; copy: TimelineCopy }) {
  const body = (
    <>
      <dt>{label}</dt>
      <dd className={live ? "rt-live-num" : undefined}>{children}</dd>
    </>
  );
  return onClick ? (
    <div className="rt-stat" data-tone={tone} data-active={active || undefined}>
      <button type="button" onClick={onClick} aria-pressed={active} title={active ? copy.filterClear(label) : copy.filterOnly(label)}>{body}</button>
    </div>
  ) : (
    <div className="rt-stat" data-tone={tone}>{body}</div>
  );
}

const SIDE_MIN_WIDTH = 720;

const KIND_CHIPS: { id: KindFilter; label: (copy: TimelineCopy) => string }[] = [
  { id: "agent", label: (copy) => copy.chipAgent },
  { id: "llm", label: (copy) => copy.chipLlm },
  { id: "tool", label: (copy) => copy.chipTool },
];

export function ActivityConsole({
  scenario,
  clock: clockSpec,
  resetKey,
  autoFold,
  initialLayout = "stack",
  reducedMotion = false,
  lang = "zh",
  timeOrigin = null,
  task,
  summary,
  focus,
  className,
  compact = false,
}: ActivityConsoleProps) {
  const copy = TIMELINE_COPY[lang];
  const env = useMemo(() => ({ copy, lang, origin: timeOrigin, gaps: scenario.gaps }), [copy, lang, timeOrigin, scenario.gaps]);
  const mode = clockSpec.mode;
  const clockRef = useRef(clockSpec);
  clockRef.current = clockSpec;
  const scenarioRef = useRef(scenario);
  scenarioRef.current = scenario;
  const rootRef = useRef<HTMLDivElement>(null);
  usePauseOffscreen(rootRef);
  const splitRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const engine = useRef({ head: 0, cursor: 0, playing: true, speed: 1, follow: true, liveOrigin: 0 });
  const dirty = useRef(true);
  const wakeClock = useRef<(() => void) | null>(null);
  const [clockTargets] = useState(() => ({
    playhead: new Set<HTMLElement>(),
    axis: new Set<HTMLElement>(),
  }));
  useLayoutEffect(() => {
    wakeClock.current?.();
  }, [scenario]);
  const listeners = useRef(new Set<() => void>());
  const clock = useMemo<ClockStore>(
    () => ({
      cursor: () => engine.current.cursor,
      head: () => engine.current.head,
      subscribe: (listener) => {
        listeners.current.add(listener);
        return () => {
          listeners.current.delete(listener);
        };
      },
      bindTarget: (element) => {
        if (!element) return;
        const playhead = element.dataset.clock === "now";
        const targets = playhead ? clockTargets.playhead : clockTargets.axis;
        targets.add(element);
        if (playhead) element.style.setProperty("--rt-now", engine.current.cursor.toFixed(1));
        dirty.current = true;
        wakeClock.current?.();
        return () => { targets.delete(element); };
      },
    }),
    [clockTargets],
  );
  const [epochTime, setEpochTime] = useState(-1);
  const [controls, setControls] = useState<PlaybackControls>({ playing: true, speed: 1, follow: true });
  const [view, setView] = useState<TimeWindow>({ start: 0, range: 10_000 });
  const viewRef = useRef(view);
  const zoomRef = useRef<TimeWindow | null>(null);
  const [zoomLabel, setZoomLabel] = useState<string | null>(null);
  const glide = useRef<TimeWindow>({ start: 0, range: 10_000 });
  const osReduced = usePrefersReducedMotion();
  const effectiveReduced = reducedMotion || osReduced;
  const reducedRef = useRef(effectiveReduced);
  reducedRef.current = effectiveReduced;
  const denseRef = useRef(false);

  const [scopeId, setScopeId] = useState<string | null>(null);
  const [eventId, setEventId] = useState<string | null>(null);
  const [hover, setHover] = useState<HoverState | null>(null);
  const [filter, setFilter] = useState<FilterState>(EMPTY_FILTER);
  const [band, setBandState] = useState<{ from: number; to: number } | null>(null);
  const [pinNonce, setPinNonce] = useState(0);
  const [reveal, setReveal] = useState<{ id: string; nonce: number } | null>(null);
  const [userFold, setUserFold] = useState<Map<string, boolean>>(() => new Map());
  const [layout, setLayout] = useState<ConsoleLayout>(initialLayout);
  const [graphSize, setGraphSize] = useState({ stack: 0.52, side: 0.58 });
  const [collapsed, setCollapsed] = useState(false);
  const [narrow, setNarrow] = useState(false);

  useEffect(() => {
    const root = rootRef.current;
    if (!root) return;
    const observer = new ResizeObserver(([entry]) => setNarrow(entry.contentRect.width < SIDE_MIN_WIDTH));
    observer.observe(root);
    return () => observer.disconnect();
  }, []);

  const syncControls = useCallback(() => {
    const { playing, speed, follow } = engine.current;
    setControls((current) =>
      current.playing === playing && current.speed === speed && current.follow === follow
        ? current
        : { playing, speed, follow },
    );
    wakeClock.current?.();
  }, []);

  const headNow = useCallback(() => {
    const spec = clockRef.current;
    const duration = scenarioRef.current.duration;
    if (spec.mode === "replay") return duration;
    const wall = toAxisTime(scenarioRef.current.gaps, Date.now() - (spec.origin ?? engine.current.liveOrigin));
    return spec.cap != null ? clamp(wall, 0, spec.cap) : Math.max(duration, wall);
  }, []);

  const lastGaps = useRef(scenario.gaps);
  useEffect(() => {
    const before = lastGaps.current;
    const after = scenario.gaps;
    if (before === after) return;
    lastGaps.current = after;
    const remap = (t: number) => toAxisTime(after, toRealTime(before, t));
    const e = engine.current;
    if (!e.follow) e.cursor = remap(e.cursor);
    const zoom = zoomRef.current;
    if (zoom) {
      const start = remap(zoom.start);
      zoomRef.current = { start, range: Math.max(300, remap(zoom.start + zoom.range) - start) };
    }
  }, [scenario.gaps]);

  const axisRange = useCallback((cursor: number) => {
    const nice = niceRange(Math.max(1, cursor) * 1.04);
    if (clockRef.current.mode === "live") return nice;
    return Math.min(nice, Math.max(1000, scenarioRef.current.duration * 1.04));
  }, []);

  useLayoutEffect(() => {
    const e = engine.current;
    const spec = clockRef.current;
    e.liveOrigin = Date.now();
    e.head = headNow();
    const atEnd = spec.mode === "live" || spec.startAt === "end";
    e.cursor = atEnd ? e.head : 0;
    e.playing = spec.mode === "live" || !atEnd;
    e.follow = spec.mode === "live";
    zoomRef.current = null;
    setZoomLabel(null);
    setEpochTime(-1);
    setScopeId(null);
    setEventId(null);
    setHover(null);
    setUserFold(new Map());
    setBandState(null);
    setPinNonce((n) => n + 1);
    syncControls();
  }, [resetKey, headNow, syncControls]);

  const lastMode = useRef(mode);
  useEffect(() => {
    if (lastMode.current === mode) return;
    lastMode.current = mode;
    const e = engine.current;
    e.head = headNow();
    if (mode === "replay") {
      if (e.follow) e.cursor = e.head;
      e.follow = false;
      e.playing = false;
    } else {
      e.follow = true;
      e.playing = true;
    }
    syncControls();
  }, [mode, headNow, syncControls]);

  useEffect(() => {
    let frame = 0;
    let last = performance.now();
    let lastEpoch = Number.NaN;
    let lastCommit = 0;
    let lastNotify = 0;
    let lastEpochDeferred = false;
    let writtenCursor = "";
    let writtenStart = "";
    let writtenRange = "";
    let notifiedCursor: number | undefined;
    let notifiedHead: number | undefined;
    const tick = (now: number) => {
      const dt = Math.max(0, Math.min(80, now - last));
      last = now;
      const e = engine.current;
      const live = clockRef.current.mode === "live";
      const scenario = scenarioRef.current;
      e.head = headNow();
      if (e.follow) e.cursor = e.head;
      else if (e.playing) {
        e.cursor = Math.min(e.head, e.cursor + dt * e.speed);
        if (e.cursor >= e.head) {
          if (live) e.follow = true;
          else e.playing = false;
          syncControls();
        }
      } else if (e.cursor > e.head) e.cursor = e.head;
      const fast = !e.follow && e.playing && e.speed > 1;
      const deferEpoch = fast && !live;
      const atBoundary = e.cursor === 0 || e.cursor >= e.head;
      const target = zoomRef.current ?? { start: 0, range: axisRange(e.cursor) };
      if (target.start !== viewRef.current.start || target.range !== viewRef.current.range) {
        viewRef.current = target;
        if (deferEpoch && !zoomRef.current && !atBoundary) startTransition(() => setView(target));
        else setView(target);
      }
      const g = glide.current;
      const k = reducedRef.current || denseRef.current || (fast && e.speed >= 10 && !zoomRef.current)
        ? 1
        : 1 - Math.exp(-dt / 110);
      g.start += (target.start - g.start) * k;
      g.range += (target.range - g.range) * k;
      if (Math.abs(target.range - g.range) < target.range * 0.002) g.range = target.range;
      if (Math.abs(target.start - g.start) < target.range * 0.002) g.start = target.start;
      const cursor = e.cursor.toFixed(1);
      const start = g.start.toFixed(1);
      const range = g.range.toFixed(1);
      const syncTargets = dirty.current;
      dirty.current = false;
      if (cursor !== writtenCursor || syncTargets) {
        for (const { style } of clockTargets.playhead) {
          if (!syncTargets || style.getPropertyValue("--rt-now") !== cursor) style.setProperty("--rt-now", cursor);
        }
        writtenCursor = cursor;
      }
      if (syncTargets || start !== writtenStart || range !== writtenRange) {
        for (const { style } of clockTargets.axis) {
          if (style.getPropertyValue("--vs") !== start) style.setProperty("--vs", start);
          if (style.getPropertyValue("--range") !== range) style.setProperty("--range", range);
        }
        writtenStart = start;
        writtenRange = range;
      }
      const index = epochAt(scenario, e.cursor);
      const nextEpoch = index > 0 ? scenario.boundaries[index - 1] : -1;
      const commitInterval = e.speed >= 10 ? 100 : 40;
      if ((nextEpoch !== lastEpoch || (lastEpochDeferred && !deferEpoch)) && (!fast || atBoundary || now - lastCommit >= commitInterval)) {
        lastCommit = now;
        lastEpoch = nextEpoch;
        if (deferEpoch && !atBoundary) startTransition(() => setEpochTime(nextEpoch));
        else setEpochTime(nextEpoch);
        lastEpochDeferred = deferEpoch && !atBoundary;
      }
      const cursorChanged = !Object.is(e.cursor, notifiedCursor);
      const headChanged = !Object.is(e.head, notifiedHead);
      if (cursorChanged || headChanged) {
        if (!fast || headChanged || atBoundary || now - lastNotify >= 100) {
          lastNotify = now;
          notifiedCursor = e.cursor;
          notifiedHead = e.head;
          listeners.current.forEach((listener) => listener());
        }
      }
      frame = 0;
      if (live || e.playing || dirty.current || g.start !== target.start || g.range !== target.range) {
        frame = requestAnimationFrame(tick);
      }
    };
    const wake = () => {
      if (frame) return;
      last = performance.now();
      frame = requestAnimationFrame(tick);
    };
    wakeClock.current = wake;
    wake();
    return () => {
      wakeClock.current = null;
      cancelAnimationFrame(frame);
    };
  }, [headNow, axisRange, syncControls, clockTargets]);

  const spanTime = spanTimeAt(scenario, epochTime);
  const snapshot = useMemo(() => snapshotAt(scenario, spanTime), [scenario, spanTime]);
  denseRef.current = snapshot.order.length > DENSE_GLIDE_LIMIT;

  const visibleEntries = useMemo(() => {
    const rows: { event: TraceEventSpec; node: TraceNode }[] = [];
    for (const event of scenario.events) {
      if (event.ts > epochTime) break;
      const node = snapshot.byId.get(event.spanId);
      if (node) rows.push({ event, node });
    }
    return rows;
  }, [scenario.events, epochTime, snapshot.byId]);
  const eventsById = useMemo(() => new Map(scenario.events.map((event) => [event.id, event])), [scenario]);
  const selectedEvent: TraceEventSpec | null = eventId ? eventsById.get(eventId) ?? null : null;
  const scope = scopeId ? snapshot.byId.get(scopeId) ?? null : null;

  const entries = useMemo(() => {
    if (!scope && !filterActive(filter)) return visibleEntries;
    return visibleEntries.filter(({ event, node }) =>
      (!scope || isAncestor(scope, node)) && eventMatches(event, node, filter),
    );
  }, [visibleEntries, scope, filter]);
  const clearFilter = useCallback(() => setFilter(EMPTY_FILTER), []);

  const isFolded = useCallback(
    (node: TraceNode) => {
      const choice = userFold.get(node.spec.id);
      if (choice !== undefined) return choice;
      return autoFold && (node.spec.kind === "worker" || node.spec.kind === "group") && node.status !== "running" && node.children.length > 0;
    },
    [userFold, autoFold],
  );

  const revealSpan = useCallback(
    (id: string) => {
      setUserFold((current) => {
        const next = new Map(current);
        let node = snapshot.byId.get(id)?.parent ?? null;
        let changed = false;
        for (; node; node = node.parent) {
          if (isFolded(node)) {
            next.set(node.spec.id, false);
            changed = true;
          }
        }
        return changed ? next : current;
      });
      setReveal((current) => ({ id, nonce: (current?.nonce ?? 0) + 1 }));
    },
    [snapshot, isFolded],
  );

  const toggleFold = useCallback((node: TraceNode) => {
    setUserFold((current) => new Map(current).set(node.spec.id, !isFolded(node)));
  }, [isFolded]);

  const selectSpan = useCallback(
    (id: string | null) => {
      setScopeId(id);
      if (!id) return;
      const node = snapshot.byId.get(id);
      setEventId((current) => {
        const event = current ? eventsById.get(current) : null;
        const owner = event ? snapshot.byId.get(event.spanId) : null;
        return node && owner && isAncestor(node, owner) ? current : null;
      });
    },
    [snapshot, eventsById],
  );

  const selectEvent = useCallback(
    (id: string | null) => {
      setEventId(id);
      const event = id ? eventsById.get(id) : null;
      if (event) revealSpan(event.spanId);
    },
    [eventsById, revealSpan],
  );

  const callEventBySpan = useMemo(() => {
    const map = new Map<string, string>();
    for (const event of scenario.events) if (event.kind === "call") map.set(event.spanId, event.id);
    return map;
  }, [scenario]);

  const selectFromGraph = useCallback(
    (id: string | null) => {
      const node = id ? snapshot.byId.get(id) : null;
      const call = node && !isContainerKind(node.spec.kind) ? callEventBySpan.get(node.spec.id) : undefined;
      if (!call) return selectSpan(id);
      setEventId((current) => (current === call ? null : call));
      setScopeId((current) => {
        const scopeNode = current ? snapshot.byId.get(current) : null;
        return scopeNode && node && isAncestor(scopeNode, node) ? current : null;
      });
    },
    [snapshot, callEventBySpan, selectSpan],
  );

  const scopeFromLedger = useCallback(
    (id: string | null) => {
      selectSpan(id);
      if (id) revealSpan(id);
    },
    [selectSpan, revealSpan],
  );

  const setBand = useCallback((next: { from: number; to: number } | null) => {
    startTransition(() => {
      setBandState((current) => (current?.from === next?.from && current?.to === next?.to ? current : next));
    });
  }, []);

  const wasPlaying = useRef(false);
  const seek = useCallback(
    (time: number, phase: "start" | "move" | "end") => {
      const e = engine.current;
      if (phase === "start") wasPlaying.current = e.playing || e.follow;
      e.cursor = clamp(time, 0, e.head);
      e.follow = false;
      e.playing = false;
      if (phase === "end") {
        if (mode === "live" && e.cursor >= e.head - 50) e.follow = true;
        else e.playing = wasPlaying.current && e.cursor < e.head;
      }
      syncControls();
    },
    [mode, syncControls],
  );

  const play = useCallback(() => {
    const e = engine.current;
    if (mode === "replay" && e.cursor >= e.head) {
      e.cursor = 0;
      if (zoomRef.current && zoomRef.current.start > 0) {
        zoomRef.current = null;
        setZoomLabel(null);
      }
    }
    e.playing = true;
    if (mode === "live" && e.cursor >= e.head) e.follow = true;
    syncControls();
  }, [mode, syncControls]);
  const pause = useCallback(() => {
    engine.current.playing = false;
    engine.current.follow = false;
    syncControls();
  }, [syncControls]);
  const setSpeed = useCallback((speed: number) => {
    engine.current.speed = speed;
    syncControls();
  }, [syncControls]);
  const latest = useCallback(() => {
    const e = engine.current;
    e.cursor = e.head;
    e.follow = mode === "live";
    e.playing = mode === "live";
    syncControls();
    setPinNonce((n) => n + 1);
  }, [mode, syncControls]);

  const failures = useMemo(() => scenario.spans.filter((span) => span.failed).sort((a, b) => specEdge(a) - specEdge(b)), [scenario]);
  const gotoError = useCallback(
    (direction: 1 | -1) => {
      const e = engine.current;
      const known = failures.filter((span) => specEdge(span) <= e.head);
      const target = direction > 0 ? known.find((span) => specEdge(span) > e.cursor + 50) : [...known].reverse().find((span) => specEdge(span) < e.cursor - 50);
      if (!target) return;
      e.cursor = specEdge(target);
      e.follow = false;
      e.playing = false;
      syncControls();
      setEventId(callEventBySpan.get(target.id) ?? null);
      revealSpan(target.id);
    },
    [failures, callEventBySpan, syncControls, revealSpan],
  );

  const fullRange = useCallback(() => axisRange(engine.current.cursor), [axisRange]);
  const zoomTo = useCallback((window: TimeWindow | null, label: string | null) => {
    zoomRef.current = window;
    setZoomLabel(label);
    wakeClock.current?.();
  }, []);
  const zoomSpan = useCallback(
    (node: TraceNode) => {
      const end = node.end ?? engine.current.cursor;
      const duration = Math.max(400, end - node.spec.start);
      zoomTo({ start: Math.max(0, node.spec.start - duration * 0.08), range: duration * 1.2 }, node.spec.name);
      if (node.children.length) {
        setUserFold((current) => (current.get(node.spec.id) === false ? current : new Map(current).set(node.spec.id, false)));
        selectSpan(node.spec.id);
      } else {
        selectFromGraph(node.spec.id);
      }
    },
    [zoomTo, selectSpan, selectFromGraph],
  );
  const getGlide = useCallback(() => glide.current, []);
  const resetZoom = useCallback(() => zoomTo(null, null), [zoomTo]);
  const wheelZoom = useCallback((anchor: number, factor: number) => {
    const full = fullRange();
    const base = zoomRef.current ?? { start: 0, range: full };
    const range = clamp(base.range * factor, 300, full);
    if (range >= full * 0.995) return zoomTo(null, null);
    const at = base.start + anchor * base.range;
    zoomTo({ start: clamp(at - anchor * range, 0, Math.max(0, full - range)), range }, zoomLabel ?? copy.customRange);
  }, [fullRange, zoomTo, zoomLabel, copy.customRange]);
  const wheelPan = useCallback((fraction: number) => {
    const base = zoomRef.current;
    if (!base) return;
    zoomTo({ start: clamp(base.start + fraction * base.range, 0, Math.max(0, fullRange() - base.range)), range: base.range }, zoomLabel);
  }, [fullRange, zoomTo, zoomLabel]);

  const toggleKind = (kind: KindFilter) =>
    setFilter((current) => ({ ...current, kinds: current.kinds.includes(kind) ? current.kinds.filter((k) => k !== kind) : [...current.kinds, kind] }));

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (target.closest("input, textarea, [contenteditable='true']")) {
      if (event.key === "Escape") target.blur();
      return;
    }
    if (event.metaKey || event.ctrlKey || event.altKey) return;
    switch (event.key) {
      case " ":
        if (target.closest("button, [role='treeitem'], [role='slider']")) return;
        if (controls.playing || controls.follow) pause();
        else play();
        break;
      case "Escape":
        if (eventId) setEventId(null);
        else if (scopeId) setScopeId(null);
        else if (zoomRef.current) zoomTo(null, null);
        else return;
        break;
      case "j":
      case "k": {
        if (!entries.length) return;
        const index = entries.findIndex((entry) => entry.event.id === eventId);
        const next = index < 0 ? (event.key === "j" ? entries.length - 1 : 0) : clamp(index + (event.key === "j" ? 1 : -1), 0, entries.length - 1);
        selectEvent(entries[next].event.id);
        break;
      }
      case "/":
        searchRef.current?.focus();
        break;
      case "l":
      case "L":
        latest();
        break;
      case "[":
        gotoError(-1);
        break;
      case "]":
        gotoError(1);
        break;
      default:
        return;
    }
    event.preventDefault();
  };

  const splitDrag = useRef(false);
  const effectiveLayout: ConsoleLayout = collapsed || narrow ? "stack" : layout;
  const splitDown = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || collapsed) return;
    splitDrag.current = true;
    event.currentTarget.setPointerCapture(event.pointerId);
  };
  const splitMove = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!splitDrag.current) return;
    const rect = splitRef.current?.getBoundingClientRect();
    if (!rect) return;
    if (effectiveLayout === "stack") setGraphSize((size) => ({ ...size, stack: clamp((event.clientY - rect.top) / rect.height, 0.2, 0.8) }));
    else setGraphSize((size) => ({ ...size, side: clamp((event.clientX - rect.left) / rect.width, 0.32, 0.74) }));
  };
  const splitUp = () => {
    splitDrag.current = false;
  };
  const splitKey = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    const stack = effectiveLayout === "stack";
    if (event.key === "Enter") setCollapsed((value) => !value);
    else if (!collapsed && stack && (event.key === "ArrowUp" || event.key === "ArrowDown"))
      setGraphSize((size) => ({ ...size, stack: clamp(size.stack + (event.key === "ArrowDown" ? 0.04 : -0.04), 0.2, 0.8) }));
    else if (!collapsed && !stack && (event.key === "ArrowLeft" || event.key === "ArrowRight"))
      setGraphSize((size) => ({ ...size, side: clamp(size.side + (event.key === "ArrowRight" ? 0.03 : -0.03), 0.32, 0.74) }));
    else return;
    event.preventDefault();
  };

  const lastFocus = useRef<number | null>(null);
  useEffect(() => {
    if (!focus || focus.nonce === lastFocus.current || !snapshot.byId.has(focus.spanId)) return;
    lastFocus.current = focus.nonce;
    scopeFromLedger(focus.spanId);
  }, [focus, snapshot, scopeFromLedger]);

  const { stats, root } = snapshot;
  const counts = useMemo(() => {
    const out: Record<KindFilter, number> = { agent: 0, llm: 0, tool: 0 };
    for (const node of snapshot.order) out[kindOf(node)]++;
    return out;
  }, [snapshot]);
  const atEnd = !snapshot.running && !!root && root.status !== "running";
  const runFailed = atEnd && root.status === "failed";
  const statusLabel = !root ? copy.waiting : runFailed ? copy.runFailed : atEnd ? (stats.errors ? copy.completedWithErrors(stats.errors) : copy.completed) : controls.follow ? copy.live : controls.playing ? copy.replaySpeed(controls.speed) : copy.paused;
  const statusKind = !root ? "idle" : runFailed ? "failed" : atEnd ? "done" : controls.follow || controls.playing ? "running" : "paused";
  const live = !!root && root.status === "running";

  const onlyKind = (kind: KindFilter) => filter.kinds.length === 1 && filter.kinds[0] === kind;
  const toggleOnly = (kind: KindFilter) => setFilter((current) => ({ ...current, kinds: current.kinds.length === 1 && current.kinds[0] === kind ? [] : [kind] }));
  const costNode = summary?.cost ?? formatCost(stats.cost);
  const tokenNode = summary?.tokenLabel ?? (summary && summary.tokens == null ? (lang === "en" ? "Unreported" : "未上报") : formatTokens(summary?.tokens ?? stats.tokens));
  const layoutControls = (
    <>
      <div className="rt-seg" role="radiogroup" aria-label={copy.layout}>
        <button type="button" role="radio" aria-checked={layout === "stack"} aria-label={copy.stackLayout} data-on={layout === "stack" || undefined} onClick={() => setLayout("stack")} title={copy.stackLayout}><TraceGlyph name="stack" size={14} /></button>
        <button type="button" role="radio" aria-checked={layout === "side"} aria-label={copy.sideLayout} data-on={layout === "side" || undefined} onClick={() => setLayout("side")} disabled={narrow} title={narrow ? copy.tooNarrow : copy.sideLayout}><TraceGlyph name="side" size={14} /></button>
      </div>
      <button type="button" className="rt-ghost" onClick={() => setCollapsed((value) => !value)} aria-pressed={collapsed} title={copy.toggleGraphHint}>
        <TraceGlyph name="fit" size={14} />
        <span>{collapsed ? copy.expandGraph : copy.collapseGraph}</span>
      </button>
    </>
  );

  const fastReplay = controls.playing && !controls.follow && controls.speed >= 10;

  return (
    <TimelineEnvContext.Provider value={env}>
    <ClockContext.Provider value={clock}>
      <div
        ref={rootRef}
        className={`rt-console${effectiveReduced ? " rt-reduce" : ""}${className ? ` ${className}` : ""}`}
        data-layout={effectiveLayout}
        data-dense={snapshot.order.length > 60 || undefined}
        data-fast={fastReplay || undefined}
        tabIndex={-1}
        onKeyDown={onKeyDown}
        onPointerDown={(event) => {
          if (!(event.target as HTMLElement).closest("button, input, select, textarea, [tabindex]:not(.rt-console)")) rootRef.current?.focus({ preventScroll: true });
        }}
      >
        {!compact && (
          <header className="rt-head">
            <span className="rt-spark"><TraceGlyph name="spark" size={15} /></span>
            <strong>{copy.title}</strong>
            {(task ?? scenario.task) && <span className="rt-task" title={task ?? scenario.task}>{task ?? scenario.task}</span>}
            <span className="rt-pill" data-status={statusKind} aria-live="polite">
              {statusKind === "running" && <i className="rt-pulse" />}
              {statusLabel}
            </span>
            <div className="rt-head-actions">{layoutControls}</div>
          </header>
        )}
        {!compact && (
          <dl className="rt-stats">
            <Stat copy={copy} label={copy.statDuration} live={live}><RunDuration root={root} /></Stat>
            <Stat copy={copy} label={copy.statCost}>{costNode}</Stat>
            <Stat copy={copy} label={copy.statTokens}>{tokenNode}</Stat>
            {stats.workers > 0 && <Stat copy={copy} label={copy.statWorkers}>{stats.workers}</Stat>}
            {stats.llmCalls > 0 && <Stat copy={copy} label={copy.statLlm} active={onlyKind("llm")} onClick={() => toggleOnly("llm")}>{stats.llmCalls}</Stat>}
            <Stat copy={copy} label={copy.statTools} active={onlyKind("tool")} onClick={() => toggleOnly("tool")}>{stats.toolCalls}</Stat>
            <Stat copy={copy} label={copy.statFailed} tone="failed" active={filter.errors} onClick={() => setFilter((current) => ({ ...current, errors: !current.errors }))}>{stats.errors}</Stat>
          </dl>
        )}
        <div className="rt-toolbar">
          <label className="rt-search">
            <TraceGlyph name="search" size={13} />
            <input
              ref={searchRef}
              type="search"
              value={filter.query}
              onChange={(event) => setFilter((current) => ({ ...current, query: event.target.value }))}
              placeholder={copy.search}
              aria-label={copy.search}
            />
            {!filter.query && <kbd>/</kbd>}
          </label>
          <div className="rt-chips" role="group" aria-label={copy.filterByKind}>
            <button type="button" data-on={(!filter.kinds.length && !filter.errors) || undefined} onClick={() => setFilter((current) => ({ ...current, kinds: [], errors: false }))}>{copy.all}</button>
            {KIND_CHIPS.filter((chip) => chip.id !== "llm" || counts.llm > 0).map((chip) => (
              <button type="button" key={chip.id} data-kind={chip.id} data-on={filter.kinds.includes(chip.id) || undefined} aria-pressed={filter.kinds.includes(chip.id)} onClick={() => toggleKind(chip.id)}>
                <i />{chip.label(copy)}<span>{counts[chip.id]}</span>
              </button>
            ))}
            <button type="button" data-kind="failed" data-on={filter.errors || undefined} aria-pressed={filter.errors} onClick={() => setFilter((current) => ({ ...current, errors: !current.errors }))}>
              <i />{copy.chipFailed}<span>{stats.errors}</span>
            </button>
          </div>
          {compact && (
            <div className="rt-toolbar-end">
              <span className="rt-mini-stat"><span>{copy.statCost}</span><b>{costNode}</b></span>
              <span className="rt-mini-stat"><span>{copy.statTokens}</span><b>{tokenNode}</b></span>
              {layoutControls}
            </div>
          )}
        </div>
        <div
          ref={splitRef}
          className="rt-split"
          data-layout={effectiveLayout}
          data-collapsed={collapsed || undefined}
          style={{ "--rt-a": effectiveLayout === "stack" ? graphSize.stack : graphSize.side } as CSSProperties}
        >
          <div className="rt-graph">
            {collapsed ? (
              <TraceOverview snapshot={snapshot} scopeId={scopeId} band={band} onSelectSpan={selectSpan} onExpand={() => setCollapsed(false)} />
            ) : (
              <TraceWaterfall
                snapshot={snapshot}
                resetKey={resetKey}
                view={view}
                getWindow={getGlide}
                isFolded={isFolded}
                fast={fastReplay}
                scopeId={scopeId}
                selectedEvent={selectedEvent}
                hover={hover}
                filter={filter}
                band={band}
                zoomLabel={zoomLabel}
                pinNonce={pinNonce}
                reveal={reveal}
                onSelectSpan={selectFromGraph}
                onToggleFold={toggleFold}
                onHover={setHover}
                onZoomSpan={zoomSpan}
                onResetZoom={resetZoom}
                onSeek={seek}
                onWheelZoom={wheelZoom}
                onWheelPan={wheelPan}
              />
            )}
          </div>
          <div
            className="rt-splitter"
            role="separator"
            tabIndex={0}
            aria-orientation={effectiveLayout === "stack" ? "horizontal" : "vertical"}
            aria-label={copy.splitter}
            onPointerDown={splitDown}
            onPointerMove={splitMove}
            onPointerUp={splitUp}
            onPointerCancel={splitUp}
            onDoubleClick={() => setCollapsed((value) => !value)}
            onKeyDown={splitKey}
          >
            <i />
          </div>
          <div className="rt-ledger-pane">
            <TraceLedger
              snapshot={snapshot}
              totalEvents={visibleEntries.length}
              entries={entries}
              scope={scope}
              selectedEventId={eventId}
              hover={hover}
              filter={filter}
              pinNonce={pinNonce}
              onSelectEvent={selectEvent}
              onHover={setHover}
              onScope={scopeFromLedger}
              onBand={setBand}
              onClearFilter={clearFilter}
            />
          </div>
        </div>
        <TracePlayback
          scenario={scenario}
          mode={mode}
          controls={controls}
          band={band}
          onPlay={play}
          onPause={pause}
          onSeek={seek}
          onSpeed={setSpeed}
          onLatest={latest}
          onPrevError={() => gotoError(-1)}
          onNextError={() => gotoError(1)}
          onHoverTime={(time) => setHover(time == null ? null : { spanId: null, time, source: "scrubber" })}
        />
      </div>
    </ClockContext.Provider>
    </TimelineEnvContext.Provider>
  );
}
