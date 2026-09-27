"use client";

import { useT } from "@/lib/i18n";

export function EdgeHoverTip({
  x,
  y,
  source,
  target,
  count,
  titles,
  reduceMotion,
}: {
  x: number;
  y: number;
  source: string;
  target: string;
  count: number;
  titles: string[];
  reduceMotion: boolean;
}) {
  const t = useT();
  return (
    <div
      className={`collab-edge-tip collab-selected-knowledge${reduceMotion ? " instant" : ""}`}
      style={{ left: x, top: y }}
      role="status"
    >
      <strong>{source} → {target}</strong>
      <p>{t("collab.edgeTip.count", { n: count })}</p>
      {titles.map((title, index) => (
        <p key={index}>{title}</p>
      ))}
    </div>
  );
}
