"use client";

import { ReactNode, useId, useState } from "react";
import type { CSSProperties } from "react";
import { Icon } from "@/components/Icon";

export function ChipFilterBar({
  id,
  label,
  summary,
  title,
  expandTitle,
  collapseTitle,
  className = "",
  variant = "inline",
  maxRows = 3,
  children,
  clearLabel,
  showClear = false,
  onClear,
}: {
  id?: string;
  label: string;
  summary: string;
  title: string;
  expandTitle: string;
  collapseTitle: string;
  className?: string;
  variant?: "inline" | "floating";
  maxRows?: number;
  children: ReactNode;
  clearLabel?: string;
  showClear?: boolean;
  onClear?: () => void;
}) {
  const autoId = useId();
  const panelId = id || `chip-filter-${autoId}`;
  const [open, setOpen] = useState(false);
  const style = { "--chip-filter-rows": maxRows } as CSSProperties;

  return (
    <div className={`chip-filterbar ${variant} ${open ? "open" : ""} ${className}`} style={style}>
      <button
        type="button"
        className="chip-filter-toggle"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        aria-controls={panelId}
        title={open ? collapseTitle : expandTitle}
      >
        <span className="chip-filter-label">{label}</span>
        <span className="chip-filter-summary">{summary}</span>
        <Icon name="chevronDown" size={14} />
      </button>
      {open && (
        <div id={panelId} className="chip-filter-panel">
          <div className="chip-filter-strip" aria-label={title}>
            {children}
          </div>
          {showClear && clearLabel && onClear && (
            <button type="button" className="chip-filter-clear" onClick={onClear}>
              {clearLabel}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
