/**
 * Virtual timeline row model (#211).
 *
 * Assistant bodies with turn_id are folded into the matching user row and must
 * not occupy their own virtual indices — otherwise estimateSize (~180px) leaves
 * blank gaps for unrendered / unmeasured slots.
 */

export type TimelineMessageLike = {
  message_id: string;
  turn_id?: string | null;
  role: string;
};

export type TimelineMessageRow = {
  kind: "message";
  key: string;
  messageIndex: number;
  messageId: string;
  turnId: string;
  role: string;
  orphanTurnIds: string[];
  /** Message IDs that live inside this visual row (prompt + folded assistant + orphans). */
  containedMessageIds: string[];
};

export type TimelineOrphanTurnRow = {
  kind: "orphan-turn";
  key: string;
  turnId: string;
  containedMessageIds: string[];
};

export type TimelineVirtualRow = TimelineMessageRow | TimelineOrphanTurnRow;

function mapGetString(map: Map<string, string> | Record<string, string> | undefined, key: string): string {
  if (!map || !key) return "";
  if (map instanceof Map) return map.get(key) || "";
  return map[key] || "";
}

function mapGetNumber(map: Map<string, number> | Record<string, number> | undefined, key: string): number | undefined {
  if (!map || !key) return undefined;
  if (map instanceof Map) return map.get(key);
  return map[key];
}

function leftoversAt(
  leftoverTurnIdsBeforeMessage: Map<number, string[]> | Record<string, string[]> | undefined,
  messageIndex: number,
): string[] {
  if (!leftoverTurnIdsBeforeMessage) return [];
  if (leftoverTurnIdsBeforeMessage instanceof Map) {
    return leftoverTurnIdsBeforeMessage.get(messageIndex) || [];
  }
  return leftoverTurnIdsBeforeMessage[String(messageIndex)] || [];
}

/** Assistant messages with turn_id are merged into the user row — not standalone. */
export function isFoldedAssistantMessage(message: TimelineMessageLike): boolean {
  return message.role === "assistant" && Boolean(message.turn_id);
}

/**
 * Build the real render-row list used by count / getItemKey / measure / deep-link.
 * Includes grouped user+assistant rows, orphan inherited assistants (no turn_id),
 * and continue/retry turns that have no user prompt (trailing leftovers).
 */
export function buildConversationTimelineRows(input: {
  messages: TimelineMessageLike[];
  leftoverTurnIdsBeforeMessage?: Map<number, string[]> | Record<string, string[]>;
  trailingLeftoverTurnIds?: string[];
  assistantMessageIdByTurn?: Map<string, string> | Record<string, string>;
  lastPromptIndexByTurn?: Map<string, number> | Record<string, number>;
}): TimelineVirtualRow[] {
  const messages = input.messages || [];
  const trailing = input.trailingLeftoverTurnIds || [];
  const rows: TimelineVirtualRow[] = [];

  for (let messageIndex = 0; messageIndex < messages.length; messageIndex += 1) {
    const message = messages[messageIndex];
    if (!message?.message_id) continue;
    if (isFoldedAssistantMessage(message)) continue;

    const turnId = String(message.turn_id || "");
    const orphanTurnIds = leftoversAt(input.leftoverTurnIdsBeforeMessage, messageIndex);
    const containedMessageIds: string[] = [message.message_id];

    for (const orphanTurnId of orphanTurnIds) {
      const orphanAssistantId = mapGetString(input.assistantMessageIdByTurn, orphanTurnId);
      if (orphanAssistantId && !containedMessageIds.includes(orphanAssistantId)) {
        containedMessageIds.push(orphanAssistantId);
      }
    }

    if (message.role !== "assistant" && turnId) {
      const lastPromptIndex = mapGetNumber(input.lastPromptIndexByTurn, turnId);
      if (lastPromptIndex === messageIndex) {
        const assistantId = mapGetString(input.assistantMessageIdByTurn, turnId);
        if (assistantId && !containedMessageIds.includes(assistantId)) {
          containedMessageIds.push(assistantId);
        }
      }
    }

    rows.push({
      kind: "message",
      key: `msg:${message.message_id}`,
      messageIndex,
      messageId: message.message_id,
      turnId,
      role: message.role,
      orphanTurnIds,
      containedMessageIds,
    });
  }

  for (const turnId of trailing) {
    if (!turnId) continue;
    const containedMessageIds: string[] = [];
    const assistantId = mapGetString(input.assistantMessageIdByTurn, turnId);
    if (assistantId) containedMessageIds.push(assistantId);
    rows.push({
      kind: "orphan-turn",
      key: `orphan-turn:${turnId}`,
      turnId,
      containedMessageIds,
    });
  }

  return rows;
}

/** Map any message id (including folded assistants) to its virtual row index. */
export function timelineRowIndexForMessageId(
  rows: TimelineVirtualRow[],
  messageId: string,
): number {
  const target = String(messageId || "").trim();
  if (!target) return -1;
  return rows.findIndex((row) => row.containedMessageIds.includes(target));
}
