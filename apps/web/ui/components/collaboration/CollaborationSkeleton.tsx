"use client";

import { Skeleton } from "@heroui/react";
import { useLayoutEffect, useRef, useState, type CSSProperties } from "react";

import { modeFromWidth } from "./useCollaborationLayout";

function Line({ width, height = 10, style }: { width: string | number; height?: number; style?: CSSProperties }) {
  return <Skeleton className="skel-line t-skeleton" style={{ width, height, ...style }} />;
}

function Box({ width, height, radius = 8 }: { width: number; height: number; radius?: number }) {
  return <Skeleton className="skel-box t-skeleton" style={{ width, height, borderRadius: radius }} />;
}

function Rows({ count }: { count: number }) {
  return (
    <>
      {Array.from({ length: count }).map((_, index) => (
        <div className="collab-skeleton-row" key={index}>
          <Box width={27} height={27} radius={7} />
          <span>
            <Line width={`${44 - index * 4}%`} height={8} />
            <Line width="100%" height={10} />
            <Line width={`${68 - index * 6}%`} height={8} />
          </span>
        </div>
      ))}
    </>
  );
}

/**
 * Loading placeholder for the dynamically imported canvas: the same grid as
 * .collab-shell (toolbar / index / canvas / inspector) so the layout does not
 * jump when the chunk lands.
 */
export function CollaborationSkeleton() {
  // Same rule as useCollaborationLayout: side panels open only when the shell
  // itself is wide, so the skeleton does not paint overlays or an empty row.
  const ref = useRef<HTMLDivElement>(null);
  const [wide, setWide] = useState(true);
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const apply = (width: number) => setWide(modeFromWidth(width) === "wide");
    apply(element.getBoundingClientRect().width);
    const observer = new ResizeObserver((entries) => {
      apply(entries[0]?.contentRect.width ?? element.getBoundingClientRect().width);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return (
    <div ref={ref} className={`collab-shell collab-skeleton mobile-canvas ${wide ? "index-open detail-open" : ""}`} aria-hidden="true">
      <div className="collab-toolbar">
        <Skeleton className="skel-box t-skeleton collab-skeleton-search" style={{ height: 34, borderRadius: 9 }} />
        <Box width={150} height={34} radius={9} />
        <Box width={96} height={34} radius={9} />
        <Box width={96} height={34} radius={9} />
        <Box width={96} height={34} radius={9} />
        <span className="collab-skeleton-spacer" />
        <Box width={34} height={34} radius={9} />
        <Box width={88} height={34} radius={9} />
        <Box width={34} height={34} radius={9} />
      </div>
      <aside className="collab-index">
        <div className="collab-panel-head">
          <Line width={96} height={11} />
        </div>
        <section className="collab-index-section queue">
          <h2><Line width={64} height={9} /><Line width={20} height={12} /></h2>
          <div className="collab-queue-list"><Rows count={2} /></div>
        </section>
        <section className="collab-index-section knowledge">
          <h2><Line width={72} height={9} /><Line width={20} height={12} /></h2>
          <div className="collab-index-list"><Rows count={4} /></div>
        </section>
      </aside>
      <main className="collab-canvas" />
      <aside className="collab-inspector">
        <div className="collab-inspector-head">
          <Box width={36} height={36} radius={9} />
          <span>
            <Line width="70%" height={12} />
            <Line width="52%" height={9} />
          </span>
          <Box width={44} height={18} radius={6} />
        </div>
        <div className="collab-inspector-tabs">
          <Line width="100%" height={29} style={{ borderRadius: 6 }} />
          <Line width="100%" height={29} style={{ borderRadius: 6 }} />
          <Line width="100%" height={29} style={{ borderRadius: 6 }} />
          <Line width="100%" height={29} style={{ borderRadius: 6 }} />
        </div>
        <div className="collab-inspector-body">
          <section className="collab-detail-section">
            <Line width={72} height={9} />
            <Line width="100%" height={64} style={{ borderRadius: 9 }} />
          </section>
          <section className="collab-detail-section">
            <Line width={72} height={9} />
            <div className="collab-metric-grid">
              <Line width="100%" height={46} style={{ borderRadius: 8 }} />
              <Line width="100%" height={46} style={{ borderRadius: 8 }} />
              <Line width="100%" height={46} style={{ borderRadius: 8 }} />
            </div>
          </section>
        </div>
      </aside>
    </div>
  );
}
