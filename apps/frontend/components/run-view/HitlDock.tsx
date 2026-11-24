"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  hitlDeliveryState,
  isHitlRecoveryAttention,
  partitionHitlDeck,
  type HitlDeliveryPhase,
  type HitlHistoryEntry,
  type HitlRequest,
  type HitlRequestGroup,
} from "@/lib/events";
import { commandIdForDecision, type DecisionControlAction } from "@/lib/controlClient";
import { formatClock } from "@/lib/format";
import { useT } from "@/lib/i18n";
import { isImeComposingKeyEvent } from "@/lib/imeKeyboard";
import { Badge, Button, Icon, Input, type AlertTone } from "@muteki/ui";

export interface HitlSessionAttempt {
  action: DecisionControlAction;
  value: string;
  commandId: string;
}

function shortId(id: string): string {
  return id.length > 16 ? `${id.slice(0, 12)}…` : id;
}

function deliveryBadgeTone(phase: HitlDeliveryPhase): AlertTone {
  if (phase === "observed") return "success";
  if (phase === "failed" || phase === "unknown") return "danger";
  if (phase === "partial" || phase === "rejected") return "warning";
  return "info";
}

function recoveringStatusKey(phase: HitlDeliveryPhase): string {
  if (phase === "partial") return "hitl.status.partial";
  if (phase === "failed") return "hitl.status.failed";
  if (phase === "unknown") return "hitl.status.unknown";
  if (phase === "rejected") return "hitl.status.rejected";
  if (phase === "observed") return "hitl.status.answered";
  return "hitl.status.delivering";
}

function historyResolutionBadge(entry: HitlHistoryEntry): { key: string; tone: AlertTone } {
  if (entry.resolutionAction === "dismiss" || entry.resolutionAction === "dismiss_help") {
    return { key: "hitl.status.dismissed", tone: "neutral" };
  }
  if (entry.resolutionAction === "run_finished" || entry.resolutionAction === "run_reopened") {
    return { key: "hitl.status.archived", tone: "neutral" };
  }
  return { key: "hitl.status.answered", tone: "success" };
}

function HitlCard({
  req,
  first,
  onAnswer,
  onDismiss,
  getCommandId,
  sessionAttempt,
  onRecordAttempt,
  onRetryAttempt,
  retrying,
}: {
  req: HitlRequest;
  first: boolean;
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
  getCommandId: (requestId: string, action: DecisionControlAction) => string;
  sessionAttempt: HitlSessionAttempt | null;
  onRecordAttempt: (requestId: string, attempt: HitlSessionAttempt) => void;
  onRetryAttempt: (requestId: string) => Promise<void>;
  retrying: boolean;
}) {
  const t = useT();
  const [free, setFree] = useState("");
  const [sending, setSending] = useState(false);
  const [locallyRecorded, setLocallyRecorded] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);
  const hasOptions = req.options.length > 0;
  const pauses = req.pausesBehavior ?? true;
  const kindLabel = req.needKind ? t(`hitl.kind.${req.needKind}`) : t("hitl.title");
  const durableDelivery = hitlDeliveryState(req);
  const answerRecorded = locallyRecorded || durableDelivery.locked;
  const deliveryPhase: HitlDeliveryPhase = durableDelivery.locked
    ? durableDelivery.phase
    : locallyRecorded
      ? "pending"
      : "open";
  const clockText = formatClock(req.ts, "");

  useEffect(() => {
    if (first && pauses && !answerRecorded) inputRef.current?.focus();
  }, [first, pauses, answerRecorded]);

  useEffect(() => {
    if (durableDelivery.locked) {
      setLocallyRecorded(true);
      setSending(false);
    }
  }, [durableDelivery.locked]);

  const submit = async (value: string) => {
    const v = value.trim();
    if (!v || sending || answerRecorded) return;
    const commandId = getCommandId(req.id, "answer_decision");
    onRecordAttempt(req.id, { action: "answer_decision", value: v, commandId });
    setSending(true);
    const ok = await onAnswer(req.id, v, commandId);
    setSending(false);
    if (ok) setLocallyRecorded(true);
  };

  const dismiss = async () => {
    if (!onDismiss || sending || answerRecorded) return;
    const commandId = getCommandId(req.id, "dismiss");
    onRecordAttempt(req.id, { action: "dismiss", value: "", commandId });
    setSending(true);
    const ok = await onDismiss(req.id, commandId);
    setSending(false);
    if (ok) setLocallyRecorded(true);
  };

  const deliveryKey = deliveryPhase === "open" ? undefined : `hitl.delivery.${deliveryPhase}`;
  const retryable = answerRecorded && deliveryPhase !== "observed" && sessionAttempt !== null;
  const activeCommandId = durableDelivery.commandId ?? sessionAttempt?.commandId;

  if (!pauses) {
    return (
      <div
        className="hitl-card auto rv-notice"
        role="note"
        aria-label={kindLabel}
        title={req.promptZh && req.promptZh !== req.prompt ? req.prompt : undefined}
      >
        <span className="rv-notice-ico" aria-hidden="true"><Icon name="help" size={13} /></span>
        <span className="rv-notice-main">
          <span className="rv-notice-head">
            <b>{kindLabel}</b>
            <span className="rv-notice-state">{t("hitl.autoResolving")}</span>
            {req.worker && <span className="hitl-meta-chip">{req.worker}</span>}
          </span>
          <span className="rv-notice-text">{req.promptZh || req.prompt}</span>
        </span>
      </div>
    );
  }

  return (
    <div
      className={`hitl-card ${first ? "first" : ""} ${sending || retrying ? "sending" : ""} ${answerRecorded ? "answered-readonly" : ""} delivery-${deliveryPhase} blocking`}
      role="group"
      aria-label={t("hitl.region")}
      data-testid="hitl-card"
      data-request-id={req.id}
    >
      <div className="hitl-head">
        <span className="hitl-ico" aria-hidden="true">
          <Icon name={answerRecorded && deliveryPhase === "pending" ? "clock" : "alert"} size={15} />
        </span>
        <span className="hitl-title">{kindLabel}</span>
        {answerRecorded ? (
          <Badge tone={deliveryBadgeTone(deliveryPhase)} variant="soft">
            {t(recoveringStatusKey(deliveryPhase))}
          </Badge>
        ) : (
          <span className="hitl-blocking">{t("hitl.titleBlocking")}</span>
        )}
        <span className="hitl-meta-chips">
          {req.worker && (
            <span className="hitl-meta-chip">
              <Icon name="cpu" size={11} />
              <span>{req.worker}</span>
            </span>
          )}
          <span className="hitl-meta-chip hitl-meta-id" title={req.id}>{shortId(req.id)}</span>
          {clockText && <time className="hitl-meta-time">{clockText}</time>}
        </span>
      </div>
      <div className="body">{req.promptZh || req.prompt}</div>
      {req.promptZh && req.promptZh !== req.prompt && (
        <details className="hitl-raw">
          <summary>{t("hitl.showOriginal")}</summary>
          <div>{req.prompt}</div>
        </details>
      )}
      {answerRecorded && deliveryKey && (
        <div
          className={`hitl-delivery-state ${deliveryPhase}`}
          role="status"
          aria-live="polite"
          data-command-id={activeCommandId}
          data-tooltip={durableDelivery.detail}
        >
          <Icon name={deliveryPhase === "pending" || deliveryPhase === "observed" ? "clock" : "alert"} size={14} />
          <span className="hitl-delivery-copy">{t(deliveryKey)}</span>
          {retryable && (
            <Button
              size="sm"
              variant="outline"
              className="hitl-delivery-retry"
              disabled={sending || retrying}
              onClick={() => void onRetryAttempt(req.id)}
            >
              {sending || retrying ? t("hitl.delivery.retrying") : t("hitl.delivery.retry")}
            </Button>
          )}
        </div>
      )}
      {!answerRecorded && (
        <div className="hitl-opts">
          {req.options.map((o) => (
            <Button key={o} size="sm" variant="outline" disabled={sending} onClick={() => void submit(o)}>
              {o}
            </Button>
          ))}
          <Input
            ref={inputRef}
            className="hitl-free"
            size="sm"
            value={free}
            disabled={sending}
            aria-label={t("hitl.inputAria")}
            placeholder={hasOptions ? t("hitl.orType") : t("hitl.inputAria")}
            onChange={(e) => setFree(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !isImeComposingKeyEvent(e)) {
                e.preventDefault();
                void submit(free);
              }
            }}
          />
          <Button
            size="sm"
            variant="primary"
            className="hitl-send"
            disabled={sending || !free.trim()}
            onClick={() => void submit(free)}
          >
            {sending ? t("hitl.sending") : t("hitl.submit")}
          </Button>
          {onDismiss && (
            <Button
              size="sm"
              variant="ghost"
              className="hitl-dismiss"
              disabled={sending}
              aria-label={t("hitl.dismiss.tip")}
              onClick={() => void dismiss()}
            >
              {t("hitl.dismiss")}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}

function RecoveringRow({
  req,
  expanded,
  onToggle,
  sessionAttempt,
  onRetryAttempt,
  retrying,
  onOpenControlHistory,
}: {
  req: HitlRequest;
  expanded: boolean;
  onToggle: () => void;
  sessionAttempt: HitlSessionAttempt | null;
  onRetryAttempt: (requestId: string) => Promise<void>;
  retrying: boolean;
  onOpenControlHistory?: () => void;
}) {
  const t = useT();
  const delivery = hitlDeliveryState(req);
  const phase = delivery.phase;
  const attention = isHitlRecoveryAttention(req);
  const displayText = req.promptZh || req.prompt;
  const clockText = formatClock(req.ts, "");

  return (
    <div
      className={`hitl-compact-row phase-${phase}${expanded ? " is-open" : ""}`}
      data-testid="hitl-recovering-row"
      data-request-id={req.id}
      data-delivery-phase={phase}
    >
      <div className="hitl-compact-main">
        <Badge tone={deliveryBadgeTone(phase)} variant="soft">
          {t(recoveringStatusKey(phase))}
        </Badge>
        {req.worker && (
          <span className="hitl-meta-chip">
            <Icon name="cpu" size={11} />
            <span>{req.worker}</span>
          </span>
        )}
        <span className="hitl-compact-summary" title={displayText}>{displayText}</span>
        {clockText && <time className="hitl-meta-time">{clockText}</time>}
        <div className="hitl-compact-actions">
          {sessionAttempt && phase !== "observed" && (
            <Button
              size="xs"
              variant="outline"
              className="hitl-compact-btn"
              disabled={retrying}
              onClick={() => void onRetryAttempt(req.id)}
            >
              {retrying ? t("hitl.delivery.retrying") : t("hitl.delivery.retry")}
            </Button>
          )}
          {attention && onOpenControlHistory && (
            <Button
              size="xs"
              variant="ghost"
              className="hitl-compact-btn"
              onClick={onOpenControlHistory}
            >
              {t("hitl.action.viewControlLog")}
            </Button>
          )}
          <Button
            size="xs"
            variant="ghost"
            className="hitl-compact-btn"
            aria-expanded={expanded}
            onClick={onToggle}
          >
            {expanded ? t("hitl.action.collapse") : t("hitl.action.expand")}
          </Button>
        </div>
      </div>
      {expanded && (
        <div className="hitl-compact-detail">
          <div className="hitl-detail-body">{displayText}</div>
          {req.promptZh && req.promptZh !== req.prompt && (
            <details className="hitl-raw">
              <summary>{t("hitl.showOriginal")}</summary>
              <div>{req.prompt}</div>
            </details>
          )}
          <div
            className={`hitl-delivery-state ${phase}`}
            role="status"
            aria-live="polite"
            data-command-id={req.deliveryCommandId}
          >
            <Icon name={phase === "pending" || phase === "observed" ? "clock" : "alert"} size={14} />
            <span className="hitl-delivery-copy">
              {t(`hitl.delivery.${phase}`)}
              {req.deliveryDetail ? ` — ${req.deliveryDetail}` : ""}
            </span>
          </div>
          <div className="hitl-detail-meta">
            <span>{t("hitl.meta.requestId")}: <code>{req.id}</code></span>
            {req.deliveryCommandId && (
              <span>{t("hitl.meta.commandId")}: <code>{req.deliveryCommandId}</code></span>
            )}
            {!attention && onOpenControlHistory && (
              <Button size="xs" variant="ghost" onClick={onOpenControlHistory}>
                {t("hitl.action.viewControlLog")}
              </Button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

function groupHeadline(group: HitlRequestGroup): string {
  const first = group.items[0];
  return group.topic.label || (first ? (first.promptZh || first.prompt) : "");
}

export function HitlDock({
  hitlRequests,
  hitlHistory,
  onAnswer,
  onDismiss,
  onOpenControlHistory,
}: {
  hitlRequests: HitlRequest[];
  hitlHistory: HitlHistoryEntry[];
  onAnswer: (requestId: string, opt: string, commandId: string) => Promise<boolean>;
  onDismiss?: (requestId: string, commandId: string) => Promise<boolean>;
  onOpenControlHistory?: () => void;
}) {
  const t = useT();
  const [historyOpen, setHistoryOpen] = useState(false);
  const [pendingOpen, setPendingOpen] = useState(true);
  const [recoveringOpen, setRecoveringOpen] = useState(true);
  const [expandedRecoveringIds, setExpandedRecoveringIds] = useState<Record<string, boolean>>({});
  const [expandedPendingIds, setExpandedPendingIds] = useState<Record<string, boolean>>({});
  const [expandedPendingGroups, setExpandedPendingGroups] = useState<Record<string, boolean>>({});
  const [retryingIds, setRetryingIds] = useState<Record<string, boolean>>({});
  const commandIdsRef = useRef<Record<string, Partial<Record<DecisionControlAction, string>>>>({});
  const attemptsRef = useRef<Record<string, HitlSessionAttempt>>({});

  const partition = useMemo(
    () => partitionHitlDeck(hitlRequests, hitlHistory),
    [hitlRequests, hitlHistory],
  );

  if (
    partition.pendingCount === 0
    && partition.recoveringCount === 0
    && partition.auto.length === 0
    && partition.historyCount === 0
  ) {
    return null;
  }

  const getCommandId = (requestId: string, action: DecisionControlAction): string => {
    const bucket = commandIdsRef.current[requestId] ?? (commandIdsRef.current[requestId] = {});
    return commandIdForDecision(bucket, action);
  };

  const recordAttempt = (requestId: string, attempt: HitlSessionAttempt) => {
    attemptsRef.current[requestId] = attempt;
  };

  const retryAttempt = async (requestId: string) => {
    const attempt = attemptsRef.current[requestId];
    if (!attempt || retryingIds[requestId]) return;
    setRetryingIds((prev) => ({ ...prev, [requestId]: true }));
    try {
      if (attempt.action === "answer_decision") {
        await onAnswer(requestId, attempt.value, attempt.commandId);
      } else if (onDismiss) {
        await onDismiss(requestId, attempt.commandId);
      }
    } finally {
      setRetryingIds((prev) => {
        const next = { ...prev };
        delete next[requestId];
        return next;
      });
    }
  };

  const toggleRecovering = (id: string) => {
    setExpandedRecoveringIds((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const togglePendingItem = (id: string) => {
    setExpandedPendingIds((prev) => ({ ...prev, [id]: !prev[id] }));
  };

  const togglePendingGroup = (key: string, current: boolean) => {
    setExpandedPendingGroups((prev) => ({ ...prev, [key]: !current }));
  };

  const renderCard = (req: HitlRequest, first: boolean) => (
    <HitlCard
      key={req.id}
      req={req}
      first={first}
      onAnswer={onAnswer}
      onDismiss={onDismiss}
      getCommandId={getCommandId}
      sessionAttempt={attemptsRef.current[req.id] ?? null}
      onRecordAttempt={recordAttempt}
      onRetryAttempt={retryAttempt}
      retrying={Boolean(retryingIds[req.id])}
    />
  );

  const collapseSecondaryPendingGroups = partition.pendingGroups.length >= 3;
  const reversedHistory = partition.historyCount > 0 ? [...partition.history].reverse() : [];

  return (
    <section className="hitl-dock" aria-label={t("hitl.region")} data-testid="hitl-dock">
      {partition.historyCount > 0 && (
        <div className="hitl-history" data-testid="hitl-history-section">
          <button
            type="button"
            className="hitl-history-toggle"
            aria-expanded={historyOpen}
            data-testid="hitl-history-toggle"
            onClick={() => setHistoryOpen((open) => !open)}
          >
            <Icon name="history" size={13} />
            <span className="hitl-history-label">{t("hitl.section.history", { n: partition.historyCount })}</span>
            <span className="hitl-history-action">
              {historyOpen ? t("hitl.action.collapse") : t("hitl.action.expand")}
            </span>
            <Icon name={historyOpen ? "chevronUp" : "chevronDown"} size={12} />
          </button>
          {historyOpen && (
            <div className="hitl-history-list" data-testid="hitl-history-list">
              {reversedHistory.map((entry) => {
                const badge = historyResolutionBadge(entry);
                const kindLabel = entry.needKind ? t(`hitl.kind.${entry.needKind}`) : t("hitl.title");
                const clockText = formatClock(entry.resolvedTs || entry.ts, "");
                return (
                  <div className="hitl-history-item" key={entry.id} data-request-id={entry.id}>
                    <div className="hitl-history-head">
                      <span className="hitl-history-kind">{kindLabel}</span>
                      <Badge tone={badge.tone} variant="soft">{t(badge.key)}</Badge>
                      {entry.worker && (
                        <span className="hitl-meta-chip">
                          <Icon name="cpu" size={11} />
                          <span>{entry.worker}</span>
                        </span>
                      )}
                      {clockText && <time className="hitl-meta-time">{clockText}</time>}
                    </div>
                    <div className="hitl-history-body">{entry.promptZh || entry.prompt}</div>
                    {entry.promptZh && entry.promptZh !== entry.prompt && (
                      <details className="hitl-raw">
                        <summary>{t("hitl.showOriginal")}</summary>
                        <div>{entry.prompt}</div>
                      </details>
                    )}
                    <div className="hitl-detail-meta">
                      <span>{t("hitl.meta.requestId")}: <code>{entry.id}</code></span>
                      {entry.deliveryCommandId && (
                        <span>{t("hitl.meta.commandId")}: <code>{entry.deliveryCommandId}</code></span>
                      )}
                      {entry.deliveryMode && (
                        <span>{t("hitl.meta.deliveryMode")}: <code>{entry.deliveryMode}</code></span>
                      )}
                      {entry.deliveryDetail && <span>{entry.deliveryDetail}</span>}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}

      {partition.auto.map((req) => renderCard(req, false))}

      {partition.recoveringCount > 0 && (
        <div
          className={`hitl-section hitl-section-recovering${partition.recoveringNeedsAttentionCount > 0 ? " needs-attention" : ""}`}
          data-testid="hitl-recovering-section"
        >
          <button
            type="button"
            className="hitl-section-head"
            aria-expanded={recoveringOpen}
            onClick={() => setRecoveringOpen((open) => !open)}
          >
            <Icon
              name={partition.recoveringNeedsAttentionCount > 0 ? "alert" : "clock"}
              size={13}
              className="hitl-section-ico"
            />
            <span className="hitl-section-title">
              {partition.recoveringNeedsAttentionCount > 0
                ? t("hitl.section.recoveringAttention", { n: partition.recoveringNeedsAttentionCount })
                : t("hitl.section.recovering", { n: partition.recoveringCount })}
            </span>
            <span className="hitl-section-action">
              {recoveringOpen ? t("hitl.action.collapse") : t("hitl.action.expand")}
            </span>
            <Icon name={recoveringOpen ? "chevronUp" : "chevronDown"} size={12} />
          </button>
          {recoveringOpen && (
            <div className="hitl-section-body">
              {partition.recoveringGroups.map((group) => {
                const headline = groupHeadline(group);
                return (
                  <div className="hitl-recovering-group" key={group.key}>
                    {group.items.length > 1 && (
                      <div className="hitl-group-subhead">
                        <span className="hitl-group-title" title={group.topic.detail || headline}>{headline}</span>
                        <Badge tone="neutral" variant="soft">{t("hitl.group.count", { n: group.items.length })}</Badge>
                      </div>
                    )}
                    {group.items.map((req) => (
                      <RecoveringRow
                        key={req.id}
                        req={req}
                        expanded={Boolean(expandedRecoveringIds[req.id])}
                        onToggle={() => toggleRecovering(req.id)}
                        sessionAttempt={attemptsRef.current[req.id] ?? null}
                        onRetryAttempt={retryAttempt}
                        retrying={Boolean(retryingIds[req.id])}
                        onOpenControlHistory={onOpenControlHistory}
                      />
                    ))}
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}

      {partition.pendingCount === 1 && renderCard(partition.pending[0], true)}

      {partition.pendingCount > 1 && (
        <div className="hitl-section hitl-section-pending" data-testid="hitl-pending-section">
          <button
            type="button"
            className="hitl-section-head"
            aria-expanded={pendingOpen}
            onClick={() => setPendingOpen((open) => !open)}
          >
            <Icon name="alert" size={13} className="hitl-section-ico" />
            <span className="hitl-section-title">{t("hitl.section.pending", { n: partition.pendingCount })}</span>
            <span className="hitl-section-action">
              {pendingOpen ? t("hitl.action.collapse") : t("hitl.action.expand")}
            </span>
            <Icon name={pendingOpen ? "chevronUp" : "chevronDown"} size={12} />
          </button>
          {pendingOpen && (
            <div className="hitl-section-body">
              {partition.pendingGroups.map((group, groupIndex) => {
                const groupOpen = !collapseSecondaryPendingGroups
                  || (expandedPendingGroups[group.key] ?? groupIndex === 0);
                const headline = groupHeadline(group);
                if (!groupOpen) {
                  return (
                    <div className="hitl-compact-row phase-open" key={group.key} data-testid="hitl-group-collapsed">
                      <div className="hitl-compact-main">
                        <Badge tone="warning" variant="outline">{t(`hitl.kind.${group.needKind}`)}</Badge>
                        {group.items.length > 1 && (
                          <Badge tone="warning" variant="soft">{t("hitl.group.count", { n: group.items.length })}</Badge>
                        )}
                        <span className="hitl-compact-summary" title={group.topic.detail || headline}>{headline}</span>
                        {group.workers.length > 0 && (
                          <span className="hitl-meta-chip">{group.workers.join(" · ")}</span>
                        )}
                        <Button
                          size="xs"
                          variant="outline"
                          className="hitl-compact-btn"
                          onClick={() => togglePendingGroup(group.key, false)}
                        >
                          {t("hitl.action.expand")}
                        </Button>
                      </div>
                    </div>
                  );
                }
                if (group.items.length === 1) {
                  return renderCard(group.items[0], groupIndex === 0);
                }
                return (
                  <div className="hitl-group" key={group.key} data-testid="hitl-group">
                    <div className="hitl-group-head">
                      <span className="hitl-group-title" title={group.topic.detail || headline}>{headline}</span>
                      <Badge tone="warning" variant="soft">{t("hitl.group.count", { n: group.items.length })}</Badge>
                      {group.workers.length > 0 && (
                        <span className="hitl-group-workers">{group.workers.join(" · ")}</span>
                      )}
                      <span className="hitl-group-note">{t("hitl.group.independentNote")}</span>
                    </div>
                    <div className="hitl-group-items">
                      {renderCard(group.items[0], groupIndex === 0)}
                      {group.items.slice(1).map((item) => {
                        const expanded = Boolean(expandedPendingIds[item.id]);
                        if (expanded) {
                          return (
                            <div className="hitl-group-item-expanded" key={item.id}>
                              <div className="hitl-group-item-bar">
                                <span className="hitl-meta-chip">{item.worker || shortId(item.id)}</span>
                                <Button size="xs" variant="ghost" onClick={() => togglePendingItem(item.id)}>
                                  {t("hitl.action.collapse")}
                                </Button>
                              </div>
                              {renderCard(item, false)}
                            </div>
                          );
                        }
                        const itemText = item.promptZh || item.prompt;
                        const clockText = formatClock(item.ts, "");
                        return (
                          <div
                            className="hitl-compact-row phase-open"
                            key={item.id}
                            data-testid="hitl-pending-sibling"
                            data-request-id={item.id}
                          >
                            <div className="hitl-compact-main">
                              <Badge tone="warning" variant="outline">{t(item.needKind ? `hitl.kind.${item.needKind}` : "hitl.title")}</Badge>
                              {item.worker && (
                                <span className="hitl-meta-chip">
                                  <Icon name="cpu" size={11} />
                                  <span>{item.worker}</span>
                                </span>
                              )}
                              <span className="hitl-compact-summary" title={itemText}>{itemText}</span>
                              {clockText && <time className="hitl-meta-time">{clockText}</time>}
                              <Button
                                size="xs"
                                variant="outline"
                                className="hitl-compact-btn"
                                onClick={() => togglePendingItem(item.id)}
                              >
                                {t("hitl.action.answerItem")}
                              </Button>
                            </div>
                          </div>
                        );
                      })}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </div>
      )}
    </section>
  );
}
