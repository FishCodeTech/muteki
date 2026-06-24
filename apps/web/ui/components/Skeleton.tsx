"use client";

import type { CSSProperties } from "react";

/**
 * Shimmer skeleton primitives for the "still rehydrating" gap — when a run is
 * SELECTED and the rail says it started, but the SSE replay hasn't folded its
 * first event yet (deck still empty). Renders calm placeholder blocks instead of
 * a flash of "no data". The shimmer is pure CSS (.skel); prefers-reduced-motion
 * collapses it to a static muted block (see globals.css). Theme-token-driven.
 */

/** A single shimmer line. `w` is any CSS width (e.g. "60%", 120). */
export function SkelLine({ w, h, style }: { w?: string | number; h?: number; style?: CSSProperties }) {
  return (
    <span
      className="skel skel-line"
      style={{ width: w ?? "100%", ...(h ? { height: h } : {}), ...style }}
      aria-hidden="true"
    />
  );
}

/** A shimmer block (cards / tiles / avatars). */
export function SkelBox({ w, h, r, style }: { w?: string | number; h?: string | number; r?: number; style?: CSSProperties }) {
  return (
    <span
      className="skel skel-box"
      style={{ width: w ?? "100%", height: h ?? 40, ...(r != null ? { borderRadius: r } : {}), ...style }}
      aria-hidden="true"
    />
  );
}

/** The right-column inspector skeleton: flag block, chip row, worker mini-rows. */
export function InspectorSkeleton() {
  return (
    <div className="insp-skel" aria-hidden="true">
      <div className="insp-skel-sec">
        <SkelLine w={56} h={9} style={{ marginBottom: 4 }} />
        <SkelBox h={42} r={9} />
        <div className="insp-skel-chips">
          {Array.from({ length: 5 }).map((_, i) => (
            <SkelBox key={i} w={62 + (i % 3) * 16} h={22} r={999} />
          ))}
        </div>
      </div>
      <div className="insp-skel-sec">
        <SkelLine w={70} h={9} style={{ marginBottom: 4 }} />
        {Array.from({ length: 3 }).map((_, i) => (
          <div className="insp-skel-row" key={i}>
            <SkelBox w={28} h={28} r={7} />
            <div className="insp-skel-row-meta">
              <SkelLine w={`${64 - i * 8}%`} h={10} />
              <SkelLine w={`${40 + i * 6}%`} h={8} />
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

/** A generic secondary-panel skeleton: a few stacked card rows. */
export function PanelSkeleton({ rows = 4 }: { rows?: number }) {
  return (
    <div className="panel-skel" aria-hidden="true">
      {Array.from({ length: rows }).map((_, i) => (
        <div className="panel-skel-row" key={i}>
          <div className="panel-skel-head">
            <SkelBox w={22} h={22} r={6} />
            <SkelLine w={`${52 - i * 4}%`} h={11} />
            <SkelLine w={40} h={9} style={{ marginLeft: "auto" }} />
          </div>
          <SkelLine w="100%" h={10} />
          <SkelLine w={`${72 - i * 6}%`} h={10} />
        </div>
      ))}
    </div>
  );
}
