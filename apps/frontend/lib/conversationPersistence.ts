import { flushComposerAttachmentCache } from "./composerDraftStore";
import { flushComposerRecallStore } from "./composerRecallStore";
import { flushUserInputDraftStore } from "./userInputDraftStore";
import { flushQueueEditDraftStore } from "./queueEditDraftStore";
import { flushSendIntentStore } from "./sendIntentStore";
import { flushTaskReceiptStore } from "./task-center";
import { flushConversationControlStore } from "./conversationControlState";

const participants = [
  flushComposerAttachmentCache,
  flushComposerRecallStore,
  flushUserInputDraftStore,
  flushQueueEditDraftStore,
  flushSendIntentStore,
  flushTaskReceiptStore,
  flushConversationControlStore,
] as const;

export async function flushConversationPersistence(): Promise<{ persisted: boolean; error?: string }> {
  const errors: string[] = [];
  for (const flush of participants) {
    try {
      const result = await flush();
      if (!result.persisted) errors.push(result.error || "本地恢复记录保存失败");
    } catch (error) { errors.push(error instanceof Error ? error.message : String(error)); }
  }
  return errors.length ? { persisted: false, error: errors.join("\n") } : { persisted: true };
}
