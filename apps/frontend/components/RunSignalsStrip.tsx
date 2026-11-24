"use client";

import { useMemo, useState } from "react";
import {
  DeckState,
  verifiedFactTexts,
  candidateFactTexts,
  deadEndTexts,
  workerIds,
  type ArtifactView,
} from "@/lib/events";
import { PricingIssues } from "@/components/usage/PricingIssues";
import type { PricingIssue } from "@/lib/modelPricing";
import { runUsageAmount, runUsageTokens, useRunUsageLedger } from "@/lib/useRunUsageLedger";
import { useT } from "@/lib/i18n";
import { canvasModeOf } from "@/lib/swarmProjection";
import {
  workerEngine,
  toWorkerIdentity,
  actorDisplayTitle,
} from "@/lib/workers";
import { Icon, type IconName } from "@muteki/ui";
import { Button, Card, Chip, Popover, type Tone } from "@muteki/ui";

export type InspectorSignal =
  | "verified" | "candidates" | "intents" | "dead" | "cost"
  | "facts" | "steps" | "goals" | "flags";

export type SignalCostRow = {
  id: string;
  label: string;
  engine: string;
  usd: number | null;
  tokens: number | null;
  tokenLabel: string;
  amount: string;
  amountSource: string;
  pricingIssues?: PricingIssue[];
};

function compactNumber(value: number): string {
  if (value < 1000) return String(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(1)}k`;
  return `${(value / 1_000_000).toFixed(2)}m`;
}

function getSignalChipColor(signal: InspectorSignal): Tone {
  switch (signal) {
    case "verified":
      return "success";
    case "candidates":
      return "warning";
    case "intents":
      return "accent";
    case "dead":
      return "danger";
    case "facts":
      return "success";
    case "steps":
      return "accent";
    case "goals":
      return "accent";
    case "flags":
      return "warning";
    case "cost":
      return "neutral";
    default: {
      const _never: never = signal;
      return _never;
    }
  }
}

export interface RunSignalsStripProps {
  deck: DeckState;
  onOpenArtifact: (view: ArtifactView) => void;
  onOpenKnowledge?: (id: string) => void;
  className?: string;
}

export function RunSignalsStrip({
  deck,
  onOpenArtifact,
  onOpenKnowledge,
  className = "",
}: RunSignalsStripProps) {
  const t = useT();
  const isCtf = canvasModeOf(deck) === "ctf";
  const isFgs = isCtf || deck.mode === "pentest";
  const [activeSignal, setActiveSignal] = useState<InspectorSignal | null>(null);

  const verifiedItems = verifiedFactTexts(deck);
  const candidateItems = candidateFactTexts(deck);
  const deadItems = deadEndTexts(deck);
  const openIntentItems = useMemo(() => {
    const doneIds = new Set(deck.blackboard.intents.filter((intent) => intent.status === "done").map((intent) => intent.id));
    const items: Array<{ id: string; text: string }> = [];
    const seen = new Set<string>();
    for (const intent of deck.blackboard.intents) {
      if (intent.status === "done" || (intent.dispatchState ?? "active") !== "active") continue;
      const text = intent.summary || intent.goal;
      if (seen.has(text)) continue;
      seen.add(text);
      items.push({ id: intent.id, text });
    }
    for (const intent of deck.reason.intents) {
      if (doneIds.has(intent.id) || seen.has(intent.goal)) continue;
      seen.add(intent.goal);
      items.push({ id: intent.id, text: intent.goal });
    }
    return items;
  }, [deck]);

  const verified = verifiedItems.length;
  const candidates = candidateItems.length;
  const intents = openIntentItems.length;
  const deads = deadItems.length;
  const factItems = deck.mode === "pentest"
    ? deck.blackboard.facts.filter((fact) => fact.verified && fact.actor !== "origin").map((fact) => fact.fact)
    : [...verifiedFactTexts(deck), ...deadEndTexts(deck)];
  const stepItems = useMemo(() => {
    const seen = new Set<string>();
    return deck.blackboard.intents.flatMap((intent) => {
      const text = intent.summary || intent.goal || intent.id;
      if (!text || seen.has(text)) return [];
      seen.add(text);
      return [text];
    });
  }, [deck.blackboard.intents]);
  const goalItems = useMemo(() => {
    const seen = new Set<string>();
    const values = [
      (deck.taskContract?.completion.goal || "").trim() || t("insp.run.taskGoal"),
      ...deck.blackboard.subGoals.map((goal) => goal.criterion),
    ];
    return values.filter((value) => {
      if (!value || seen.has(value)) return false;
      seen.add(value);
      return true;
    });
  }, [deck.blackboard.subGoals, deck.taskContract?.completion.goal, t]);
  const flagItems = deck.flagConfirmations.length
    ? deck.flagConfirmations.map((row) => {
      switch (row.status) {
        case "accepted":
          return row.title || t("insp.run.platformAccepted");
        case "rejected":
          return row.title || t("insp.run.platformRejected");
        case "pending":
          return row.title || t("insp.run.platformPending");
        case "internal":
          return row.title || t("insp.run.platformInternal");
        default: {
          const _never: never = row.status;
          return _never;
        }
      }
    })
    : deck.flags.map(() => t("insp.run.platformInternal"));

  const workerSiblings = useMemo(
    () => workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id])),
    [deck],
  );

  const { data: usage, error: usageError } = useRunUsageLedger(deck.runId);
  const ledger = usage?.totals;
  const ledgerByWorker = useMemo(() => Object.fromEntries(
    (usage?.groups?.worker_id ?? []).map(row => [row.name, row]),
  ), [usage]);

  const costRows = useMemo<SignalCostRow[]>(
    () => [...new Set([...Object.keys(deck.costBySolver), ...Object.keys(ledgerByWorker).filter(id => id !== "未上报")])]
      .map(id => {
        const cost = deck.costBySolver[id];
        const priced = ledgerByWorker[id];
        const display = priced ? runUsageAmount(priced) : {
          amount: usageError ? "读取失败" : ledger ? "未上报" : "读取中", source: "",
        };
        return {
          id,
          label: actorDisplayTitle(id, t, toWorkerIdentity(id, deck.lanes[id]), workerSiblings),
          engine: cost?.engine || workerEngine(id, deck.lanes[id]?.engine),
          usd: priced?.cost ?? null,
          tokens: priced?.total_tokens ?? null,
          tokenLabel: runUsageTokens(priced, compactNumber),
          amount: display.amount,
          amountSource: display.source,
          pricingIssues: priced?.pricing_issues,
        };
      })
      .sort((a, b) => (b.usd ?? -1) - (a.usd ?? -1) || (b.tokens ?? -1) - (a.tokens ?? -1)),
    [deck.costBySolver, deck.lanes, ledger, ledgerByWorker, t, usageError, workerSiblings],
  );
  const unassignedPricingIssues = useMemo(() => {
    const byRole = new Map<string, PricingIssue[]>();
    for (const issue of ledger?.pricing_issues ?? []) {
      if (issue.worker_id) continue;
      const role = issue.role || "auxiliary";
      byRole.set(role, [...(byRole.get(role) ?? []), issue]);
    }
    return [...byRole];
  }, [ledger]);
  const totalTokens = ledger?.total_tokens ?? null;
  const totalTokenLabel = runUsageTokens(ledger, compactNumber);
  const rawTotalCost = ledger ? runUsageAmount(ledger) : {
    amount: usageError ? "读取失败" : "读取中", source: "",
  };
  const totalCost = ledger && usageError
    ? { amount: rawTotalCost.amount, source: [rawTotalCost.source, "刷新失败（旧值）"].filter(Boolean).join(" · ") }
    : rawTotalCost;

  const signalItems: Record<InspectorSignal, string[]> = {
    verified: verifiedItems,
    candidates: candidateItems,
    intents: openIntentItems.map((item) => item.text),
    dead: deadItems,
    facts: factItems,
    steps: stepItems,
    goals: goalItems,
    flags: flagItems,
    cost: [],
  };

  const signalTargets: Record<InspectorSignal, { view: ArtifactView; label: string }> = {
    verified: { view: "evidence", label: t("insp.signal.openEvidence") },
    candidates: { view: "evidence", label: t("insp.signal.openEvidence") },
    intents: { view: "collaboration", label: t("insp.signal.openCollaboration") },
    dead: { view: "evidence", label: t("insp.signal.openEvidence") },
    facts: { view: "evidence", label: t("insp.signal.openEvidence") },
    steps: { view: "collaboration", label: t("insp.signal.openCollaboration") },
    goals: { view: "evidence", label: t("insp.signal.openEvidence") },
    flags: { view: "evidence", label: t("insp.signal.openEvidence") },
    cost: { view: "workers", label: t("insp.signal.openWorkers") },
  };

  const costSignal = {
    key: "cost" as const,
    icon: "terminal" as const,
    label: "记账金额",
    value: ledger?.cost != null
      ? `${ledger.estimated_cost != null ? "≈" : ""}$${ledger.cost.toFixed(4)}`
      : totalCost.amount,
    count: costRows.length + unassignedPricingIssues.length,
    iconBoxTone: "bg-ink/10 text-cx-fg-2",
    cardTone: "hover:border-accent/40 hover:bg-cx-bg-subtle border-cx-border/60 bg-cx-elevated/80",
    activeCardTone: "border-accent/60 bg-accent/10 ring-1 ring-accent/30",
    tooltip: `记账金额：${totalCost.amount}${totalCost.source ? `（${totalCost.source}）` : ""} · ${totalTokenLabel} ${t("meta.tokens")}`,
  };
  const signals: Array<{
    key: InspectorSignal;
    icon: IconName;
    label: string;
    value: string;
    count: number;
    iconBoxTone: string;
    cardTone: string;
    activeCardTone: string;
    tooltip: string;
  }> = isFgs
    ? [
        {
          key: "facts",
          icon: "check",
          label: t("meta.facts"),
          value: String(factItems.length),
          count: factItems.length,
          iconBoxTone: "bg-green/15 text-green",
          cardTone: "hover:border-green/40 hover:bg-green/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-green/60 bg-green/10 ring-1 ring-green/30",
          tooltip: `${t("meta.facts")}: ${factItems.length}`,
        },
        {
          key: "steps",
          icon: "crosshair",
          label: t("meta.steps"),
          value: String(stepItems.length),
          count: stepItems.length,
          iconBoxTone: "bg-accent/15 text-accent",
          cardTone: "hover:border-accent/40 hover:bg-accent/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-accent/60 bg-accent/10 ring-1 ring-accent/30",
          tooltip: `${t("meta.steps")}: ${stepItems.length}`,
        },
        {
          key: "goals",
          icon: deck.mode === "pentest" ? "target" : "flag",
          label: t("meta.goals"),
          value: String(goalItems.length),
          count: goalItems.length,
          iconBoxTone: "bg-accent/15 text-accent",
          cardTone: "hover:border-accent/40 hover:bg-accent/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-accent/60 bg-accent/10 ring-1 ring-accent/30",
          tooltip: `${t("meta.goals")}: ${goalItems.length}`,
        },
        ...(isCtf ? [{
          key: "flags",
          icon: "flag",
          label: t("meta.flags"),
          value: String(flagItems.length),
          count: flagItems.length,
          iconBoxTone: "bg-amber/15 text-amber",
          cardTone: "hover:border-amber/40 hover:bg-amber/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-amber/60 bg-amber/10 ring-1 ring-amber/30",
          tooltip: `${t("meta.flags")}: ${flagItems.length}`,
        } as const] : []),
        costSignal,
      ]
    : [
        {
          key: "verified",
          icon: "check",
          label: t("meta.verified"),
          value: String(verified),
          count: verified,
          iconBoxTone: "bg-green/15 text-green",
          cardTone: "hover:border-green/40 hover:bg-green/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-green/60 bg-green/10 ring-1 ring-green/30",
          tooltip: `${t("meta.verified")}: ${verified}`,
        },
        {
          key: "candidates",
          icon: "help",
          label: t("meta.candidates"),
          value: String(candidates),
          count: candidates,
          iconBoxTone: "bg-amber/15 text-amber",
          cardTone: "hover:border-amber/40 hover:bg-amber/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-amber/60 bg-amber/10 ring-1 ring-amber/30",
          tooltip: `${t("meta.candidates")}: ${candidates}`,
        },
        {
          key: "intents",
          icon: "crosshair",
          label: t("meta.intents"),
          value: String(intents),
          count: intents,
          iconBoxTone: "bg-accent/15 text-accent",
          cardTone: "hover:border-accent/40 hover:bg-accent/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-accent/60 bg-accent/10 ring-1 ring-accent/30",
          tooltip: `${t("meta.intents")}: ${intents}`,
        },
        {
          key: "dead",
          icon: "xCircle",
          label: t("meta.dead"),
          value: String(deads),
          count: deads,
          iconBoxTone: "bg-red/15 text-red",
          cardTone: "hover:border-red/40 hover:bg-red/5 border-cx-border/60 bg-cx-elevated/80",
          activeCardTone: "border-red/60 bg-red/10 ring-1 ring-red/30",
          tooltip: `${t("meta.dead")}: ${deads}`,
        },
        costSignal,
      ];

  return (
    <div className={`run-signals-strip-shell ${className}`}>
      <Card
        variant="elevated"
        className="run-signals-strip p-1.5 rounded-xl border-0 ring-1 ring-cx-ring bg-cx-elevated/65 backdrop-blur-sm shadow-cx-xs"
      >
        <div className="grid grid-cols-5 gap-1.5 w-full items-center" role="group" aria-label={t("insp.signal.title")}>
          {signals.map((sig) => {
            const isOpen = activeSignal === sig.key;
            const target = signalTargets[sig.key];
            const items = signalItems[sig.key];
            const visibleItems = items.slice(0, 4);
            const visibleCosts = costRows;
            const remaining =
              sig.key === "cost"
                ? Math.max(0, costRows.length - visibleCosts.length)
                : Math.max(0, items.length - visibleItems.length);
            const empty = sig.key === "cost" ? visibleCosts.length === 0 && unassignedPricingIssues.length === 0 : visibleItems.length === 0;

            return (
              <Popover
                key={sig.key}
                open={isOpen}
                onOpenChange={(open) => setActiveSignal(open ? sig.key : null)}
                placement="bottom"
                ariaLabel={sig.label}
                className="w-80 overflow-hidden p-0"
                trigger={
                  <button
                    type="button"
                    className={`flex-1 min-w-0 h-8 px-2 flex items-center justify-between gap-1.5 rounded-lg border transition-[color,background-color,border-color,opacity] text-cx-fg select-none cursor-pointer group focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] ${
                      isOpen ? sig.activeCardTone : sig.cardTone
                    }`}
                    data-tooltip={sig.tooltip}
                    aria-label={sig.tooltip}
                    aria-expanded={isOpen}
                  >
                    <div className={`flex items-center gap-1.5 ${sig.key === "cost" ? "shrink-0" : "min-w-0"}`}>
                      <div className={`size-4.5 rounded flex items-center justify-center shrink-0 ${sig.iconBoxTone}`}>
                        <Icon name={sig.icon} size={11} />
                      </div>
                      <span className={sig.key === "cost" ? "sr-only" : "text-cx-2xs font-medium text-cx-fg-2 group-hover:text-cx-fg truncate transition-colors"}>
                        {sig.label}
                      </span>
                    </div>
                    <div className={`flex items-center gap-1 ${sig.key === "cost" ? "min-w-0 flex-1 justify-end" : "shrink-0"}`}>
                      <div className={sig.key === "cost" ? "min-w-0 text-right leading-tight" : "contents"}>
                        <span className={`text-cx-xs font-semibold font-mono text-cx-fg tabular-nums ${sig.key === "cost" ? "block truncate leading-tight" : ""}`}>
                          {sig.value}
                        </span>
                        {sig.key === "cost" && ledger?.cost != null && ledger.unpriced > 0 ? <span className="block truncate text-cx-2xs leading-tight text-cx-fg-2">部分未定价</span> : null}
                      </div>
                      <Icon
                        name={isOpen ? "chevronDown" : "chevronRight"}
                        size={10}
                        className={`shrink-0 text-cx-fg-4 group-hover:text-cx-fg transition-[color,transform] ${
                          isOpen ? "text-accent" : "[@media(hover:hover)_and_(pointer:fine)]:group-hover:translate-x-0.5"
                        }`}
                      />
                    </div>
                  </button>
                }
              >
                  <div className="focus:outline-none">
                    <div className="flex items-center justify-between gap-2 px-3 py-2 border-b border-cx-border bg-cx-bg-subtle/70">
                      <div className="flex items-center gap-2 min-w-0">
                        <div className={`size-5 rounded flex items-center justify-center shrink-0 ${sig.iconBoxTone}`}>
                          <Icon name={sig.icon} size={12} />
                        </div>
                        <span className="text-cx-xs font-semibold text-cx-fg truncate">
                          {sig.label}
                        </span>
                        <Chip
                          size="xs"
                          tone={getSignalChipColor(sig.key)}
                          variant="soft"
                          className="h-4.5 px-1.5 text-cx-2xs font-mono font-semibold"
                        >
                          {sig.count}
                        </Chip>
                      </div>
                      <Button
                        size="sm"
                        variant="ghost"
                        iconOnly
                        onClick={() => setActiveSignal(null)}
                        aria-label={t("settings.close")}
                        className="size-5.5 text-cx-fg-3 hover:text-cx-fg hover:bg-cx-hover rounded-md transition-colors"
                      >
                        <Icon name="x" size={12} />
                      </Button>
                    </div>

                    <div className="p-0 max-h-60 overflow-y-auto">
                      {sig.key === "cost" && (
                        <div className="flex flex-col gap-1.5 px-3 py-2 border-b border-cx-border/60 bg-cx-bg-subtle/50">
                          {(["solve", "auxiliary"] as const).map(scope => {
                            const bucket = usage?.buckets?.[scope];
                            const display = bucket ? runUsageAmount(bucket) : null;
                            return <div key={scope} className="flex flex-wrap items-baseline justify-between gap-2 text-cx-2xs">
                              <span>{t(`usage.bucket.${scope}`)}</span>
                              <span>{display?.amount || (usageError ? "读取失败" : "暂无分桶数据")} {display?.source}</span>
                              <span>{runUsageTokens(bucket, n => n.toLocaleString())} tokens</span>
                            </div>;
                          })}
                          <button className="self-start text-cx-2xs text-cx-fg-3" onClick={() => onOpenArtifact("usage")}>用量明细 · {totalCost.source || "金额口径"}</button>
                          <div className="flex flex-wrap items-baseline justify-between gap-x-2 gap-y-1">
                            <b className="whitespace-nowrap text-cx-sm font-semibold font-mono text-cx-fg">{totalCost.amount}</b>
                            <small className="whitespace-nowrap text-cx-2xs font-mono text-cx-fg-3">
                              {totalTokens == null ? totalTokenLabel : `${totalTokenLabel} ${t("meta.tokens")}`}
                            </small>
                          </div>
                        </div>
                      )}

                      {empty ? (
                        <div className="py-6 px-3 text-center text-cx-2xs text-cx-fg-3">
                          {t("insp.signal.empty")}
                        </div>
                      ) : sig.key === "cost" ? (
                        <div className="divide-y divide-cx-border-subtle/40">
                          {visibleCosts.map((row) => (
                            <div
                              className="flex flex-col gap-1.5 px-3 py-2 hover:bg-cx-bg-subtle/50 transition-colors"
                              key={row.id}
                            >
                              <div className="flex items-start justify-between gap-3">
                                <div className="flex flex-col min-w-0 gap-0.5">
                                <b className="text-cx-2xs font-medium text-cx-fg truncate max-w-[140px]">{row.label}</b>
                                <small className="text-cx-2xs font-mono text-cx-fg-3">{row.engine}</small>
                              </div>
                              <div className="flex flex-col items-end shrink-0 gap-0.5 text-right">
                                <b className="text-cx-2xs font-mono font-semibold text-cx-fg">{row.amount}</b>
                                {row.amountSource ? <small className="text-cx-2xs text-cx-fg-3">{row.amountSource}</small> : null}
                                <small className="text-cx-2xs font-mono text-cx-fg-3">
                                  {row.tokenLabel}{row.tokens != null ? ` ${t("meta.tokens")}` : ""}
                                </small>
                              </div>
                              </div>
                              <PricingIssues issues={row.pricingIssues} />
                            </div>
                          ))}
                          {unassignedPricingIssues.map(([role, issues]) => <div key={`pricing:${role}`} className="flex flex-col gap-1.5 px-3 py-2"><b className="text-cx-2xs font-medium text-cx-fg">{{ reason: "协调器", planner: "协调器", dispatch: "任务标题", titler: "标题 / 摘要", auxiliary: "辅助调用" }[role] || role}</b><PricingIssues issues={issues} /></div>)}
                        </div>
                      ) : sig.key === "intents" || sig.key === "steps" ? (
                        <ol className="divide-y divide-cx-border-subtle/40 m-0 p-0 list-none">
                          {(sig.key === "intents"
                              ? openIntentItems
                              : stepItems.map((text, i) => ({ id: `step-${i}`, text })))
                            .slice(0, 4)
                            .map((item, index) => (
                            <li key={item.id} className="m-0 p-0">
                              <button
                                type="button"
                                className="flex w-full items-start gap-2.5 px-3 py-2 text-left hover:bg-cx-bg-subtle/50 transition-colors"
                                onClick={() => {
                                  setActiveSignal(null);
                                  if (sig.key === "intents" && onOpenKnowledge) onOpenKnowledge(`intent:${item.id}`);
                                  else onOpenArtifact("collaboration");
                                }}
                              >
                                <span className="size-4 rounded-full bg-cx-bg-subtle border border-cx-border text-cx-2xs font-mono text-cx-fg-3 flex items-center justify-center shrink-0 mt-0.5">
                                  {index + 1}
                                </span>
                                <span className="text-cx-2xs text-cx-fg-2 leading-relaxed break-words flex-1 min-w-0">
                                  {item.text}
                                </span>
                              </button>
                            </li>
                          ))}
                        </ol>
                      ) : (
                        <ol className="divide-y divide-cx-border-subtle/40 m-0 p-0 list-none">
                          {visibleItems.map((item, index) => (
                            <li
                              key={`${index}-${item}`}
                              className="flex items-start gap-2.5 px-3 py-2 hover:bg-cx-bg-subtle/50 transition-colors"
                            >
                              <span className="size-4 rounded-full bg-cx-bg-subtle border border-cx-border text-cx-2xs font-mono text-cx-fg-3 flex items-center justify-center shrink-0 mt-0.5">
                                {index + 1}
                              </span>
                              <span className="text-cx-2xs text-cx-fg-2 leading-relaxed break-words flex-1 min-w-0">
                                {item}
                              </span>
                            </li>
                          ))}
                        </ol>
                      )}
                    </div>

                    <div className="flex items-center justify-between gap-2 px-3 py-2 border-t border-cx-border bg-cx-bg-subtle/70">
                      <span className="text-cx-2xs text-cx-fg-3">
                        {remaining > 0 ? t("insp.signal.more", { n: remaining }) : ""}
                      </span>
                      <Button
                        size="sm"
                        variant="ghost"
                        onClick={() => {
                          setActiveSignal(null);
                          onOpenArtifact(target.view);
                        }}
                        className="h-6 px-2 text-cx-2xs font-medium text-accent hover:text-accent hover:bg-accent/10 rounded-md transition-colors flex items-center gap-1"
                      >
                        <span>{target.label}</span>
                        <Icon name="chevronRight" size={11} />
                      </Button>
                    </div>
                  </div>
              </Popover>
            );
          })}
        </div>
      </Card>
    </div>
  );
}
