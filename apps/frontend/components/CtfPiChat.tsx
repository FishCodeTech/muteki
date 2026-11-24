"use client";

import { useState } from "react";
import { ConversationStreamTimeline } from "@/components/conversation/ConversationStreamBindings";
import { ConversationDetailsDrawer, type DrawerDetailPayload } from "@/components/conversation/ConversationDetailsDrawer";
import { useConversation, sendConversationCommand } from "@/lib/useConversation";
import { classifyCommandReceipt } from "@/lib/commandReceiptState";

export function CtfPiChat({ threadId }: { threadId: string }) {
  const conversation = useConversation(threadId);
  const [detail, setDetail] = useState<DrawerDetailPayload | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const respond = async (type: string, payload: Record<string, unknown>) => {
    setBusy(true); setError("");
    try {
      const receipt = await sendConversationCommand(threadId, type, payload);
      const completed = classifyCommandReceipt(receipt) === "completed";
      if (!completed) setError("操作已受理，等待确认");
      return completed;
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); return false; }
    finally { setBusy(false); }
  };
  if (!conversation.view) return <p role="status">{conversation.error || "正在读取 Pi 会话…"}</p>;
  return <div className="flex h-[60vh] min-h-[280px] flex-col" aria-label="Pi 会话记录">
    <ConversationStreamTimeline stream={conversation.stream} streamThreadId={threadId} threadId={threadId}
      view={conversation.view} running={Boolean(conversation.view.state.running_turn_id)}
      connected={conversation.connected} streamStatus={conversation.streamStatus} busy={busy}
      contentError={conversation.error} contentLoading={conversation.loading}
      onReloadContent={() => void conversation.refresh()} onLoadOlder={conversation.loadOlderMessages}
      onJumpToLatest={conversation.jumpToLatestMessages} loadingOlder={conversation.loadingOlder}
      onOpenDrawer={setDetail} commandError={error} onDismissCommandError={() => setError("")}
      onApprovalDecision={(approvalId, decision, scope, optionId, note) => {
        void respond("conversation.approval.resolve", { approval_id: approvalId, decision, scope: scope || "once", option_id: optionId, note });
      }}
      onUserInputResolve={payload => respond("conversation.user_input.resolve", payload)} />
    <ConversationDetailsDrawer payload={detail} onClose={() => setDetail(null)} threadId={threadId} />
  </div>;
}
