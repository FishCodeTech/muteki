"use client";

import { createContext, useContext, useMemo, type ReactNode } from "react";

import { formatRelativeTime, formatRunOffset, toEpochMs } from "@/lib/format";
import { useLang, useT } from "@/lib/i18n";
import { SECOND_TICKER, useTicker } from "@/lib/ticker";

type StampMode = { live: boolean; finished: boolean; origin: number };

const StampModeContext = createContext<StampMode>({ live: false, finished: false, origin: 0 });

/** Run clock for knowledge / queue / activity stamps (relative while live, +offset once finished). */
export function EventStampProvider({
  live,
  finished,
  origin,
  children,
}: {
  live: boolean;
  finished: boolean;
  origin?: number;
  children: ReactNode;
}) {
  const value = useMemo<StampMode>(
    () => ({ live, finished, origin: toEpochMs(origin) }),
    [live, finished, origin],
  );
  return <StampModeContext.Provider value={value}>{children}</StampModeContext.Provider>;
}

/**
 * Live relative time via the shared second ticker; finished runs show +M:SS
 * from `origin` (date-prefixed when the event is on another calendar day).
 */
export function EventStamp({ ts }: { ts?: number | null }) {
  const t = useT();
  const { lang } = useLang();
  const { live, finished, origin } = useContext(StampModeContext);
  const ms = toEpochMs(ts);
  const tick = useTicker(SECOND_TICKER, live && ms > 0);
  if (!ms) return "—";
  const text = finished && origin
    ? formatRunOffset(ms, origin, lang)
    : formatRelativeTime(ms, t, live ? tick : Date.now());
  const absolute = new Date(ms).toLocaleString(lang === "zh" ? "zh-CN" : "en-US");
  return <time dateTime={new Date(ms).toISOString()} title={absolute}>{text}</time>;
}
