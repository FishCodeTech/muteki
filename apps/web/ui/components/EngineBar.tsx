"use client";

import { useCallback, useEffect, useId, useRef, useState } from "react";
import { useEngines, EngineStatus } from "@/lib/useRun";
import { useT } from "@/lib/i18n";
import { workerEngine } from "@/lib/workers";

/** Why an engine is degraded, or "" when it's fine. Two independent signals:
 *  · run-scoped: the current run dropped it at dispatch time (degradedEngines).
 *  · global: the /api/engines deep probe says it can't complete a turn (healthy
 *    === false) — covers the no-active-run case.
 *  The run-scoped reason wins (it's the one the operator just hit). */
function degradeReason(e: EngineStatus, runDegraded?: Record<string, string>): string {
  const fromRun = (e.profile_id && runDegraded?.[e.profile_id]) || runDegraded?.[e.engine];
  if (fromRun) return fromRun;
  if (e.healthy === false) return e.health_detail || "health check failed";
  return "";
}

function engineLabel(engine: string): string {
  const label = workerEngine(engine, engine);
  return label === "Worker" ? engine : label;
}

function rowKey(e: EngineStatus): string {
  return e.profile_id || e.profile_name || e.engine;
}

function workerLabel(e: EngineStatus, engineName: string): string {
  const name = (e.profile_name || "").trim();
  if (!name || name.toLowerCase() === engineName.toLowerCase()) return "";
  return name;
}

type EngineRow = {
  key: string;
  engine: string;
  label: string;
  worker: string;
  ok: boolean;
  degraded: string;
  cls: "up" | "down" | "down degraded";
};

export function EngineBar({ degradedEngines }: { degradedEngines?: Record<string, string> } = {}) {
  const t = useT();
  const engines = useEngines();
  const [open, setOpen] = useState(false);
  const [dismissed, setDismissed] = useState(false);
  const rootRef = useRef<HTMLSpanElement>(null);
  const popId = useId();

  const close = useCallback(() => {
    setOpen(false);
    setDismissed(true);
  }, []);

  useEffect(() => {
    if (!open && !dismissed) return;
    const onPointer = (event: PointerEvent) => {
      if (!rootRef.current?.contains(event.target as Node)) close();
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, dismissed, close]);

  if (engines.length === 0) return null;

  const rows: EngineRow[] = engines.map((e) => {
    const degraded = degradeReason(e, degradedEngines);
    const ok = !degraded && e.available;
    const label = engineLabel(e.engine);
    return {
      key: rowKey(e),
      engine: e.engine,
      label,
      worker: workerLabel(e, label),
      ok,
      degraded,
      cls: degraded ? "down degraded" : e.available ? "up" : "down",
    };
  });
  const total = rows.length;
  const up = rows.filter((row) => row.ok).length;
  const down = total - up;
  const downRows = rows.filter((row) => !row.ok);
  const barState = down === 0 ? "all-up" : up === 0 ? "all-down" : "mixed";
  const summary = down === 0 ? String(total) : `${up}/${total}`;
  const exception = down === 0 || up === 0
    ? null
    : down === 1
      ? (downRows[0].worker || downRows[0].label)
      : t("engines.downCount", { n: down });
  const aria = down === 0
    ? t("engines.summaryUp", { n: total })
    : t("engines.summaryMixed", { up, total });

  return (
    <span
      ref={rootRef}
      className={`engine-bar ${barState}${open ? " open" : ""}${dismissed ? " dismissed" : ""}`}
      onMouseLeave={() => setDismissed(false)}
    >
      <button
        type="button"
        className="engine-chip"
        aria-haspopup="true"
        aria-expanded={open}
        aria-controls={popId}
        aria-label={aria}
        title={aria}
        onClick={() => {
          if (open) close();
          else {
            setDismissed(false);
            setOpen(true);
          }
        }}
      >
        <span className="engine-cluster" aria-hidden="true">
          {rows.map((row) => (
            <span key={row.key} className={`engine-dot ${row.cls}`} />
          ))}
        </span>
        <span className="engine-count">{summary}</span>
        {exception ? <span className="engine-exception">{exception}</span> : null}
      </button>
      <div id={popId} className="engine-pop" role="region" aria-label={t("engines.popoverTitle")}>
        <span className="engine-pop-head">
          <b>{t("engines.popoverTitle")}</b>
          <span className="engine-pop-sub">{up}/{total}</span>
        </span>
        <ul className="engine-pop-list">
          {rows.map((row) => (
            <li key={row.key} className={`engine-pop-row ${row.cls}`}>
              <span className={`engine-dot ${row.cls}`} aria-hidden="true" />
              <span className="engine-pop-label">
                <strong>{row.label}</strong>
                {row.worker ? <em>{row.worker}</em> : null}
              </span>
              <span className="engine-pop-status">
                {row.ok ? t("engines.up") : t("engines.down")}
              </span>
              {row.degraded ? (
                <span className="engine-pop-note engine-pop-degraded">{row.degraded}</span>
              ) : null}
            </li>
          ))}
        </ul>
      </div>
    </span>
  );
}
