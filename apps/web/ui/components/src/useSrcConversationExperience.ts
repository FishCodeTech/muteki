"use client";

import { useMemo, useRef } from "react";
import type { ConversationExperience } from "@/components/conversation/ConversationShell";
import { createSrcTask, listSrcTasks } from "@/lib/srcApi";
import type { SrcTask } from "@/lib/srcTypes";
import { useSrcResource } from "./ui";
import { SRC_DEMO_DISCLAIMER, srcDemoConversation, srcDemoTasks, srcDemoThreads } from "@/lib/srcDemoData";

export function useSrcConversationExperience(activeTask?: SrcTask, demoTaskId?: string) {
  const resource = useSrcResource(() => demoTaskId ? Promise.resolve(srcDemoTasks) : listSrcTasks(), [demoTaskId]);
  const createdTasks = useRef(new Map<string, string>());
  const experience = useMemo<ConversationExperience>(() => {
    const taskIds = new Map((resource.data || []).map(task => [task.binding.thread_id, task.task_id]));
    if (activeTask) taskIds.set(activeTask.binding.thread_id, activeTask.task_id);
    const taskIdFor = (threadId: string) => createdTasks.current.get(threadId) || taskIds.get(threadId);
    return {
      defaultMode: "src",
      draftScope: "src",
      hideProjects: true,
      allowFork: false,
      ...(demoTaskId ? { preview: { threads: srcDemoThreads, view: srcDemoConversation(demoTaskId), notice: SRC_DEMO_DISCLAIMER } } : {}),
      threadFilter: thread => thread.mode === "src" && Boolean(taskIdFor(thread.thread_id)),
      threadHref: threadId => {
        const taskId = taskIdFor(threadId);
        return taskId ? `/src/tasks/${encodeURIComponent(taskId)}${demoTaskId ? "?demo=1" : ""}` : "/src";
      },
      newThreadHref: draftId => `/src?draft=${encodeURIComponent(draftId)}`,
      home: {
        title: "开始一项 SRC 研究",
        subtitle: "描述研究目标，和 Agent 一起整理资料、验证线索与沉淀证据。",
        placeholder: "描述研究目标、已知线索或需要解答的问题…",
        hideWorkspaceControls: true,
        hideReadinessGuide: true,
        suggestions: [
          { icon: "book", label: "整理研究资料", prompt: "帮我整理这项研究的现有资料，区分已知事实、待验证线索和下一步问题。" },
          { icon: "target", label: "梳理研究计划", prompt: "根据我提供的授权范围和目标，梳理研究计划、所需资料与交付内容。" },
          { icon: "file", label: "检查报告证据", prompt: "帮我检查现有报告的证据是否充分，标出缺少依据或尚未验证的内容。" },
        ],
      },
      sidebarLabels: { title: "SRC 研究", newThread: "新研究", search: "搜索研究", thread: "研究" },
      createThread: async (input, options, { promptText }) => {
        const title = promptText.split(/\r?\n/, 1)[0].trim().slice(0, 80) || "SRC 研究";
        const receipt = await createSrcTask({
          title,
          goal: promptText,
          project_id: input.project_id,
          runtime: input.runtime,
        }, { idempotencyKey: options?.idempotencyKey || options?.commandId, signal: options?.signal });
        const { task_id: taskId, thread_id: threadId } = receipt.output;
        createdTasks.current.set(threadId, taskId);
        return { threadId, href: `/src/tasks/${encodeURIComponent(taskId)}` };
      },
    };
  }, [activeTask, demoTaskId, resource.data]);
  return { experience, error: resource.error, reload: resource.reload };
}
