import type { CollaborationKnowledgeKind } from "./agentCollaboration";

export type StatusTranslate = (key: string, vars?: Record<string, string | number>) => string;

const INTENT_STATUS_KEY: Record<string, string> = {
  active: "collab.status.active",
  closed: "collab.status.closed",
  resume: "meta.paused",
  retired: "meta.retired",
  open: "bb.open",
  claimed: "bb.claimed",
  done: "bb.done",
};

const FACT_STATUS_KEY: Record<string, string> = {
  verified: "collab.status.verified",
  candidate: "collab.status.candidate",
  challenged: "bb.challenged",
  revalidated: "bb.revalidated",
  rejected: "bb.rejected",
  merged: "bb.merged",
  superseded: "bb.superseded",
  retired: "meta.retired",
};

const REVIEW_STATUS_KEY: Record<string, string> = {
  blocker: "runtime.findings.sev.blocker",
  warn: "runtime.findings.sev.warn",
  info: "runtime.findings.sev.info",
  critical: "runtime.reports.severity.critical",
  high: "runtime.reports.severity.high",
  medium: "runtime.reports.severity.medium",
  low: "runtime.reports.severity.low",
};

const DIRECTIVE_STATUS_KEY: Record<string, string> = {
  received: "directive.received",
  queued: "directive.queued",
  bound: "directive.bound",
  acted: "directive.acted",
  applied: "directive.acted",
  superseded: "directive.superseded",
  expired: "directive.expired",
  rejected: "directive.rejected",
};

const ROUTE_STATUS_KEY: Record<string, string> = {
  reopened: "runtime.routes.group.reopened",
  suppressed: "runtime.routes.group.suppressed",
};

/** Resolve `t(key)` and keep the raw value when the key is missing. */
export function labelOrRaw(t: StatusTranslate, key: string | undefined, fallback: string): string {
  if (!key) return fallback;
  const label = t(key);
  return label === key ? fallback : label;
}

export function reviewSeverityLabel(severity: string, t: StatusTranslate): string {
  if (severity === "blocker") return t("runtime.findings.sev.blocker");
  if (severity === "warn") return t("runtime.findings.sev.warn");
  if (severity === "info") return t("runtime.findings.sev.info");
  return severity;
}

export function reviewKindLabel(kind: string, t: StatusTranslate): string {
  return labelOrRaw(t, `runtime.findings.kind.${kind}`, kind);
}

export function pocStatusLabel(status: string, t: StatusTranslate): string {
  return labelOrRaw(t, `runtime.pocs.status.${status}`, status);
}

export function directiveStatusLabel(status: string, t: StatusTranslate): string {
  return labelOrRaw(t, DIRECTIVE_STATUS_KEY[status] ?? `directive.${status}`, status);
}

/** i18n key for a knowledge row; omitted for free-text findingClass and directive.action. */
export function knowledgeStatusKey(kind: CollaborationKnowledgeKind, status?: string): string | undefined {
  if (!status) return undefined;
  switch (kind) {
    case "intent":
    case "step":
      return INTENT_STATUS_KEY[status];
    case "fact":
    case "candidate":
      return status === "recorded"
        ? "collab.status.recorded"
        : status === "scoped_negative"
          ? "collab.status.scopedNegative"
          : FACT_STATUS_KEY[status];
    case "observation":
      return "evidence.observationStatus";
    case "goal":
      return status === "satisfied" ? "collab.status.satisfied" : INTENT_STATUS_KEY[status] ?? "collab.status.open";
    case "dead_end":
      return "collab.status.dead_end";
    case "poc":
      return `runtime.pocs.status.${status}`;
    case "report":
      return status === "repro_failed" ? "runtime.reports.reproFailed" : `runtime.reports.${status}`;
    case "review":
      return REVIEW_STATUS_KEY[status] ?? `runtime.findings.kind.${status}`;
    case "finding":
      return undefined;
    case "route":
      return ROUTE_STATUS_KEY[status];
    case "branch":
      return `runtime.routes.status.${status}`;
    case "directive":
      return DIRECTIVE_STATUS_KEY[status];
    case "flag":
      return status === "found" ? "collab.status.found" : undefined;
    case "lock":
      return undefined;
    default: {
      const _never: never = kind;
      return _never;
    }
  }
}

export function knowledgeStatusLabel(
  item: { status?: string; statusKey?: string },
  t: StatusTranslate,
): string {
  return labelOrRaw(t, item.statusKey, item.status || "");
}

export function intentStatusLabel(status: string, t: StatusTranslate): string {
  return labelOrRaw(t, knowledgeStatusKey("intent", status), status);
}

export type ActivityTitleSource = "event" | "action" | "runtime" | "message";

export function activityTitleKey(source: ActivityTitleSource, value: string): string {
  switch (source) {
    case "event":
      return `collab.event.${value}`;
    case "action":
      return `control.action.${value}`;
    case "runtime":
      return `collab.runtime.${value}`;
    case "message":
      return `msg.kind.${value}`;
    default: {
      const _never: never = source;
      return _never;
    }
  }
}

export function activityTitle(
  item: { title: string; titleKey?: string },
  t: StatusTranslate,
): string {
  return labelOrRaw(t, item.titleKey, item.title);
}
