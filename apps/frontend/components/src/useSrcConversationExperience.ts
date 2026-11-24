"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { ConversationExperience } from "@/components/conversation/ConversationShell";
import { createSrcTask, listSrcTasks, srcTasksSnapshot } from "@/lib/srcApi";
import { srcTaskThreadId, type SrcTask } from "@/lib/srcTypes";
import { readCacheIdentity } from "@/lib/readCache";

export interface SrcAuthorizationDraft {
  targetsText?: string;
  excludedTargetsText?: string;
  notes?: string;
}

export function parseScopeEntries(raw: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const part of raw.split(/[\r\n,;，；]+/)) {
    const item = part.trim();
    if (!item || seen.has(item)) continue;
    seen.add(item);
    out.push(item);
  }
  return out;
}

export function useSrcConversationExperience(
  activeTask?: SrcTask,
  authorizedScope: string | SrcAuthorizationDraft = "",
) {
  const scopeKey = typeof authorizedScope === "string"
    ? `${authorizedScope}\0\0`
    : `${authorizedScope.targetsText || ""}\0${authorizedScope.excludedTargetsText || ""}\0${authorizedScope.notes || ""}`;
  const authorizationPayload = useMemo(() => {
    const [rawTargets = "", rawExcluded = "", rawNotes = ""] = scopeKey.split("\0");
    const targets = parseScopeEntries(rawTargets);
    const excludedTargets = parseScopeEntries(rawExcluded);
    const notes = rawNotes.trim();
    if (!targets.length && !excludedTargets.length && !notes) return undefined;
    return {
      ...(targets.length ? { targets } : {}),
      ...(excludedTargets.length ? { excluded_targets: excludedTargets } : {}),
      ...(notes ? { notes } : {}),
    };
  }, [scopeKey]);
  const identity = readCacheIdentity();
  const [tasks, setTasks] = useState(() => ({ identity, data: srcTasksSnapshot() }));
  const [error, setError] = useState("");
  const sequence = useRef(0);
  const reload = useCallback(async (refresh = true) => {
    const current = ++sequence.current;
    try {
      const data = await listSrcTasks(refresh);
      if (current === sequence.current && identity === readCacheIdentity()) { setTasks({ identity, data }); setError(""); }
    } catch (cause) {
      if (current === sequence.current && identity === readCacheIdentity()) setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [identity]);
  useEffect(() => {
    void reload(false);
    return () => { sequence.current += 1; };
  }, [reload]);
  const data = tasks.identity === identity ? tasks.data : srcTasksSnapshot();
  const createdTasks = useRef(new Map<string, string>());
  const experience = useMemo<ConversationExperience>(() => {
    const taskIds = new Map((data || []).filter(task => !task.read_only).flatMap(task => {
      const threadId = srcTaskThreadId(task);
      return threadId ? [[threadId, task.task_id] as const] : [];
    }));
    if (activeTask && srcTaskThreadId(activeTask)) taskIds.set(srcTaskThreadId(activeTask), activeTask.task_id);
    const taskIdFor = (threadId: string) => createdTasks.current.get(threadId) || taskIds.get(threadId);
    return {
      defaultMode: "src",
      draftScope: "src",
      projectNavigationOnly: true,
      allowFork: false,
      threadFilter: thread => thread.mode === "src" && Boolean(taskIdFor(thread.thread_id)),
      threadHref: threadId => {
        const taskId = taskIdFor(threadId);
        return taskId ? `/src/tasks/${encodeURIComponent(taskId)}` : "/src";
      },
      newThreadHref: draftId => `/src?draft=${encodeURIComponent(draftId)}`,
      home: {
        title: "开始一项 SRC 研究",
        subtitle: "直接在对话中描述目标、排除范围或测试约束，Agent 会自动解析并更新研究范围。",
        placeholder: "描述目标域名/URL、排除路径、授权约束或需要验证的线索…",

        projectSelectionOnly: true,
        hideWorkspaceControls: true,
        hideReadinessGuide: true,
        suggestions: [
          { icon: "book", label: "整理研究资料", prompt: "帮我整理这项研究的现有资料，区分已知事实、待验证线索和下一步问题。" },
          { icon: "target", label: "解析范围并梳理计划", prompt: "请根据我在对话中给出的目标、排除路径与约束条件，自动识别并更新任务授权范围，同时梳理研究计划与交付内容。" },
          { icon: "file", label: "检查报告证据", prompt: "帮我检查现有报告的证据是否充分，标出缺少依据或尚未验证的内容。" },
        ],
      },
      emptyThread: {
        title: "开始这项研究",
        hint: "向 Agent 说明这项研究的目标，或从下面的建议开始。",
        suggestions: [
          { icon: "book", label: "梳理当前资产范围", prompt: "根据这项任务的资料，梳理当前涉及的资产范围与已知线索。" },
          { icon: "target", label: "制定研究计划", prompt: "为这项研究制定一份分步计划：目标、要验证的问题与交付内容。" },
          { icon: "file", label: "总结已有发现", prompt: "总结这项任务已经沉淀的发现和证据，指出还缺什么。" },
        ],
      },
      sidebarLabels: {
        title: "研究项目",
        newThread: "新研究",
        search: "搜索研究",
        thread: "研究",
        projectsTitle: "研究项目",
        createProject: "新建研究项目",
        deleteProject: "删除研究项目",
        deleteEmptyProjects: "删除空研究项目",
        newThreadInProject: "在此研究项目中新建研究",
        groupByProject: "按研究项目",
        projectIcon: "layers",
        projectOpenIcon: "layers",
        createProjectIcon: "plus",
      },
      createThread: async (input, options, { promptText }) => {
        const title = promptText.split(/\r?\n/, 1)[0].trim().slice(0, 80) || "SRC 研究";
        const receipt = await createSrcTask({
          title,
          goal: promptText,
          ...(authorizationPayload ? { authorization: authorizationPayload } : {}),
          project_id: input.project_id,
          runtime: input.runtime,
        }, { idempotencyKey: options?.idempotencyKey || options?.commandId, signal: options?.signal });
        const { task_id: taskId, thread_id: threadId } = receipt.output;
        createdTasks.current.set(threadId, taskId);
        return { threadId, href: `/src/tasks/${encodeURIComponent(taskId)}` };
      },
    };
  }, [activeTask, data, authorizationPayload]);
  return { experience, error, reload, historicalTasks: data?.filter(task => task.historical && task.read_only) ?? [] };
}
