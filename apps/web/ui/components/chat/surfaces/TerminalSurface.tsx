"use client";

import { lazy, Suspense, useEffect, useState } from "react";
import { Spinner } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";

const ConversationInteractiveTerminal = lazy(
  () => import("@/components/conversation/ConversationInteractiveTerminal").then((mod) => ({ default: mod.ConversationInteractiveTerminal })),
);

function TerminalLoading() {
  return (
      <div className="flex flex-1 items-center justify-center gap-2 text-[13px] text-cx-fg-3">
        <Spinner size={13} />
        正在启动终端…
      </div>
  );
}

export function TerminalSurface({ threadId, view, onCiteToComposer }: SurfaceProps) {
  const [clientReady, setClientReady] = useState(false);
  useEffect(() => setClientReady(true), []);
  if (!clientReady) return <TerminalLoading />;
  return (
    <Suspense fallback={<TerminalLoading />}>
    <ConversationInteractiveTerminal
      threadId={threadId}
      rootPath={view.workspace?.root_path || ""}
      onCiteToComposer={onCiteToComposer}
    />
    </Suspense>
  );
}
