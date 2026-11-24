import { conversationStorageKey } from "./conversationStorageScope";

export type EditResendBuffer = {
  threadId: string;
  turnId: string;
  text: string;
  updatedAt: number;
};

const memory = new Map<string, EditResendBuffer>();

function keyOf(threadId: string, turnId: string): string {
  return conversationStorageKey(`${threadId}::${turnId}`);
}

export function readEditBuffer(threadId: string, turnId: string): EditResendBuffer | null {
  return memory.get(keyOf(threadId, turnId)) || null;
}

export function writeEditBuffer(threadId: string, turnId: string, text: string): EditResendBuffer {
  const row: EditResendBuffer = {
    threadId,
    turnId,
    text,
    updatedAt: Date.now(),
  };
  memory.set(keyOf(threadId, turnId), row);
  return row;
}

export function clearEditBuffer(threadId: string, turnId: string): void {
  memory.delete(keyOf(threadId, turnId));
}
