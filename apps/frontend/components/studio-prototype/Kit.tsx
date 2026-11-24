"use client";

import type { ReactNode } from "react";
import { cn } from "@/lib/cn";
import { CopyButton, toast } from "@muteki/ui";
import type { StudioRef } from "./model";
import { revealConversation } from "./nav";
import { askAi, citeRef } from "./parts";
import { sendNow } from "./ConversationParts";
import { hasAttachedDomains, readStudioState, refLabel } from "./store";

export function sendAbout(target: StudioRef | null, prompt: string): boolean {
  const state = readStudioState();
  if (hasAttachedDomains()) {
    const accepted = askAi(target, prompt);
    if (accepted) {
      toast({
        title: "已带入对话",
        description: target ? `已附上「${refLabel(state, target)}」并填入指令，发送后交给 Agent。` : "指令已填入真实对话，发送后交给 Agent。",
        tone: "success",
      });
    }
    return accepted;
  }
  if (target) citeRef(target, { silent: true });
  revealConversation();
  return sendNow(state.activeThreadId, prompt);
}

export function Dl({ items, className }: { items: Array<{ label: ReactNode; value: ReactNode; mono?: boolean; copy?: string }>; className?: string }) {
  return (
    <dl className={cn("grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2 text-cx-sm", className)}>
      {items.map((item, index) => (
        <div key={index} className="contents">
          <dt className="text-cx-fg-4">{item.label}</dt>
          <dd className="flex min-w-0 items-center gap-1.5 text-cx-fg-2">
            <span className={cn("min-w-0 truncate", item.mono && "font-cx-mono text-cx-xs")}>{item.value}</span>
            {item.copy ? <CopyButton text={item.copy} label="复制" /> : null}
          </dd>
        </div>
      ))}
    </dl>
  );
}
