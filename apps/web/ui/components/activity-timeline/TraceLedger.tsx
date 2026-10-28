"use client";

import {
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { EventIcon, SpanIcon, TraceGlyph } from "./traceIcons";
import { useCopy, useTimelineEnv } from "./traceCopy";
import {
  formatClock,
  formatCost,
  formatTokens,
  isAncestor,
  sameNodeState,
  toRealTime,
  type TraceEventSpec,
  type TraceNode,
  type TraceSnapshot,
} from "./traceModel";
import { SpanDuration, actorColor, filterActive, usePresence, type FilterState } from "./traceShared";
import type { HoverState } from "./TraceWaterfall";

export interface LedgerEntry {
  event: TraceEventSpec;
  node: TraceNode;
}

type Entry = LedgerEntry;

const entryKey = (entry: Entry) => entry.event.id;
/** Matches .rl-head height and .rl-list top padding in activity-timeline.css. */
const ROW_HEIGHT = 32;
const LIST_PAD = 4;
const OVERSCAN = 480;

function pathOf(node: TraceNode): TraceNode[] {
  const out: TraceNode[] = [];
  for (let current: TraceNode | null = node; current; current = current.parent) out.unshift(current);
  return out;
}

interface RowProps {
  entry: Entry;
  leaving: boolean;
  leaveDelay: number;
  /** Null when the row mounts because it scrolled into view rather than because it is new. */
  enterDelay: number | null;
  selected: boolean;
  linked: boolean;
  dimmed: boolean;
  onToggle: (entry: Entry) => void;
  onEnter: (entry: Entry) => void;
  onLeave: () => void;
  onScope: (id: string) => void;
  onMeasure: (key: string, height: number | null) => void;
}

function ledgerPropsEqual(a: RowProps, b: RowProps): boolean {
  for (const key in b) {
    if (key !== "entry" && a[key as keyof RowProps] !== b[key as keyof RowProps]) return false;
  }
  return a.entry.event === b.entry.event && sameNodeState(a.entry.node, b.entry.node);
}

const LedgerRow = memo(function LedgerRow(props: RowProps) {
  const { entry, selected, onMeasure } = props;
  const { event, node } = entry;
  const copy = useCopy();
  const { origin, lang, gaps } = useTimelineEnv();
  const rowRef = useRef<HTMLDivElement>(null);
  const [enterDelay] = useState(props.enterDelay);
  const [showBody, setShowBody] = useState(selected);
  const [closing, setClosing] = useState(false);
  useEffect(() => {
    if (selected) {
      setShowBody(true);
      setClosing(false);
      return;
    }
    if (!showBody) return;
    setClosing(true);
    const timer = window.setTimeout(() => {
      setShowBody(false);
      setClosing(false);
    }, 220);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selected]);
  // Only rows with an open body differ from the fixed row height the window math assumes.
  useLayoutEffect(() => {
    const row = rowRef.current;
    if (!showBody || !row) return;
    const key = event.id;
    const observer = new ResizeObserver(() => onMeasure(key, row.offsetHeight));
    observer.observe(row);
    onMeasure(key, row.offsetHeight);
    return () => {
      observer.disconnect();
      onMeasure(key, null);
    };
  }, [showBody, event.id, onMeasure]);
  const call = event.kind === "call";
  const actor = node.actor;
  const failed = call && node.status === "failed";
  const real = toRealTime(gaps, event.ts);
  const absolute = origin != null ? new Date(origin + real).toLocaleString(lang === "en" ? "en" : "zh-CN") : undefined;
  return (
    <div
      ref={rowRef}
      className="rl-row"
      data-ts={event.ts}
      data-event={event.id}
      data-kind={call ? node.spec.kind : event.kind}
      data-status={call ? node.status : undefined}
      data-selected={selected || undefined}
      data-linked={props.linked || undefined}
      data-dim={props.dimmed || undefined}
      data-leaving={props.leaving || undefined}
      data-fresh={enterDelay != null || undefined}
      role="listitem"
      style={{ animationDelay: props.leaving ? `${props.leaveDelay}ms` : `${enterDelay ?? 0}ms`, "--wc": actorColor(node) } as CSSProperties}
    >
      <button
        type="button"
        className="rl-head"
        aria-expanded={selected}
        onClick={() => props.onToggle(entry)}
        onMouseEnter={() => props.onEnter(entry)}
        onMouseLeave={props.onLeave}
      >
        <time className="rl-time" title={absolute}>{formatClock(real)}</time>
        {call ? <SpanIcon node={node} size={18} /> : <EventIcon kind={event.kind} size={18} />}
        <span className="rl-actor" title={actor.spec.detail ? `${actor.spec.name} · ${actor.spec.detail}` : actor.spec.name}>
          <i aria-hidden="true" />
          <span>{actor.spec.name}</span>
        </span>
        <span className="rl-text" title={call ? (node.spec.detail ?? node.spec.name) : event.text}>
          {call ? (
            <>
              <code>{node.spec.name}</code>
              {node.spec.detail && <span className="rl-arg">{node.spec.detail}</span>}
              {node.spec.retryOf && <span className="rt-retry">{copy.retry}</span>}
            </>
          ) : (
            event.text
          )}
        </span>
        <span className="rl-meta" data-status={call ? node.status : undefined}>
          {call && node.status === "running" && (
            <svg className="rt-spin" width="11" height="11" viewBox="0 0 24 24" fill="none" aria-hidden="true">
              <circle cx="12" cy="12" r="8" stroke="currentColor" strokeOpacity=".22" strokeWidth="3" />
              <path d="M20 12a8 8 0 0 0-8-8" stroke="currentColor" strokeWidth="3" strokeLinecap="round" />
            </svg>
          )}
          {failed && <span className="rl-failed">{copy.failedTag}</span>}
          {call && <span title={node.spec.estimated ? copy.estimatedHint : undefined}><SpanDuration node={node} /></span>}
        </span>
        <TraceGlyph name="chevron" size={12} className="rl-chev" />
      </button>
      {showBody && (
        <div className="rl-body" data-closing={closing || undefined}>
          <div className="rl-body-inner">
            {failed && node.spec.error && (
              <p className="rl-error">
                <TraceGlyph name="alert" size={13} />
                {node.spec.error}
              </p>
            )}
            {call && node.spec.kind === "tool" && (node.spec.input || node.spec.output) ? (
              <ToolDetails node={node} />
            ) : call && node.status === "running" ? (
              <p className="rl-pending">{copy.pending}</p>
            ) : (
              event.body && <pre className="rl-pre">{event.body}</pre>
            )}
            <div className="rl-foot">
              <nav className="rl-path" aria-label={copy.location}>
                {pathOf(node).map((step, index) => (
                  <span key={step.spec.id}>
                    {index > 0 && <TraceGlyph name="chevron" size={9} className="rl-path-sep" />}
                    <button type="button" onClick={() => props.onScope(step.spec.id)}>{step.spec.name}</button>
                  </span>
                ))}
              </nav>
              {call && node.tokens > 0 && <span>{formatTokens(node.tokens)} Token</span>}
              {call && node.cost > 0 && <span>{formatCost(node.cost)}</span>}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}, ledgerPropsEqual);

/** Scope context stays in the fixed header so a short ledger still shows its events. */
function ScopeBar({ node, count, onClear }: { node: TraceNode; count: number; onClear: () => void }) {
  const copy = useCopy();
  const { spec } = node;
  return (
    <div className="rl-scope-bar" key={spec.id}>
      <SpanIcon node={node} size={20} />
      <span className="rl-scope-who" title={spec.detail ? `${spec.name} · ${spec.detail}` : spec.name}>
        <b>{spec.name}</b>
        <small>{copy.kind[spec.kind]}{spec.detail ? ` · ${spec.detail}` : ""}</small>
      </span>
      <span className="rl-scope-stats">
        <span><SpanDuration node={node} /></span>
        {node.cost > 0 && <span>{formatCost(node.cost)}</span>}
        {node.tokens > 0 && <span>{formatTokens(node.tokens)} Token</span>}
        <span>{copy.eventCount(count)}</span>
      </span>
      <span className="rt-pill" data-status={node.status}>
        {node.status === "running" && <i className="rt-pulse" />}
        {copy.status[node.status]}
      </span>
      <button type="button" className="rl-scope-close" onClick={onClear} aria-label={copy.clearScopeAria} title={copy.clearScopeTitle}>
        <TraceGlyph name="close" size={13} />
      </button>
    </div>
  );
}

/** Mount full text only in expanded details, with independent scroll areas for I/O. */
function ToolDetails({ node }: { node: TraceNode }) {
  const copy = useCopy();
  const { spec } = node;
  const output = node.status !== "running" ? spec.output : undefined;
  return (
    <>
      {spec.input && <pre className="rl-pre" aria-label={copy.input}><b>{copy.input}</b>{"\n"}{spec.input}</pre>}
      {node.status === "running"
        ? <p className="rl-pending">{copy.pending}</p>
        : output && <pre className="rl-pre" aria-label={copy.output}><b>{copy.output}</b>{"\n"}{output}</pre>}
    </>
  );
}

function ScopeDetail({ node }: { node: TraceNode }) {
  const copy = useCopy();
  const { spec } = node;
  const output = node.status !== "running" ? spec.output : undefined;
  const error = node.status === "failed" ? spec.error : undefined;
  if (!spec.input && !output && !error) return null;
  return (
    <section className="rl-scope" key={spec.id} aria-label={copy.ioOf(spec.name)}>
      {error && <p className="rl-error"><TraceGlyph name="alert" size={13} />{error}</p>}
      <ToolDetails node={node} />
    </section>
  );
}

export interface LedgerProps {
  snapshot: TraceSnapshot;
  /** Events after scope and filter, shared with keyboard navigation in the console. */
  entries: LedgerEntry[];
  scope: TraceNode | null;
  selectedEventId: string | null;
  hover: HoverState | null;
  filter: FilterState;
  pinNonce: number;
  onSelectEvent: (id: string | null) => void;
  onHover: (hover: HoverState | null) => void;
  onScope: (id: string | null) => void;
  onBand: (band: { from: number; to: number } | null) => void;
  onClearFilter: () => void;
}

export function TraceLedger(props: LedgerProps) {
  const { snapshot, entries, scope, selectedEventId, hover, filter } = props;
  const copy = useCopy();
  const latest = useRef(props);
  latest.current = props;
  const scrollRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const pinnedRef = useRef(true);
  const programmaticTop = useRef<number | null>(null);
  const [pinned, setPinned] = useState(true);
  const [unread, setUnread] = useState(0);
  const [far, setFar] = useState(false);
  const [viewport, setViewport] = useState({ top: 0, height: 600 });
  const [heights, setHeights] = useState<Map<string, number>>(() => new Map());
  const frame = useRef(0);
  useEffect(() => () => cancelAnimationFrame(frame.current), []);

  const present = usePresence(entries, entryKey, 280);
  const presentRef = useRef(present);
  presentRef.current = present;

  // Long runs keep thousands of events, so only the rows near the viewport mount.
  const tops = useMemo(() => {
    const out = new Float64Array(present.length + 1);
    for (let index = 0; index < present.length; index++) out[index + 1] = out[index] + (heights.get(present[index].key) ?? ROW_HEIGHT);
    return out;
  }, [present, heights]);
  const topsRef = useRef(tops);
  topsRef.current = tops;
  const indexAt = useCallback((y: number) => {
    const list = topsRef.current;
    let lo = 0;
    let hi = list.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (list[mid + 1] <= y) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }, []);
  const listOffset = () => (listRef.current?.offsetTop ?? 0) + LIST_PAD;

  const onMeasure = useCallback((key: string, height: number | null) => {
    setHeights((current) => {
      const value = height == null || Math.abs(height - ROW_HEIGHT) < 1 ? undefined : height;
      if (current.get(key) === value) return current;
      const next = new Map(current);
      if (value == null) next.delete(key);
      else next.set(key, value);
      return next;
    });
  }, []);

  const prevKeys = useRef<Set<string>>(new Set());
  const prevCount = useRef(0);
  useEffect(() => {
    prevKeys.current = new Set(entries.map(entryKey));
    const added = entries.length - prevCount.current;
    prevCount.current = entries.length;
    if (!pinnedRef.current && added > 0) setUnread((count) => count + added);
  }, [entries]);

  const setPin = useCallback((value: boolean) => {
    pinnedRef.current = value;
    setPinned(value);
    if (value) setUnread(0);
  }, []);

  const scrollTo = useCallback((top: number, behavior: ScrollBehavior = "auto") => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const target = Math.max(0, Math.min(top, scroller.scrollHeight - scroller.clientHeight));
    programmaticTop.current = target;
    scroller.scrollTo({ top: target, behavior });
  }, []);

  const measure = useCallback(() => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    const top = scroller.scrollTop;
    const height = scroller.clientHeight;
    setViewport((current) => (current.top === top && current.height === height ? current : { top, height }));
    const rows = presentRef.current;
    if (!rows.length) return latest.current.onBand(null);
    const offset = listOffset();
    const first = indexAt(Math.max(0, top - offset));
    const last = indexAt(Math.max(0, top + height - offset - 1));
    if (first >= rows.length) return latest.current.onBand(null);
    const from = rows[first].item.event.ts;
    const to = rows[Math.min(rows.length - 1, Math.max(first, last))].item.event.ts;
    latest.current.onBand({ from, to });
  }, [indexAt]);

  const schedule = useCallback(() => {
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      measure();
    });
  }, [measure]);

  useEffect(() => {
    const scroller = scrollRef.current;
    const list = listRef.current;
    if (!scroller || !list) return;
    const onResize = () => {
      if (pinnedRef.current) {
        const target = scroller.scrollHeight - scroller.clientHeight;
        if (Math.abs(scroller.scrollTop - target) >= 1) {
          programmaticTop.current = target;
          scroller.scrollTop = target;
        }
      }
      schedule();
    };
    const observer = new ResizeObserver(onResize);
    observer.observe(list);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [schedule]);

  useEffect(() => {
    setPin(true);
    scrollTo(Number.MAX_SAFE_INTEGER, "smooth");
  }, [props.pinNonce, setPin, scrollTo]);

  const scopeId = scope?.spec.id ?? null;
  useLayoutEffect(() => {
    if (!scopeId) return;
    const node = latest.current.scope;
    if (node && node.status === "running" && pinnedRef.current) return;
    setPin(false);
    scrollTo(0);
  }, [scopeId, setPin, scrollTo]);

  useEffect(() => {
    if (!selectedEventId) return;
    const raf = requestAnimationFrame(() => {
      const scroller = scrollRef.current;
      const index = presentRef.current.findIndex((entry) => entry.key === selectedEventId);
      if (!scroller || index < 0) return;
      const top = listOffset() + topsRef.current[index];
      if (top >= scroller.scrollTop && top + 60 <= scroller.scrollTop + scroller.clientHeight) return;
      setPin(false);
      scrollTo(top - scroller.clientHeight * 0.3, "smooth");
    });
    return () => cancelAnimationFrame(raf);
  }, [selectedEventId, setPin, scrollTo]);

  const onScroll = () => {
    const scroller = scrollRef.current;
    if (!scroller) return;
    schedule();
    if (programmaticTop.current != null) {
      if (Math.abs(scroller.scrollTop - programmaticTop.current) < 2) programmaticTop.current = null;
      return;
    }
    const gap = scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop;
    const atBottom = gap <= 12;
    if (atBottom !== pinnedRef.current) setPin(atBottom);
    setFar(gap > scroller.clientHeight);
  };
  const userScrollIntent = () => {
    // A user gesture cancels any pending smooth scroll so follow state tracks the user again.
    programmaticTop.current = null;
  };

  const onToggle = useCallback((entry: Entry) => {
    const current = latest.current;
    current.onSelectEvent(current.selectedEventId === entry.event.id ? null : entry.event.id);
  }, []);
  const onEnter = useCallback((entry: Entry) => {
    latest.current.onHover({ spanId: entry.node.spec.id, time: entry.event.ts, source: "ledger" });
  }, []);
  const onLeave = useCallback(() => latest.current.onHover(null), []);
  const onScope = useCallback((id: string) => latest.current.onScope(id), []);

  const hoverNode = hover?.source === "graph" && hover.spanId ? snapshot.byId.get(hover.spanId) ?? null : null;
  let leavingBelow = 0;
  const leaveDelays = new Map<string, number>();
  for (let index = present.length - 1; index >= 0 && leavingBelow < 6; index--) {
    if (present[index].leaving) leaveDelays.set(present[index].key, Math.min(leavingBelow++, 4) * 18);
  }
  const total = snapshot.events.length;
  const scoped = !!scope || filterActive(filter);

  const offset = listRef.current ? listOffset() : LIST_PAD;
  const startIndex = present.length ? indexAt(Math.max(0, viewport.top - offset - OVERSCAN)) : 0;
  const endIndex = present.length ? Math.min(present.length, indexAt(Math.max(0, viewport.top + viewport.height - offset + OVERSCAN)) + 1) : 0;
  // The newest rows always render so live arrivals animate in even before the window catches up.
  const tailStart = Math.max(endIndex, present.length - 3);
  let freshIndex = 0;
  const renderRow = (index: number) => {
    const entry = present[index];
    const fresh = !entry.leaving && !prevKeys.current.has(entry.key);
    const enterDelay = fresh ? (prevKeys.current.size > 0 ? Math.min(freshIndex++, 10) * 26 : 0) : null;
    return (
      <LedgerRow
        key={entry.key}
        entry={entry.item}
        leaving={entry.leaving}
        leaveDelay={leaveDelays.get(entry.key) ?? 0}
        enterDelay={enterDelay}
        selected={selectedEventId === entry.key}
        linked={!!hoverNode && isAncestor(hoverNode, entry.item.node)}
        dimmed={!!hoverNode && !isAncestor(hoverNode, entry.item.node)}
        onToggle={onToggle}
        onEnter={onEnter}
        onLeave={onLeave}
        onScope={onScope}
        onMeasure={onMeasure}
      />
    );
  };
  const rows: ReactNode[] = [];
  for (let index = startIndex; index < endIndex; index++) rows.push(renderRow(index));
  const gapAfterWindow = tops[tailStart] - tops[endIndex];
  if (gapAfterWindow > 0) rows.push(<div key="rl-gap" aria-hidden="true" style={{ height: gapAfterWindow }} />);
  for (let index = tailStart; index < present.length; index++) rows.push(renderRow(index));

  return (
    <div className="rl">
      <div className="rl-head-bar" data-scoped={scope ? "" : undefined}>
        {scope ? (
          <ScopeBar node={scope} count={entries.length} onClear={() => props.onScope(null)} />
        ) : (
          <>
            <strong>{copy.ledger}</strong>
            <span className="rl-count">{scoped ? `${entries.length} / ${total}` : total}</span>
          </>
        )}
      </div>
      <div
        ref={scrollRef}
        className="rl-scroll"
        onScroll={onScroll}
        onWheel={userScrollIntent}
        onTouchStart={userScrollIntent}
        onPointerDown={userScrollIntent}
        onKeyDown={userScrollIntent}
      >
        {scope && <ScopeDetail node={scope} />}
        <div ref={listRef} className="rl-list" role="list" aria-label={copy.ledger}>
          {startIndex > 0 && <div aria-hidden="true" style={{ height: tops[startIndex] }} />}
          {rows}
        </div>
        {!entries.length && (
          <div className="rl-empty">
            {total === 0 ? (
              <span>{copy.firstEvent}</span>
            ) : (
              <>
                <span>{copy.noMatch}</span>
                {filterActive(filter) && <button type="button" onClick={props.onClearFilter}>{copy.clearFilter}</button>}
                {scope && <button type="button" onClick={() => props.onScope(null)}>{copy.clearScope}</button>}
              </>
            )}
          </div>
        )}
      </div>
      {!pinned && entries.length > 0 && (unread > 0 || far) && (
        <button type="button" className="rl-jump" onClick={() => { setPin(true); scrollTo(Number.MAX_SAFE_INTEGER, "smooth"); }}>
          <TraceGlyph name="chevron" size={12} />
          <span>{copy.jumpLatest}</span>
          {unread > 0 && <b>{unread > 99 ? "99+" : unread}</b>}
        </button>
      )}
    </div>
  );
}
