// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
import type { ReactNode } from "react";

export type AgentActivityStatus = "working" | "complete";
export type AgentStepStatus = "pending" | "active" | "complete";

export interface AgentActivityStep {
  id: string;
  type: "step";
  label: ReactNode;
  status?: AgentStepStatus;
  meta?: ReactNode;
}

export interface AgentActivityText {
  id: string;
  type: "text";
  content: ReactNode;
}

export interface AgentSearchResult {
  id: string;
  title: ReactNode;
  domain?: ReactNode;
  url?: string;
  icon?: ReactNode;
}

export interface AgentActivitySearch {
  id: string;
  type: "search";
  query: ReactNode;
  results?: AgentSearchResult[];
  moreCount?: number;
}

export interface AgentActivityTool {
  id: string;
  type: "tool";
  action: "read" | "edit" | "run" | (string & {});
  target: ReactNode;
  additions?: number;
  deletions?: number;
}

export type AgentTraceKind =
  | "thinking"
  | "message"
  | "write"
  | "run"
  | "read"
  | (string & {});

export interface AgentActivityTrace {
  id: string;
  type: "trace";
  kind: AgentTraceKind;
  label: ReactNode;
  detail?: ReactNode;
  icon?: ReactNode;
}

/** Host-rendered row for entries that need their own interaction (tool output, streamed text). */
export interface AgentActivityCustom {
  id: string;
  type: "custom";
  content: ReactNode;
}

export type AgentActivityItem =
  | AgentActivityStep
  | AgentActivityText
  | AgentActivitySearch
  | AgentActivityTool
  | AgentActivityTrace
  | AgentActivityCustom;

export type AgentActivityContentType = AgentActivityItem["type"] | "mixed";

export interface AgentActivityProps {
  /** Chronological activity entries. Append or update items as events stream. */
  items: AgentActivityItem[];
  /** Expected activity kind before the first streamed item arrives. */
  contentType?: AgentActivityContentType;
  /** Current run phase. Active runs start expanded and can be collapsed by the reader. */
  status?: AgentActivityStatus;
  /** Elapsed run time, in seconds. Used by the step-only summary. */
  duration?: number;
  /** Controlled expanded state used after the run completes. */
  open?: boolean;
  /** Initial expanded state used after the run completes. */
  defaultOpen?: boolean;
  /** Called when the completed activity disclosure changes state. */
  onOpenChange?: (open: boolean) => void;
  /** Collapse the disclosure when status changes from working to complete. */
  collapseOnComplete?: boolean;
  /** Optional label shown while the run is active. */
  activeLabel?: ReactNode;
  /** Optional completed summary. Derived from the item types by default. */
  summary?: ReactNode;
  /** Optional renderer for the contents of the active status row. */
  renderWorkingStatus?: (context: {
    label: ReactNode;
    duration: number;
  }) => ReactNode;
  /** Optional renderer for the contents before the built-in disclosure chevron. */
  renderCompletedStatus?: (context: {
    summary: ReactNode;
    duration: number;
  }) => ReactNode;
  /** Maximum live viewport height. Follows new entries until the reader interacts. */
  maxHeight?: number;
  /** Height cap once the run completes and the reader expands it. Defaults to `maxHeight`. */
  completedMaxHeight?: number;
  /** Connect rows that carry a `data-timeline-node` glyph with a vertical rail. */
  timeline?: boolean;
  className?: string;
  contentClassName?: string;
}
