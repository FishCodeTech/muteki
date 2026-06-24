"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties } from "react";
import { ChatMessage, DeckState, isReviewWorkerLane, isWorkerLane, COORDINATOR_IDS } from "@/lib/events";
import { useT, useLang } from "@/lib/i18n";
import { workerColor, workerEngine, workerInitial, workerShortLabel } from "@/lib/workers";
import { Icon } from "@/components/Icon";
import { PanelEmpty } from "@/components/PanelEmpty";
import { ChipFilterBar } from "@/components/ChipFilterBar";

// Stable filter key + display metadata for a message's speaker. Workers key by
// their solverId (so each cli-claude-N filters independently); coordinator,
// system, and human collapse into one bucket each.
const COORD_KEY = "__coordinator";
const SYSTEM_KEY = "__system";
const HUMAN_KEY = "__human";

function speakerKey(m: ChatMessage): string {
  if (m.role === "human") return HUMAN_KEY;
  if (m.role === "system") return SYSTEM_KEY;
  if (m.solverId && isWorkerLane(m.solverId)) return m.solverId;
  return COORD_KEY;
}

/**
 * The raw activity stream — every worker + coordinator + system event in time
 * order (the old main-timeline firehose). Lives in a secondary panel now so the
 * coordinator conversation stays readable; this is where the operator inspects
 * the full play-by-play.
 */

function tsMs(ts: number): number {
  if (!ts) return 0;
  return ts < 1e12 ? ts * 1000 : ts;
}

function clock(ts: number): string {
  const ms = tsMs(ts);
  if (!ms) return "";
  const d = new Date(ms);
  if (isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// Coarse "x ago" string for the row's hover title — light touch, recomputed only
// on render (good enough; the tooltip is on-demand). zh/en chosen by the `lang`.
function relTime(ts: number, lang: string): string {
  const ms = tsMs(ts);
  if (!ms) return "";
  const sec = Math.max(0, Math.round((Date.now() - ms) / 1000));
  const zh = lang === "zh";
  if (sec < 5) return zh ? "刚刚" : "just now";
  if (sec < 60) return zh ? `${sec} 秒前` : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return zh ? `${min} 分钟前` : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return zh ? `${hr} 小时前` : `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return zh ? `${day} 天前` : `${day}d ago`;
}

function rowClass(m: ChatMessage): string {
  if (m.role === "human") return "human";
  if (m.role === "system") return `system ${m.kind}`;
  if (m.solverId && isWorkerLane(m.solverId)) return `worker ${m.kind}`;
  return `coordinator ${m.kind}`;
}

// Distance (px) from the bottom within which we consider the operator "pinned"
// and keep auto-following the tail. Small so a deliberate scroll-up of even a
// row or two releases the follow and surfaces the unread pill instead of yanking.
const PIN_SLOP = 40;
// Same speaker within this window (ms) → suppress the repeated header for a
// denser, iMessage/Slack-style grouped log.
const GROUP_WINDOW_MS = 60_000;
const COMPACT_KEY = "muteki.activity.compact";

export function ActivityStream({ deck }: { deck: DeckState }) {
  const t = useT();
  const { lang } = useLang();
  const feedRef = useRef<HTMLDivElement>(null);

  // ── density toggle (comfortable ↔ compact), persisted to localStorage ──────
  const [compact, setCompact] = useState(false);
  useEffect(() => {
    try { if (window.localStorage.getItem(COMPACT_KEY) === "1") setCompact(true); } catch { /* ignore */ }
  }, []);
  const toggleCompact = () =>
    setCompact((v) => {
      const next = !v;
      try { window.localStorage.setItem(COMPACT_KEY, next ? "1" : "0"); } catch { /* ignore */ }
      return next;
    });

  // ── per-speaker filter ──────────────────────────────────────────────────
  // CLICK-TO-SHOW: `shown` holds the keys the operator has SELECTED to display.
  // Empty = show everyone (default); click a chip to light it up and show only
  // the selected speakers (multi-select — click several to watch them together).
  // Distinct speakers are derived from the live chat in first-appearance order.
  const [shown, setShown] = useState<Set<string>>(new Set());
  const speakers = useMemo(() => {
    const seen = new Map<string, { key: string; label: string; fullLabel: string; sub: string; color?: string; initial: string; review?: boolean }>();
    for (const m of deck.chat) {
      const key = speakerKey(m);
      if (seen.has(key)) continue;
      if (key === HUMAN_KEY) seen.set(key, { key, label: t("coord.you"), fullLabel: t("coord.you"), sub: "", initial: "你" });
      else if (key === SYSTEM_KEY) seen.set(key, { key, label: "system", fullLabel: "system", sub: "", initial: "S" });
      else if (key === COORD_KEY) seen.set(key, { key, label: t("coord.title"), fullLabel: t("coord.title"), sub: "", color: workerColor("reason"), initial: "CO" });
      else {
        const review = isReviewWorkerLane(deck.lanes[key]);
        seen.set(key, {
          key,
          label: workerShortLabel(key),
          fullLabel: key,
          sub: review ? `${t("worker.role.review")} · ${workerEngine(key)}` : workerEngine(key),
          color: workerColor(key),
          initial: workerInitial(key),
          review,
        });
      }
    }
    return [...seen.values()];
  }, [deck.chat, deck.lanes, t]);

  const toggleSpeaker = (key: string) =>
    setShown((prev) => { const n = new Set(prev); n.has(key) ? n.delete(key) : n.add(key); return n; });
  const showAll = () => setShown(new Set());

  const visibleChat = useMemo(
    () => (shown.size === 0 ? deck.chat : deck.chat.filter((m) => shown.has(speakerKey(m)))),
    [deck.chat, shown],
  );
  const filterSummary = shown.size === 0
    ? t("activity.filterAll")
    : t("activity.filterSelected", { n: shown.size, total: speakers.length });
  // "stick to bottom" follow mode: only auto-scroll to the newest message when the
  // operator is ALREADY pinned to (within PIN_SLOP px of) the bottom. If they've
  // scrolled up to read history, new messages must NOT yank them down — instead we
  // surface a "↓ N new" pill they click to jump down and re-enable follow. (Was:
  // unconditional scrollTop=scrollHeight on every deck.chat change → "疯狂闪现到最
  // 下面" while reading history.)
  const stick = useRef(true);
  // count of new messages that arrived while the operator was scrolled up. 0 = pinned
  // (pill hidden); >0 → pill shows "↓ N new". Reset to 0 on jump / re-pin.
  const [unread, setUnread] = useState(0);
  // deck.chat is a CHAT_CAP-message ring buffer (events.ts: `.slice(-CHAT_CAP)`,
  // raised from 400 to 4000 in defect-7 so a long run's history isn't dropped). Once
  // it fills, .length stays at the cap while messages cycle — a length check would
  // NEVER see growth. Detect a genuinely new message by the LAST message's unique id
  // (streaming updates mutate the last message in place, keeping its id, so those
  // don't false-trigger the pill).
  const lastId = useRef(deck.chat[deck.chat.length - 1]?.id);

  const scrollToBottom = (smooth: boolean) => {
    const el = feedRef.current;
    if (!el) return;
    const reduce = typeof window !== "undefined"
      && window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    if (smooth && !reduce && typeof el.scrollTo === "function") {
      el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    } else {
      el.scrollTop = el.scrollHeight;
    }
  };

  const jumpToBottom = () => {
    scrollToBottom(true);
    stick.current = true;
    setUnread(0);
  };

  useEffect(() => {
    const curLast = deck.chat[deck.chat.length - 1]?.id;
    const arrived = curLast !== lastId.current; // a new message (new id) landed
    lastId.current = curLast;
    if (stick.current) {
      scrollToBottom(false); // following the tail on append → keep pinned (instant)
      if (unread) setUnread(0);
    } else if (arrived) {
      setUnread((n) => n + 1); // reading history + a new message → bump the pill count
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deck.chat]);

  const speaker = (m: ChatMessage): string => {
    if (m.role === "human") return t("coord.you");
    if (m.role === "system") return m.kind === "insight" ? "insight" : "system";
    if (m.solverId && COORDINATOR_IDS.has(m.solverId)) return t("coord.title");
    return m.solverId || t("hitl.agent");
  };

  return (
    <div className="panel-scroll-wrap activity-panel">
      {speakers.length > 1 && (
        <ChipFilterBar
          id="activity-filter-chips"
          className="activity-filterbar"
          label={t("activity.filter")}
          summary={filterSummary}
          title={t("activity.filterTitle")}
          expandTitle={t("activity.filterExpand")}
          collapseTitle={t("activity.filterCollapse")}
          clearLabel={t("activity.filterAll")}
          showClear={shown.size > 0}
          onClear={showAll}
        >
          {speakers.map((s) => (
            <button
              key={s.key}
              type="button"
              className={`filter-chip ${shown.size === 0 || shown.has(s.key) ? "on" : "off"} ${s.review ? "review-speaker" : ""}`}
              style={s.color ? ({ "--fc": s.color } as CSSProperties) : undefined}
              onClick={() => toggleSpeaker(s.key)}
              aria-pressed={shown.has(s.key)}
              title={`${s.fullLabel} · ${t("activity.filterTitle")}`}
            >
              <span className="filter-chip-ico">{s.initial}</span>
              <span className="filter-chip-text">{s.label}</span>
            </button>
          ))}
        </ChipFilterBar>
      )}
    <div
      className="panel-scroll"
      ref={feedRef}
      onScroll={(e) => {
        const el = e.currentTarget;
        const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight <= PIN_SLOP;
        stick.current = atBottom;
        if (atBottom && unread) setUnread(0);
      }}
    >
      <div className="panel-titlebar">
        <div className="panel-title">{t("activity.title")}</div>
        <button
          type="button"
          className={`density-toggle ${compact ? "on" : ""}`}
          onClick={toggleCompact}
          aria-pressed={compact}
          title={compact ? t("activity.densityComfortable") : t("activity.densityCompact")}
        >
          <Icon name="rows" size={14} />
          <span>{t("activity.densityLabel")}</span>
        </button>
      </div>
      <div className="panel-sub">{t("activity.subtitle")}</div>
      {deck.chat.length === 0 ? (
        <PanelEmpty icon="list" title={t("activity.empty")} hint={t("activity.emptyHint")} />
      ) : visibleChat.length === 0 ? (
        <PanelEmpty icon="list" title={t("activity.filterEmpty")} hint={t("activity.filterEmptyHint")} />
      ) : (
        <div className={`activity-feed ${compact ? "compact" : ""}`}>
          {visibleChat.map((m, i) => {
            const text = m.i18nKey ? t(m.i18nKey, m.i18nVars) : m.content;
            const isWorker = !!m.solverId && isWorkerLane(m.solverId);
            const isReview = isWorker && isReviewWorkerLane(deck.lanes[m.solverId!]);
            const color = isWorker ? workerColor(m.solverId!, undefined) : undefined;
            // Group consecutive rows from the same speaker within a short window:
            // suppress the repeated header (avatar + who line) for a denser log.
            const prev = visibleChat[i - 1];
            const grouped = !!prev
              && speakerKey(prev) === speakerKey(m)
              && Math.abs(tsMs(m.ts) - tsMs(prev.ts)) <= GROUP_WINDOW_MS;
            return (
              <div key={m.id} className={`act-msg ${rowClass(m)}${grouped ? " grouped" : ""}${isReview ? " review-worker" : ""}`}
                title={relTime(m.ts, lang) || undefined}
                style={color ? ({ "--wc": color } as CSSProperties) : undefined}>
                <div className="act-ico" aria-hidden="true">
                  {m.role === "human" ? "你"
                    : m.kind === "insight" ? <Icon name="radio" size={15} />
                    : m.kind === "tool" ? <Icon name="terminal" size={15} />
                    : <Icon name="dot" size={11} />}
                </div>
                <div className="act-main">
                  <div className="act-who">
                    {speaker(m)}
                    {isReview && <span className="worker-role-chip review">{t("worker.role.review")}</span>}
                    <span className="k">{isWorker ? workerEngine(m.solverId!) : t(`msg.kind.${m.kind}`)}</span>
                    {clock(m.ts) && <span className="ts">{clock(m.ts)}</span>}
                  </div>
                  <div className="act-body">{text}</div>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
      {unread > 0 && (
        <button
          type="button"
          className="jump-newest"
          onClick={jumpToBottom}
          title={t("activity.jumpNewest")}
        >
          <Icon name="chevronDown" size={14} />
          {t("activity.jumpNewestCount", { n: unread })}
        </button>
      )}
    </div>
  );
}
