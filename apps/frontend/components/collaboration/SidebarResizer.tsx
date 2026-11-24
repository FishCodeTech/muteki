"use client";

import type { KeyboardEvent, PointerEvent } from "react";

import { COLLAB_SIDEBAR_MAX, COLLAB_SIDEBAR_MIN } from "./useCollaborationSidebarResize";

export function SidebarResizer({
  label,
  value,
  className,
  onPointerDown,
  onKeyDown,
  onReset,
}: {
  label: string;
  value: number;
  className: string;
  onPointerDown: (event: PointerEvent<HTMLDivElement>) => void;
  onKeyDown: (event: KeyboardEvent<HTMLDivElement>) => void;
  onReset: () => void;
}) {
  return (
    <div
      className={`collab-sidebar-resizer ${className}`}
      role="separator"
      tabIndex={0}
      aria-orientation="vertical"
      aria-label={label}
      aria-valuemin={COLLAB_SIDEBAR_MIN}
      aria-valuemax={COLLAB_SIDEBAR_MAX}
      aria-valuenow={value}
      onPointerDown={onPointerDown}
      onKeyDown={onKeyDown}
      onDoubleClick={onReset}
    />
  );
}
