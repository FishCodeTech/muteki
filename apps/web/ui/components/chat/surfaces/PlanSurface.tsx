"use client";

import { ScrollArea } from "@/components/chat/ui";
import type { SurfaceProps } from "@/components/chat/panel/types";
import { ConversationPlanPanel } from "@/components/conversation/ConversationPlanPanel";

export function PlanSurface({ view, tools, onOpenDetails }: SurfaceProps) {
  return (
    <ScrollArea className="flex flex-1 flex-col">
      <ConversationPlanPanel view={view} tools={tools} onOpenDetails={onOpenDetails} />
    </ScrollArea>
  );
}
