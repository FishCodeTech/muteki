import type { ChatMessage } from "@/lib/events";

export function toolCommandLabel(message: ChatMessage): string {
  return message.content.replace(/^[▶↳]\s*/, "").trim() || "tool";
}

/** `Bash: {"command":"ls"}` -> `{ name: "Bash", args: '{"command":"ls"}' }`.
 *  A leading `<word>:` is the tool name; anything else is all args. The `//`
 *  guard keeps a bare `https://…` label from splitting on its scheme. */
export function splitToolCommand(command: string): { name: string; args: string } {
  const match = /^([\w.-]{1,32}):\s*(.*)$/s.exec(command);
  if (!match || !match[2] || match[2].startsWith("//")) return { name: "", args: command };
  return { name: match[1], args: match[2] };
}

/** Collapsed rows show tool commands on one or two lines, so the run's own
 *  workspace prefix (`…/sessions/<run>/workspace/workers/<seat>`) would eat
 *  the whole row before the interesting part. Only the summary is rewritten;
 *  the expanded call and its tooltip keep the verbatim command. */
export function abbreviateWorkspacePaths(text: string): string {
  return text
    .replace(/\S*?\/sessions\/[^/\s]+\/workspace\/workers\/[^/\s]+/g, "$WORKER")
    .replace(/\S*?\/sessions\/[^/\s]+\/workspace/g, "$WORKSPACE");
}

/** Fixed row heights per density. The virtualizer positions rows by these
 *  numbers and the CSS custom properties mirror them, so the two must never
 *  diverge: `row` is a collapsed item, `child` one call inside an open tool
 *  group, `expanded` a row or call opened to show its full text/output. */
export type LedgerMetrics = { row: number; child: number; expanded: number };
export const LEDGER_METRICS: Record<"comfortable" | "compact", LedgerMetrics> = {
  comfortable: { row: 60, child: 34, expanded: 208 },
  compact: { row: 32, child: 28, expanded: 148 },
};

/** Tallest an expanded row/call may grow before its text scrolls internally:
 *  a share of the visible ledger so one event never fills the whole panel. */
export function expandedHeightCeiling(metrics: LedgerMetrics, viewportHeight: number): number {
  return Math.max(metrics.expanded, Math.min(480, Math.floor(viewportHeight * 0.72)));
}

/** Height of the laid-out inline content of `el`, unaffected by the element's
 *  own max-height/overflow clipping (unlike scrollHeight, which is floored at
 *  the client height and so could never tell us a row got too tall). */
function inlineContentHeight(el: Element): number {
  const range = document.createRange();
  range.selectNodeContents(el);
  return range.getBoundingClientRect().height;
}

/** Longest the verbatim command may be inside an expanded call before it
 *  scrolls, so long heredocs don't push the output out of view. */
export const EXPANDED_COMMAND_MAX = 96;

/** Line boxes report fractional heights while the scroller works in whole
 *  pixels; without this slack a fully-visible text still shows a 1–3px
 *  scrollbar nub. */
const MEASURE_SLACK = 4;

/** Row height an expanded ledger element needs to show its content without
 *  internal scrolling. `el` is either a plain row (`.act-msg`, measured via its
 *  `.act-body`) or a tool call (`.act-tool-child`, command + output). Returns
 *  null when the expected parts are not rendered (nothing to measure yet). */
export function measureExpandedHeight(el: HTMLElement): number | null {
  const top = el.getBoundingClientRect().top;
  if (el.classList.contains("act-tool-child")) {
    const cmd = el.querySelector(".act-tool-cmd");
    if (!cmd) return null;
    const args = cmd.querySelector(".act-tool-args") ?? cmd;
    const out = el.querySelector(".act-tool-out");
    const style = getComputedStyle(el);
    const gap = parseFloat(style.rowGap) || 0;
    let needed = cmd.getBoundingClientRect().top - top + Math.min(EXPANDED_COMMAND_MAX, args.getBoundingClientRect().height);
    if (out) needed += gap + inlineContentHeight(out);
    return needed + (parseFloat(style.paddingBottom) || 0) + MEASURE_SLACK;
  }
  const main = el.querySelector(".act-main");
  const body = el.querySelector(".act-body");
  if (!main || !body) return null;
  const padBottom = parseFloat(getComputedStyle(main).paddingBottom) || 0;
  const border = parseFloat(getComputedStyle(el).borderBottomWidth) || 0;
  return body.getBoundingClientRect().top - top + inlineContentHeight(body) + padBottom + border + MEASURE_SLACK;
}

export type ActivityLedgerItem =
  | { type: "single"; id: string; message: ChatMessage }
  | { type: "tools"; id: string; solverId: string; messages: ChatMessage[]; ts: number };

/** Consecutive tool rows from the same worker become one collapsed group.
 *  Other kinds stay one row. Order follows the chat array (global time). */
export function projectActivityLedger(chat: ChatMessage[]): ActivityLedgerItem[] {
  const items: ActivityLedgerItem[] = [];
  for (const message of chat) {
    if (message.kind !== "tool") {
      items.push({ type: "single", id: message.id, message });
      continue;
    }
    const solverId = message.solverId || "";
    const last = items[items.length - 1];
    if (last?.type === "tools" && last.solverId === solverId) {
      last.messages.push(message);
      last.ts = message.ts;
      continue;
    }
    items.push({
      type: "tools",
      id: `tools:${message.id}`,
      solverId,
      messages: [message],
      ts: message.ts,
    });
  }
  return items;
}

export function toolGroupLatestCommand(messages: ChatMessage[]): string {
  const last = messages[messages.length - 1];
  return last ? toolCommandLabel(last) : "";
}

export function toolGroupFailedCommand(messages: ChatMessage[]): string | undefined {
  for (let i = messages.length - 1; i >= 0; i--) {
    if (messages[i].toolFailed) return toolCommandLabel(messages[i]);
  }
  return undefined;
}

export function ledgerItemHeight(
  item: ActivityLedgerItem,
  opts: { metrics: LedgerMetrics; groupOpen: boolean; expandedMessageId: string | null },
): number {
  const { row, child, expanded } = opts.metrics;
  if (item.type === "single") {
    return item.message.id === opts.expandedMessageId ? expanded : row;
  }
  if (!opts.groupOpen) return row;
  let total = row;
  for (const message of item.messages) {
    total += message.id === opts.expandedMessageId ? expanded : child;
  }
  return total;
}

export function ledgerItemContainsId(item: ActivityLedgerItem, id: string | null): boolean {
  if (!id) return false;
  if (item.type === "single") return item.message.id === id;
  return item.messages.some((message) => message.id === id);
}
