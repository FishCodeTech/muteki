"use client";

import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";

const MAX_CELLS = 4;

export function RosterAvatar({ engines, size = 22, className, title }: {
  engines: string[];
  size?: number;
  className?: string;
  title?: string;
}) {
  const shown = engines.length > MAX_CELLS ? engines.slice(0, MAX_CELLS - 1) : engines;
  const rest = engines.length - shown.length;
  const count = shown.length + (rest > 0 ? 1 : 0);
  const pad = Math.max(1.5, size * 0.09);
  const gap = Math.max(1, size * 0.06);
  const cell = count <= 1 ? size - pad * 2 : (size - pad * 2 - gap) / 2;
  const far = pad + cell + gap;
  const centre = (size - cell) / 2;
  const slots: [number, number][] = count <= 1
    ? [[pad, pad]]
    : count === 2
      ? [[pad, centre], [far, centre]]
      : count === 3
        ? [[centre, pad], [pad, far], [far, far]]
        : [[pad, pad], [far, pad], [pad, far], [far, far]];
  return (
    <span
      role={title ? "img" : undefined}
      aria-label={title}
      aria-hidden={title ? undefined : true}
      className={cn("relative inline-block shrink-0 overflow-hidden rounded-[28%] bg-cx-hover ring-1 ring-inset ring-cx-border-subtle", className)}
      style={{ width: size, height: size }}
    >
      {count === 0 ? (
        <span className="absolute inset-0 grid place-items-center text-cx-fg-4"><Icon name="grid" size={Math.round(size * 0.55)} /></span>
      ) : null}
      {shown.map((engine, index) => (
        <span
          key={`${engine}-${index}`}
          className="absolute grid place-items-center rounded-[30%] bg-cx-bg"
          style={{ left: slots[index][0], top: slots[index][1], width: cell, height: cell }}
        >
          <EngineLogo engine={engine} size={Math.max(6, Math.round(cell * 0.78))} />
        </span>
      ))}
      {rest > 0 ? (
        <span
          className="absolute grid place-items-center rounded-[30%] bg-cx-bg font-semibold leading-none text-cx-fg-3"
          style={{ left: slots[count - 1][0], top: slots[count - 1][1], width: cell, height: cell, fontSize: Math.max(6, Math.round(cell * 0.5)) }}
        >
          +{rest}
        </span>
      ) : null}
    </span>
  );
}
