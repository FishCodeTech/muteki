"use client";

import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { IconButton } from "@muteki/ui";
import { cn } from "@/lib/cn";
import {
  MAX_VIEWPORT,
  MIN_VIEWPORT,
  clampViewport,
  devicePreset,
  deviceSize,
  type DeviceSettings,
} from "./devices";

function SizeField({ value, label, onCommit }: { value: number; label: string; onCommit: (value: number) => void }) {
  const [draft, setDraft] = useState(String(value));
  const cancelled = useRef(false);
  useEffect(() => setDraft(String(value)), [value]);
  const commit = () => {
    if (cancelled.current) { cancelled.current = false; return; }
    const next = clampViewport(Number(draft), value);
    setDraft(String(next));
    if (next !== value) onCommit(next);
  };
  return (
    <input
      type="text"
      inputMode="numeric"
      aria-label={label}
      value={draft}
      onChange={(event) => setDraft(event.target.value.replace(/\D/g, "").slice(0, 4))}
      onBlur={commit}
      onKeyDown={(event) => {
        if (event.key === "Enter") event.currentTarget.blur();
        if (event.key === "Escape") { cancelled.current = true; setDraft(String(value)); event.currentTarget.blur(); }
      }}
      className="cx-tabular h-7 w-[58px] rounded-md border border-cx-border-subtle bg-cx-bg px-2 text-center text-cx-xs text-cx-fg outline-none focus:border-cx-accent"
    />
  );
}

export function ViewportBar({ settings, scale, onChange }: { settings: DeviceSettings; scale: number; onChange: (next: DeviceSettings) => void }) {
  const size = deviceSize(settings);
  if (!size) return null;
  const preset = devicePreset(settings.device);
  const custom = (width: number, height: number): DeviceSettings => ({ ...settings, device: "custom", width, height });
  return (
    <div className="flex h-9 shrink-0 items-center gap-2 border-b border-cx-border-subtle px-3 text-cx-xs text-cx-fg-3">
      <span className="shrink-0">{preset.label}</span>
      <SizeField label="宽度" value={size.width} onCommit={(width) => onChange(custom(width, size.height))} />
      <span aria-hidden>×</span>
      <SizeField label="高度" value={size.height} onCommit={(height) => onChange(custom(size.width, height))} />
      <IconButton
        icon="rotateCw"
        label="交换宽高"
        onClick={() => onChange(settings.device === "custom" ? custom(size.height, size.width) : { ...settings, rotated: !settings.rotated })}
      />
      <span className="cx-tabular ml-auto shrink-0 select-none text-cx-fg-4">{Math.round(scale * 100)}%</span>
      <IconButton icon="x" label="退出设备模拟" onClick={() => onChange({ ...settings, device: "responsive" })} />
    </div>
  );
}

type Edge = "right" | "bottom" | "corner";

export function ViewportHandles({
  settings,
  scale,
  onChange,
  onDragging,
}: {
  settings: DeviceSettings;
  scale: number;
  onChange: (next: DeviceSettings) => void;
  onDragging: (dragging: boolean) => void;
}) {
  const size = deviceSize(settings);
  const drag = useRef<{ edge: Edge; x: number; y: number; width: number; height: number; scale: number } | null>(null);
  if (!size) return null;

  const start = (edge: Edge) => (event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    drag.current = { edge, x: event.clientX, y: event.clientY, width: size.width, height: size.height, scale: Math.max(scale, 0.1) };
    onDragging(true);
  };
  const move = (event: ReactPointerEvent<HTMLDivElement>) => {
    const state = drag.current;
    if (!state) return;
    const width = state.edge === "bottom" ? state.width : clampViewport(state.width + (event.clientX - state.x) / state.scale, state.width);
    const height = state.edge === "right" ? state.height : clampViewport(state.height + (event.clientY - state.y) / state.scale, state.height);
    if (width !== size.width || height !== size.height) onChange({ ...settings, device: "custom", width, height });
  };
  const end = (event: ReactPointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    drag.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId);
    onDragging(false);
  };
  const handlers = { onPointerMove: move, onPointerUp: end, onPointerCancel: end };
  const grip = "absolute z-10 grid place-items-center touch-none select-none";
  const bar = "rounded-full bg-cx-border-strong transition-colors group-hover/handle:bg-cx-accent";
  return (
    <>
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label={`调整宽度（${MIN_VIEWPORT}–${MAX_VIEWPORT}）`}
        onPointerDown={start("right")}
        {...handlers}
        className={cn(grip, "group/handle inset-y-0 -right-4 w-4 cursor-ew-resize")}
      >
        <span className={cn(bar, "h-10 w-1")} />
      </div>
      <div
        role="separator"
        aria-orientation="horizontal"
        aria-label={`调整高度（${MIN_VIEWPORT}–${MAX_VIEWPORT}）`}
        onPointerDown={start("bottom")}
        {...handlers}
        className={cn(grip, "group/handle inset-x-0 -bottom-4 h-4 cursor-ns-resize")}
      >
        <span className={cn(bar, "h-1 w-10")} />
      </div>
      <div
        role="separator"
        aria-label="同时调整宽高"
        onPointerDown={start("corner")}
        {...handlers}
        className={cn(grip, "group/handle -bottom-4 -right-4 size-4 cursor-nwse-resize")}
      >
        <span className={cn(bar, "size-1.5")} />
      </div>
    </>
  );
}
