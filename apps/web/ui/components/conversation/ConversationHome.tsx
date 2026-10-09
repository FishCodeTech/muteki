"use client";

import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";

import React, { useEffect, useState } from "react";
import { motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "../Icon";
import { Tooltip, useReducedMotion } from "@/components/chat/ui";
import { PromptBar, type PromptBarAttachment } from "../ai-native/prompt-bar";
import { ConversationModelPicker } from "./ConversationModelPicker";
import { ConversationComposerModes } from "./ConversationComposerModes";
import { ComposerContextStrip } from "./ComposerContextStrip";
import type {
  ConversationCredential,
  ConversationProject,
  RuntimeInstance,
  WorkspaceBindMode,
} from "@/lib/useConversation";
import type {
  ComposerCapabilityContext,
  ComposerCapabilityRef,
} from "@/lib/composerCapabilities";
import type { ConversationReadiness } from "@/lib/conversationReadiness";
import { ConversationReadinessBanner } from "./ConversationReadinessBanner";
import { documentFromPromptAndRefs, type PromptDocument } from "@/lib/composerContextDoc";

export interface ConversationHomeProps {
  prompt: string;
  onPromptChange: (val: string) => void;
  promptDocument?: PromptDocument;
  onPromptDocumentChange?: (doc: PromptDocument) => void;
  onSubmit: () => void;
  busy?: boolean;
  credentials: ConversationCredential[];
  selectedCredentialId: string;
  selectedModel: string;
  selectedEffort: string;
  selectedServiceTier: string;
  selectedAccessMode: string;
  accessModes?: string[];
  accessModeReason?: string;
  interactionMode?: "default" | "plan";
  planModeAvailable?: boolean;
  planModeReason?: string;
  onInteractionModeChange?: (mode: "default" | "plan") => void;
  runtimes?: RuntimeInstance[];
  runtimeKey?: string;
  onRuntimeChange?: (key: string) => void;
  onSelectModelParams: (params: {
    credentialId: string;
    model: string;
    effort?: string;
    serviceTier?: string;
    accessMode?: string;
  }) => void;
  projects: ConversationProject[];
  selectedProjectId: string;
  onProjectChange: (id: string) => void;
  onCreateProject?: () => Promise<string | null>;
  onCreateFromPath?: (path: string) => Promise<string | null>;
  creatingProject?: boolean;
  credentialsLoading?: boolean;
  credentialsError?: string;
  onRetryCredentials?: () => void;
  projectsLoading?: boolean;
  projectsError?: string;
  onRetryProjects?: () => void;
  preferPathInput?: boolean;
  requestPathInput?: boolean;
  recentPaths?: string[];
  readiness?: ConversationReadiness;
  probing?: boolean;
  onRetryReadiness?: () => void;
  onProbeAgent?: () => void;
  onDismissGuide?: () => void;
  onRequestPathInput?: () => void;
  workspaceMode?: WorkspaceBindMode;
  onWorkspaceModeChange?: (mode: WorkspaceBindMode) => void;
  worktreeBranch?: string;
  onWorktreeBranchChange?: (branch: string) => void;
  existingWorktreePath?: string;
  onExistingWorktreePathChange?: (path: string) => void;
  attachments?: PromptBarAttachment[];
  onAddAttachment?: () => void;
  onAddFiles?: (files: File[]) => void;
  onRemoveAttachment?: (index: number) => void;
  onRetryAttachment?: (index: number) => void;
  onLargePaste?: (text: string, selection?: LargePasteSelection) => void;
  capabilityContext?: ComposerCapabilityContext;
  capabilityRefs?: ComposerCapabilityRef[];
  onCapabilityRefsChange?: (refs: ComposerCapabilityRef[]) => void;
  onComposerCommand?: (action: string, sourceDocument?: PromptDocument) => void;
  projectHasDefault?: boolean;
  savingProjectDefault?: boolean;
  onSetProjectDefault?: () => void;
  onClearProjectDefault?: () => void;
  onHistoryNavigate?: (direction: "older" | "newer") => boolean;
  historyBrowsing?: boolean;
  historyStatus?: string;
  onStashShortcut?: () => void;
  onOpenStashPanel?: () => void;
  stashCount?: number;
  className?: string;
  modelPickerOpen?: boolean;
  onModelPickerOpenChange?: (open: boolean) => void;
}

const SUGGESTIONS: Array<{ icon: IconName; label: string; prompt: string; workspace?: boolean }> = [
  { icon: "gitCompare", label: "审查当前改动", prompt: "审查当前工作区的未提交改动，指出潜在问题并给出修改建议。", workspace: true },
  { icon: "book", label: "解释这个项目", prompt: "阅读这个项目，概述它的架构、关键模块和运行方式。", workspace: true },
  { icon: "wrench", label: "修复失败的测试", prompt: "运行测试，定位失败原因并修复，完成后再次验证。", workspace: true },
  { icon: "listTodo", label: "起草实现方案", prompt: "为下面的需求起草一个分步骤的实现方案，并说明取舍：\n\n" },
];

function greeting(hour: number): string {
  if (hour < 5) return "夜深了";
  if (hour < 11) return "早上好";
  if (hour < 13) return "中午好";
  if (hour < 18) return "下午好";
  return "晚上好";
}

export function ConversationHome({
  prompt,
  onPromptChange,
  promptDocument,
  onPromptDocumentChange,
  onSubmit,
  busy = false,
  credentials,
  selectedCredentialId,
  selectedModel,
  selectedEffort,
  selectedServiceTier,
  selectedAccessMode,
  accessModes = [],
  accessModeReason = "",
  interactionMode = "default",
  planModeAvailable = false,
  planModeReason = "",
  onInteractionModeChange,
  runtimes,
  runtimeKey,
  onRuntimeChange,
  onSelectModelParams,
  projects,
  selectedProjectId,
  onProjectChange,
  onCreateProject,
  onCreateFromPath,
  creatingProject = false,
  credentialsLoading = false,
  credentialsError = "",
  onRetryCredentials,
  projectsLoading = false,
  projectsError = "",
  onRetryProjects,
  preferPathInput = false,
  requestPathInput = false,
  recentPaths = [],
  readiness,
  probing = false,
  onRetryReadiness,
  onProbeAgent,
  onDismissGuide,
  onRequestPathInput,
  workspaceMode,
  onWorkspaceModeChange,
  worktreeBranch,
  onWorktreeBranchChange,
  existingWorktreePath,
  onExistingWorktreePathChange,
  attachments = [],
  onAddAttachment,
  onAddFiles,
  onRemoveAttachment,
  onRetryAttachment,
  onLargePaste,
  capabilityContext,
  capabilityRefs = [],
  onCapabilityRefsChange,
  onComposerCommand,
  projectHasDefault = false,
  savingProjectDefault = false,
  onSetProjectDefault,
  onClearProjectDefault,
  onHistoryNavigate,
  historyBrowsing,
  historyStatus,
  onStashShortcut,
  onOpenStashPanel,
  stashCount,
  className = "",
  modelPickerOpen,
  onModelPickerOpenChange,
}: ConversationHomeProps) {
  const reduced = useReducedMotion();
  const [hello, setHello] = useState("你好");
  useEffect(() => setHello(greeting(new Date().getHours())), []);
  const reveal = (delay: number) => (reduced
    ? { initial: { opacity: 0 }, animate: { opacity: 1 } }
    : {
      initial: { opacity: 0, y: 10, filter: "blur(4px)" },
      animate: { opacity: 1, y: 0, filter: "blur(0px)", transition: { duration: 0.42, delay, ease: [0.16, 1, 0.3, 1] as const } },
    });
  const applySuggestion = (text: string) => {
    if (onPromptDocumentChange) onPromptDocumentChange(documentFromPromptAndRefs(text, []));
    else onPromptChange(text);
    window.requestAnimationFrame(() => {
      const composer = document.querySelector<HTMLElement>("[data-c34-composer]");
      composer?.focus();
    });
  };
  const composerEmpty = !prompt.trim() && !attachments.length;

  return (
    <div className={cn("cx-scroll flex min-h-0 flex-1 flex-col items-center overflow-y-auto px-4", className)}>
      <div className="flex w-full max-w-[var(--dsh-composer-card-max-width,752px)] flex-1 flex-col items-center justify-center gap-8 pb-[12vh] pt-10">
        <motion.div className="flex flex-col items-center gap-2 text-center" {...reveal(0)}>
          <h1 className="text-[28px] font-semibold leading-9 text-cx-fg">{hello}，想让 Agent 做点什么？</h1>
          <p className="text-[14px] text-cx-fg-3">{selectedProjectId ? "描述任务，Agent 可使用已选择的工作区执行并汇报。" : "描述任务即可开始普通聊天。读取文件、审查代码和终端操作需要先选择工作目录。"}</p>
        </motion.div>

        {readiness && onRetryReadiness && onProbeAgent && onDismissGuide ? (
          <ConversationReadinessBanner
            readiness={readiness}
            probing={probing}
            onRetry={onRetryReadiness}
            onProbe={onProbeAgent}
            onDismissGuide={onDismissGuide}
            directoryControlInComposer
          />
        ) : null}

        <motion.div className="w-full" {...reveal(0.06)}>
          <PromptBar
            value={prompt}
            onChange={onPromptChange}
            promptDocument={promptDocument}
            onPromptDocumentChange={onPromptDocumentChange}
            onSubmit={onSubmit}
            busy={busy}
            placeholder="描述你想完成的任务"
            attachments={attachments}
            onAddAttachment={onAddAttachment}
            onAddFiles={onAddFiles}
            onRemoveAttachment={onRemoveAttachment}
            onRetryAttachment={onRetryAttachment}
            onLargePaste={onLargePaste}
            capabilityContext={capabilityContext}
            capabilityRefs={capabilityRefs}
            onCapabilityRefsChange={onCapabilityRefsChange}
            onComposerCommand={onComposerCommand}
            onHistoryNavigate={onHistoryNavigate}
            historyBrowsing={historyBrowsing}
            historyStatus={historyStatus}
            onStashShortcut={onStashShortcut}
            onOpenStashPanel={onOpenStashPanel}
            stashCount={stashCount}
            contextMeter={
              <ConversationComposerModes
                selectedCredentialId={selectedCredentialId}
                selectedModel={selectedModel}
                selectedAccessMode={selectedAccessMode}
                accessModes={accessModes}
                accessModeReason={accessModeReason}
                interactionMode={interactionMode}
                planModeAvailable={planModeAvailable}
                planModeReason={planModeReason}
                onInteractionModeChange={onInteractionModeChange}
                onSelect={onSelectModelParams}
              />
            }
            extraControls={
              <div className="flex min-w-0 items-center gap-1.5">
                <ConversationModelPicker
                  credentials={credentials}
                  selectedCredentialId={selectedCredentialId}
                  selectedModel={selectedModel}
                  selectedEffort={selectedEffort}
                  selectedServiceTier={selectedServiceTier}
                  runtimes={runtimes}
                  runtimeKey={runtimeKey}
                  onRuntimeChange={onRuntimeChange}
                  loading={credentialsLoading}
                  error={credentialsError}
                  onRetry={onRetryCredentials}
                  onSelect={onSelectModelParams}
                  open={modelPickerOpen}
                  onOpenChange={onModelPickerOpenChange}
                />
                {selectedProjectId ? (
                  <div className="flex items-center gap-0.5">
                    {projectHasDefault ? (
                      <>
                        <Tooltip content="当前模型参数来自项目默认设置">
                          <span className="inline-flex h-6 items-center gap-1 rounded-md bg-cx-accent-soft px-1.5 text-[12px] font-medium text-cx-accent">
                            <Icon name="bookmark" size={11} />
                            项目默认
                          </span>
                        </Tooltip>
                        <Tooltip content="用当前选择覆盖项目默认">
                          <button
                            type="button"
                            className="h-6 rounded-md px-1.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-50"
                            onClick={onSetProjectDefault}
                            disabled={savingProjectDefault}
                          >
                            更新
                          </button>
                        </Tooltip>
                        <Tooltip content="清除项目默认，恢复用户或 Provider 默认">
                          <button
                            type="button"
                            className="is-clear h-6 rounded-md px-1.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-50"
                            onClick={onClearProjectDefault}
                            disabled={savingProjectDefault}
                          >
                            清除
                          </button>
                        </Tooltip>
                      </>
                    ) : (
                      <Tooltip content="将当前模型、推理参数和权限设为该项目新对话的默认值">
                        <button
                          type="button"
                          className="inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-50"
                          onClick={onSetProjectDefault}
                          disabled={savingProjectDefault}
                        >
                          <Icon name="bookmark" size={11} />
                          设为项目默认
                        </button>
                      </Tooltip>
                    )}
                  </div>
                ) : null}
              </div>
            }
          />
          <ComposerContextStrip
            projects={projects}
            selectedProjectId={selectedProjectId}
            onProjectChange={onProjectChange}
            projectDisabled={busy}
            modeDisabled={busy}
            branchDisabled={busy}
            onCreateProject={onCreateProject}
            onCreateFromPath={onCreateFromPath}
            creatingProject={creatingProject}
            projectsLoading={projectsLoading}
            projectsError={projectsError}
            onRetryProjects={onRetryProjects}
            preferPathInput={preferPathInput}
            requestPathInput={requestPathInput}
            recentPaths={recentPaths}
            workspaceMode={workspaceMode}
            onWorkspaceModeChange={onWorkspaceModeChange}
            worktreeBranch={worktreeBranch}
            onWorktreeBranchChange={onWorktreeBranchChange}
            existingWorktreePath={existingWorktreePath}
            onExistingWorktreePathChange={onExistingWorktreePathChange}
          />
        </motion.div>

        <motion.div
          className={cn("flex flex-wrap justify-center gap-2 transition-opacity duration-200", !composerEmpty && "pointer-events-none opacity-0")}
          aria-hidden={!composerEmpty || undefined}
          {...reveal(0.12)}
        >
          {SUGGESTIONS.map((item) => (
            <button
              key={item.label}
              type="button"
              tabIndex={composerEmpty ? 0 : -1}
              disabled={Boolean(item.workspace && !selectedProjectId)}
              title={item.workspace && !selectedProjectId ? "请先通过输入框下方的工作区入口选择目录。" : undefined}
              onClick={() => applySuggestion(item.prompt)}
              className="cx-press inline-flex h-8 items-center gap-1.5 rounded-full border border-cx-border px-3 text-[13px] text-cx-fg-2 hover:border-cx-border-strong hover:bg-cx-hover hover:text-cx-fg disabled:opacity-50"
            >
              <Icon name={item.icon} size={13} className="text-cx-fg-4" />
              {item.label}
            </button>
          ))}
        </motion.div>
      </div>
    </div>
  );
}
