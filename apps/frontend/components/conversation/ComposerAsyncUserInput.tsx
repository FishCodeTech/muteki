"use client";

import React, { useState } from "react";
import { Button } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { ConversationApproval, type UserInputResolvePayload } from "./ConversationApproval";

function isBackgroundRequest(pending: Record<string, unknown>): boolean {
  const native = pending.native;
  return Boolean(native && typeof native === "object" && (native as Record<string, unknown>).background === true);
}

export function ComposerAsyncUserInput({
  pendingInput,
  running,
  threadId,
  busy = false,
  onResolve,
}: {
  pendingInput: Record<string, unknown> | null | undefined;
  running: boolean;
  threadId: string;
  busy?: boolean;
  onResolve: (payload: UserInputResolvePayload) => void | Promise<boolean>;
}) {
  const [expanded, setExpanded] = useState(true);
  const requestId = typeof pendingInput?.request_id === "string" ? pendingInput.request_id : "";
  if (!pendingInput || !requestId || running || !isBackgroundRequest(pendingInput)) return null;
  const title = String(pendingInput.title || pendingInput.message || "需要你的回复");

  return (
    <section
      className="mx-2 mb-2 rounded-2xl border border-cx-border bg-cx-bg-subtle"
      aria-label="来自模型的问题"
      data-testid="composer-async-user-input"
    >
      <header className="flex h-9 items-center gap-2 pl-3 pr-1.5">
        <Icon name="messageCircle" size={13} className="shrink-0 text-cx-accent" />
        <h2 className="shrink-0 text-cx-sm font-medium text-cx-fg-2">来自模型的问题</h2>
        <p className="min-w-0 flex-1 truncate text-cx-xs text-cx-fg-4">{title}</p>
        <Button size="xs" variant="ghost" icon={expanded ? "chevronUp" : "chevronDown"} onClick={() => setExpanded((value) => !value)}>
          {expanded ? "隐藏" : "回答"}
        </Button>
      </header>
      {expanded ? (
        <div className="px-3 pb-3">
          <p className="mb-2 text-cx-xs text-cx-fg-4">模型会继续工作。你可以随时回答，也可以继续发送消息。</p>
          <ConversationApproval
            pendingApproval={null}
            pendingInput={pendingInput}
            threadId={threadId}
            busy={busy}
            onApprovalDecision={() => {}}
            onUserInputResolve={onResolve}
          />
        </div>
      ) : null}
    </section>
  );
}
