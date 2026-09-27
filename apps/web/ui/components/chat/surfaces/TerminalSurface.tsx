"use client";

import dynamic from "next/dynamic";
import { Spinner } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";

const ConversationInteractiveTerminal = dynamic(
  () => import("@/components/conversation/ConversationInteractiveTerminal").then((mod) => mod.ConversationInteractiveTerminal),
  {
    ssr: false,
    loading: () => (
      <div className="flex flex-1 items-center justify-center gap-2 text-[12.5px] text-cx-fg-3">
        <Spinner size={13} />
        正在启动终端…
      </div>
    ),
  },
);

export function TerminalSurface({ threadId, view, onCiteToComposer }: SurfaceProps) {
  return (
    <ConversationInteractiveTerminal
      threadId={threadId}
      rootPath={view.workspace?.root_path || ""}
      onCiteToComposer={onCiteToComposer}
    />
  );
}
