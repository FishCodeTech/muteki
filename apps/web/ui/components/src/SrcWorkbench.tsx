"use client";

import { useEffect, useMemo } from "react";
import { ConversationShell, type ConversationExperience, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { fetchSrcTask } from "@/lib/srcApi";
import type { SrcTask } from "@/lib/srcTypes";
import type { ComposerCapabilityRef } from "@/lib/composerCapabilities";
import { STUDIO_BASE, studioChatBase, visibleWorkspace } from "@/components/studio-prototype/nav";
import { studioComposer } from "@/components/studio-prototype/parts";
import {
  useStudioWorkbenchExperience,
  useStudioWorkspaceUrlSync,
} from "@/components/studio-prototype/StudioPrototype";
import { PendingSuggestionTray, StudioContextTray } from "@/components/studio-prototype/ConversationParts";
import { effectiveContext, readStudioState, studio, useStudioState } from "@/components/studio-prototype/store";
import { StudioSaveIndicator, TaskPersistenceGate } from "@/components/studio-prototype/SaveStatus";
import { createAssetDomain } from "@/components/studio-prototype/real/assets";
import { createFindingDomain } from "@/components/studio-prototype/real/findings";
import { createTasksDomain } from "@/components/studio-prototype/real/tasks";
import { createReportsDomain } from "@/components/studio-prototype/real/reports";
import { createSuggestionsDomain } from "@/components/studio-prototype/real/suggestions";
import { SrcError, SrcLoading, useSrcResource } from "./ui";
import { useSrcConversationExperience } from "./useSrcConversationExperience";

/**
 * Real-task host for the SRC workbench: the real conversation thread plus the
 * studio panel extensions, mentions and persistence bound to the backend.
 */
export function SrcWorkbench({ taskId, navigation }: { taskId: string; navigation: ConversationNavigation }) {
  const task = useSrcResource(() => fetchSrcTask(taskId), [taskId]);
  if (task.loading && !task.data) return <main className="src-task-state"><SrcLoading label="正在读取 SRC 任务与工作台…" /></main>;
  if (!task.data) return <main className="src-task-state"><SrcError error={task.error || "SRC 任务不可读"} onRetry={() => void task.reload()} /></main>;
  return (
    <TaskPersistenceGate
      taskId={taskId}
      threadId={task.data.binding.thread_id}
      title={task.data.title}
      goal={task.data.input?.goal ?? ""}
    >
      <SrcWorkbenchBody task={task.data} navigation={navigation} />
    </TaskPersistenceGate>
  );
}

function SrcWorkbenchBody({ task, navigation }: { task: SrcTask; navigation: ConversationNavigation }) {
  const threadId = task.binding.thread_id;
  const state = useStudioState();
  const srcConversation = useSrcConversationExperience(task);
  const workbench = useStudioWorkbenchExperience(threadId);

  useStudioWorkspaceUrlSync(threadId, true, navigation.router, navigation.searchParams.toString());

  // Conversation deep links generated inside the workbench stay on this route.
  useEffect(() => {
    const previous = studioChatBase.current;
    studioChatBase.current = `/src/tasks/${encodeURIComponent(task.task_id)}/workbench`;
    return () => { studioChatBase.current = previous || STUDIO_BASE; };
  }, [task.task_id]);

  useEffect(() => {
    studio.setActiveThread(threadId);
  }, [threadId]);

  // Real domains own their fields on this host; unregistered mutators stay local.
  useEffect(() => {
    (studio as unknown as { attachDomains: (input: { taskId: string; threadId: string; actor: string; domains: unknown[] }) => void }).attachDomains({
      taskId: task.task_id, threadId, actor: "local-user",
      domains: [createAssetDomain(), createFindingDomain(), createTasksDomain(), createReportsDomain(), createSuggestionsDomain()],
    });
    return () => (studio as unknown as { detachDomains: () => void }).detachDomains();
  }, [task.task_id, threadId]);

  const experience = useMemo<ConversationExperience>(() => ({
    ...srcConversation.experience,
    ...workbench,
    composerAccessory: (
      <span className="flex w-full flex-col gap-1">
        <span className="flex w-full items-start justify-between gap-2">
          <StudioContextTray threadId={threadId} />
          <StudioSaveIndicator />
        </span>
        <PendingSuggestionTray />
      </span>
    ),
    // Keep the real composer controls (model/effort picker); the studio demo
    // controls are preview-only.
    composerControls: undefined,
    composerHandle: studioComposer,
    // Workbench context pills ride the send as server-resolved src_object refs.
    composerRefs: (): ComposerCapabilityRef[] => {
      const state = readStudioState();
      const labelOf = (ref: { kind: string; id: string }) =>
        state.sources.find((o) => o.id === ref.id)?.title ??
        state.clues.find((o) => o.id === ref.id)?.title ??
        state.tasks.find((o) => o.id === ref.id)?.title ??
        state.reports.find((o) => o.id === ref.id)?.title ??
        `${ref.kind}:${ref.id}`;
      return effectiveContext(state, threadId, visibleWorkspace(threadId)).map((ref) => ({
        context_schema: 2 as const,
        kind: "src_object" as never,
        node_id: `ctx-src-${ref.kind}-${ref.id}`,
        id: `src_object:${ref.kind}:${ref.id}`,
        name: labelOf(ref),
        description: `SRC 工作台 ${ref.kind}`,
        source: "src-task",
        scope: "task",
        locator: { kind: ref.kind, id: ref.id } as never,
        snapshot: { label: labelOf(ref), text: "", captured_at: "" },
      }));
    },
    onSent: () => studio.clearSentContext(threadId),
    preview: undefined,
  }), [srcConversation.experience, threadId, workbench]);

  void state;

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden" data-testid="src-workbench">
      <div className="relative flex min-w-0 flex-1">
        <ConversationShell threadId={threadId} navigation={navigation} experience={experience} />
      </div>
    </div>
  );
}
