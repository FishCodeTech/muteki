import type { ChatMessage } from "@/lib/events";

export function toolCommandLabel(message: ChatMessage): string {
  return message.content.replace(/^[▶↳]\s*/, "").trim() || "tool";
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
  opts: { row: number; expanded: number; groupOpen: boolean; expandedMessageId: string | null },
): number {
  if (item.type === "single") {
    return item.message.id === opts.expandedMessageId ? opts.expanded : opts.row;
  }
  if (!opts.groupOpen) return opts.row;
  let total = opts.row;
  for (const message of item.messages) {
    total += message.id === opts.expandedMessageId ? opts.expanded : Math.max(28, Math.round(opts.row * 0.72));
  }
  return total;
}

export function ledgerItemContainsId(item: ActivityLedgerItem, id: string | null): boolean {
  if (!id) return false;
  if (item.type === "single") return item.message.id === id;
  return item.messages.some((message) => message.id === id);
}
