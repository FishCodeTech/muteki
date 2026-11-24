"use client";

import { AgentRoster, type AgentRosterSeat } from "@muteki/ui/agent";

import { useEffect, useState } from "react";
import { getWorkerSettings, type WorkerSettings } from "@/lib/useRun";
import type { DeckState } from "@/lib/events";
import { dispatchRoster, isDispatchDraft } from "@/lib/dispatchRoster";
import { useT } from "@/lib/i18n";
import { workerEngine } from "@/lib/workers";
import { EngineLogo } from "@/components/EngineLogo";

export function EngineBar({ deck, protectedMode = false }: { deck: DeckState; protectedMode?: boolean }) {
  const t = useT();
  const draft = isDispatchDraft(deck.runId);
  const [snapshot, setSnapshot] = useState<{ runId: string; config: WorkerSettings | null; failed: boolean } | null>(null);
  useEffect(() => {
    if (!draft) return;
    let alive = true;
    let pending = false;
    const load = async () => {
      if (pending) return;
      pending = true;
      const config = await getWorkerSettings();
      pending = false;
      if (alive) setSnapshot({ runId: deck.runId, config, failed: !config });
    };
    void load();
    const interval = window.setInterval(load, 30000);
    window.addEventListener("focus", load);
    return () => { alive = false; window.clearInterval(interval); window.removeEventListener("focus", load); };
  }, [draft, deck.runId]);

  const current = snapshot?.runId === deck.runId ? snapshot : null;
  const rows = dispatchRoster(deck, current?.config || null);
  const seats: AgentRosterSeat[] = rows.map(row => {
    const engineName = workerEngine(row.engine, row.engine);
    const named = row.label && ![row.key, row.engine, engineName].includes(row.label);
    const disabledByGuard = protectedMode && row.engine !== "pi";
    return {
      id: row.key,
      name: named ? row.label : engineName,
      detail: disabledByGuard ? "生产保护模式已禁用" : row.model,
      mark: (
        <span className={disabledByGuard ? "opacity-40 grayscale" : undefined}>
          <EngineLogo engine={row.engine} size={14} title={engineName} />
        </span>
      ),
      state: disabledByGuard ? "offline" : row.state === "configured" ? "idle" : row.state,
      stateLabel: disabledByGuard ? "已禁用" : draft ? undefined : t(`engines.roster.${row.state}`),
      note: disabledByGuard ? "外部 CLI 引擎仅在非保护模式可用" : row.detail,
    };
  });
  const total = seats.length;
  const up = rows.filter(row => row.state === "online").length;
  const emptyLabel = draft
    ? current ? t(current.failed ? "engines.rosterFailed" : "engines.rosterEmpty") : t("engines.rosterLoading")
    : t("engines.rosterNotDispatched");
  const title = t("engines.rosterTitle");

  return (
    <AgentRoster
      seats={seats}
      label={t("engines.rosterLabel")}
      title={title}
      summary={!draft && total ? t("engines.rosterOnline", { up, total }) : undefined}
      emptyLabel={emptyLabel}
      emptyTone={draft && current ? "warning" : "muted"}
      ariaLabel={total ? t("engines.rosterCount", { n: total }) : `${title}：${emptyLabel}`}
    />
  );
}
