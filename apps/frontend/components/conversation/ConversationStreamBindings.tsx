"use client";


import { useMemo, type ComponentProps } from "react";
import { RightPanel, type RightPanelProps } from "@/components/chat/panel/RightPanel";
import {
  useConversationStream,
  useConversationStreamSelector,
  type ConversationEvent,
  type ConversationStreamStore,
  type ConversationView,
} from "@/lib/useConversation";
import { ConversationTimeline, type ConversationTimelineProps } from "./ConversationTimeline";
import { ConversationBottomPanel } from "./ConversationWorkspaceDock";
import { ConversationQuotaResume } from "./ConversationQuotaResume";
import { collectConversationTools, type ConversationToolRecord } from "./conversationEventViews";

type StreamBinding = {
  stream: ConversationStreamStore;
  streamThreadId: string | null | undefined;
};

const NO_TOOLS: ConversationToolRecord[] = [];
const toolsByEvents = new WeakMap<ConversationEvent[], WeakMap<ConversationView, ConversationToolRecord[]>>();

export function conversationPanelTools(
  events: ConversationEvent[],
  view: ConversationView | null | undefined,
): ConversationToolRecord[] {
  if (!view) return NO_TOOLS;
  let toolsByView = toolsByEvents.get(events);
  if (!toolsByView) {
    toolsByView = new WeakMap();
    toolsByEvents.set(events, toolsByView);
  }
  const cached = toolsByView.get(view);
  if (cached) return cached;
  const statuses = Object.fromEntries((view.turns || []).map((turn) => [turn.turn_id, turn.status]));
  const tools = collectConversationTools(events, statuses);
  toolsByView.set(view, tools);
  return tools;
}

export function useConversationRunningToolCount(
  stream: ConversationStreamStore,
  view: ConversationView | null | undefined,
): number {
  return useConversationStreamSelector(
    stream,
    view?.thread.thread_id,
    (snapshot) => conversationPanelTools(snapshot.events, view).filter((tool) => tool.status === "running").length,
  );
}

export function ConversationStreamTimeline({
  stream,
  streamThreadId,
  ...props
}: StreamBinding & Omit<ConversationTimelineProps, "events" | "liveText" | "liveTextRuns" | "resumedTurnId">) {
  const snapshot = useConversationStream(stream, streamThreadId);
  return (
    <ConversationTimeline
      {...props}
      events={snapshot.events}
      liveText={snapshot.liveText}
      liveTextRuns={snapshot.liveTextRuns}
      resumedTurnId={snapshot.resumedTurnId}
    />
  );
}

export function ConversationStreamRightPanel({
  stream,
  ...props
}: Omit<StreamBinding, "streamThreadId"> & Omit<RightPanelProps, "events" | "tools">) {
  const { events } = useConversationStream(stream, props.view.thread.thread_id);
  const tools = useMemo(() => conversationPanelTools(events, props.view), [events, props.view]);
  return <RightPanel {...props} events={events} tools={tools} />;
}

export function ConversationStreamBottomPanel({
  stream,
  streamThreadId,
  ...props
}: StreamBinding & Omit<ComponentProps<typeof ConversationBottomPanel>, "events">) {
  const { events } = useConversationStream(stream, streamThreadId);
  return <ConversationBottomPanel {...props} events={events} />;
}

export function ConversationStreamQuotaResume({
  stream,
  ...props
}: Omit<StreamBinding, "streamThreadId"> & Omit<ComponentProps<typeof ConversationQuotaResume>, "events">) {
  const { events } = useConversationStream(stream, props.view.thread.thread_id);
  return <ConversationQuotaResume {...props} events={events} />;
}
