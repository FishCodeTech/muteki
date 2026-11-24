"use client";

import { ConversationDiffSurface } from "@/components/conversation/ConversationDiffSurface";
import { chatPanel, useDiffRequest } from "@/lib/chatPanelStore";
import type { SurfaceProps } from "@/components/chat/panel/types";

export function DiffSurface({ threadId, view, active, onDiffAnnotationSend }: SurfaceProps) {
  const request = useDiffRequest(threadId);
  return (
    <ConversationDiffSurface
      threadId={threadId}
      view={view}
      active={active}
      requestedBaseline={request ?? null}
      onBaselineConsumed={() => { if (request) chatPanel.consumeDiffRequest(threadId, request.nonce); }}
      onSendAnnotations={onDiffAnnotationSend}
    />
  );
}
