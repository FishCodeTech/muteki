"use client";

import type { LargePasteSelection } from "@/lib/composerLargePasteKeep";

import React, { useEffect, useRef, useState } from "react";
import { motion } from "motion/react";
import { cn } from "@/lib/cn";
import { Icon, Select, useReducedMotion, EASE_OUT } from "@muteki/ui";
import {
  initialHomePromptSeed,
  msUntilNextTimeSlot,
  readHomeClock,
  resolveConversationHomePrompt,
  sameHomeClockSlot,
  type ConversationHomeSuggestion,
  type HomeClockSnapshot,
} from "@/lib/conversationHomePrompt";
export type { ConversationHomeSuggestion };
import { PromptBar, type PromptBarAttachment } from "@/components/chat/composer/MutekiPromptBar";
import { stripPillClass } from "../chat/composer/stripPill";
import { ConversationModelPicker, type ProjectDefaultControl } from "./ConversationModelPicker";
import { ConversationComposerModes, type ConversationComposerModesHandle } from "./ConversationComposerModes";
import { allCredentialModels } from "@/lib/useConversation";
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
  title?: string;
  subtitle?: string;
  draftKey?: string;
  introAccessory?: React.ReactNode;
  placeholder?: string;
  suggestions?: ConversationHomeSuggestion[];
  projectSelectionOnly?: boolean;
  hideWorkspaceControls?: boolean;
  hideReadinessGuide?: boolean;
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
  composerModesRef?: React.Ref<ConversationComposerModesHandle>;
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
  onOpenRepoLessProjectDialog?: () => void;
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
  projectDefault?: ProjectDefaultControl;
  onHistoryNavigate?: (direction: "older" | "newer") => boolean;
  historyBrowsing?: boolean;
  historyStatus?: string;
  onStashShortcut?: () => void;
  onOpenStashPanel?: () => void;
  stashCount?: number;
  className?: string;
  modelPickerOpen?: boolean;
  onModelPickerOpenChange?: (open: boolean) => void;
  modelPickerInstant?: boolean;
  undoScope?: string;
  onRestoreAttachments?: (attachments: PromptBarAttachment[]) => void;
}

export function ConversationHome({
  title,
  subtitle,
  draftKey,
  introAccessory,
  placeholder,
  suggestions,
  projectSelectionOnly = false,
  hideWorkspaceControls = false,
  hideReadinessGuide = false,
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
  composerModesRef,
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
  onOpenRepoLessProjectDialog,
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
  projectDefault,
  onHistoryNavigate,
  historyBrowsing,
  historyStatus,
  onStashShortcut,
  onOpenStashPanel,
  stashCount,
  className = "",
  modelPickerOpen,
  onModelPickerOpenChange,
  modelPickerInstant,
  undoScope,
  onRestoreAttachments,
}: ConversationHomeProps) {
  const reduced = useReducedMotion();
  const selectedCredential = credentials.find((row) => row.id === selectedCredentialId);
  const modelMetadata = selectedCredential ? allCredentialModels(selectedCredential).find((row) => row.id === selectedModel) : undefined;
  const [clock, setClock] = useState<HomeClockSnapshot | null>(null);
  const [visitSeed, setVisitSeed] = useState(0);
  const prevDraftKeyRef = useRef(draftKey);
  const initialEmptyRef = useRef(!prompt.trim() && !attachments.length);
  const hasToggledEmptyRef = useRef(false);

  useEffect(() => {
    if (title !== undefined) return;
    let timerId: number | undefined;
    let disposed = false;

    const syncNow = () => {
      const next = readHomeClock(new Date());
      setClock((prev) => (sameHomeClockSlot(prev, next) ? prev : next));
    };

    const scheduleNext = () => {
      if (disposed) return;
      const delay = Math.min(msUntilNextTimeSlot(new Date()), 60_000);
      timerId = window.setTimeout(() => {
        if (disposed) return;
        syncNow();
        scheduleNext();
      }, delay);
    };

    setVisitSeed((prev) => (prev === 0 ? initialHomePromptSeed(new Date()) : prev));
    syncNow();
    scheduleNext();

    const handleVisibilityChange = () => {
      if (document.visibilityState === "visible") syncNow();
    };
    const handleWindowWake = () => {
      syncNow();
    };

    document.addEventListener("visibilitychange", handleVisibilityChange);
    window.addEventListener("focus", handleWindowWake);
    window.addEventListener("pageshow", handleWindowWake);

    return () => {
      disposed = true;
      if (timerId !== undefined) window.clearTimeout(timerId);
      document.removeEventListener("visibilitychange", handleVisibilityChange);
      window.removeEventListener("focus", handleWindowWake);
      window.removeEventListener("pageshow", handleWindowWake);
    };
  }, [title]);

  useEffect(() => {
    if (prevDraftKeyRef.current === draftKey) return;
    prevDraftKeyRef.current = draftKey;
    if (title !== undefined) return;
    const next = readHomeClock(new Date());
    setClock((prev) => (sameHomeClockSlot(prev, next) ? prev : next));
    setVisitSeed((prev) => prev + 1);
  }, [draftKey, title]);

  const selectedProject = projects.find((project) => project.project_id === selectedProjectId);
  const homePrompt = resolveConversationHomePrompt({
    clock,
    seed: visitSeed,
    projectName: selectedProject?.name,
    hasSelectedProject: Boolean(selectedProjectId),
    hasProjects: projects.length > 0,
    workspaceMode,
    worktreeBranch,
    interactionMode,
    attachmentCount: attachments.length,
  });
  const resolvedTitle = title ?? homePrompt.title;
  const resolvedSubtitle = subtitle ?? homePrompt.subtitle;
  const resolvedPlaceholder = placeholder ?? homePrompt.placeholder;
  const resolvedSuggestions = suggestions ?? homePrompt.suggestions;

  const reveal = (delay: number) => (reduced
    ? { initial: { opacity: 0 }, animate: { opacity: 1, transition: { duration: 0.14, ease: EASE_OUT } } }
    : {
      initial: { opacity: 0, transform: "translateY(10px)" },
      animate: {
        opacity: 1,
        transform: "translateY(0px)",
        transition: { duration: 0.42, delay, ease: EASE_OUT },
      },
    });
  const rootRef = useRef<HTMLDivElement | null>(null);
  const applySuggestion = (text: string) => {
    if (onPromptDocumentChange) onPromptDocumentChange(documentFromPromptAndRefs(text, []));
    else onPromptChange(text);
    window.requestAnimationFrame(() => {
      const composer = rootRef.current?.querySelector<HTMLElement>("[data-c34-composer]");
      composer?.focus();
    });
  };
  const composerEmpty = !prompt.trim() && !attachments.length;
  useEffect(() => {
    if (composerEmpty !== initialEmptyRef.current) hasToggledEmptyRef.current = true;
  }, [composerEmpty]);
  const staggerChips = composerEmpty && !hasToggledEmptyRef.current && composerEmpty === initialEmptyRef.current;

  return (
    <div ref={rootRef} className={cn("cx-conversation-home cx-scroll flex min-h-0 flex-1 flex-col items-center overflow-y-auto", className)}>
      <div className="workbench-welcome-content conversation-home-content flex flex-col items-center pb-8">
        <motion.div className="workbench-welcome-intro" {...reveal(0)}>
          <h1 className="workbench-welcome-title">{resolvedTitle}</h1>
          <p className="workbench-welcome-subtitle">{resolvedSubtitle}</p>
        </motion.div>

        <motion.div className="w-full" {...reveal(0.06)}>
          <PromptBar
            value={prompt}
            onChange={onPromptChange}
            promptDocument={promptDocument}
            onPromptDocumentChange={onPromptDocumentChange}
            onSubmit={onSubmit}
            busy={busy}
            submitDisabled={readiness?.blockers.some(blocker => blocker.kind === "model_unavailable")}
            placeholder={resolvedPlaceholder}
            attachments={attachments}
            onAddAttachment={onAddAttachment}
            undoScope={undoScope}
            onRestoreAttachments={onRestoreAttachments}
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
                selectedModelMetadata={modelMetadata}
                selectedCredential={selectedCredential}
                selectedEffort={selectedEffort}
                selectedServiceTier={selectedServiceTier}
                ref={composerModesRef}
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
                  separateEffortControl
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
                  instant={modelPickerInstant}
                  projectDefault={!hideWorkspaceControls && selectedProjectId ? projectDefault : undefined}
                />
              </div>
            }
          />
          {projectSelectionOnly ? (
            <div
              className="cx-context-strip flex min-w-0 flex-wrap items-center gap-x-1 gap-y-1 px-1.5 pt-2"
              data-has-project={selectedProjectId ? "true" : "false"}
            >
              <Select<string>
                value={selectedProjectId}
                onChange={onProjectChange}
                disabled={busy}
                ariaLabel="研究项目"
                placement="top-start"
                popoverClassName="w-[min(320px,calc(100vw-24px))]"
                searchable={projects.length > 6}
                searchPlaceholder="搜索研究项目…"
                options={[
                  { value: "", label: "独立研究", icon: "folder" },
                  ...projects.map((project) => ({
                    value: project.project_id,
                    label: project.name,
                    icon: "folder" as const,
                  })),
                ]}
                trigger={
                  <button
                    type="button"
                    disabled={busy}
                    aria-label="研究项目"
                    className={stripPillClass(true)}
                  >
                    <Icon name="folder" size={13} className="shrink-0" />
                    <span className="max-w-[220px] truncate">
                      {projects.find((project) => project.project_id === selectedProjectId)?.name ?? "独立研究"}
                    </span>
                    <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
                  </button>
                }
              />
              {onOpenRepoLessProjectDialog ? (
                <button
                  type="button"
                  disabled={busy || creatingProject}
                  onClick={onOpenRepoLessProjectDialog}
                  className={stripPillClass(true)}
                >
                  <Icon name="plus" size={13} className="shrink-0" />
                  <span>新建研究项目</span>
                </button>
              ) : null}
              {projectsError && onRetryProjects ? (
                <button
                  type="button"
                  onClick={onRetryProjects}
                  className={cn(stripPillClass(true), "max-w-full text-cx-warning hover:text-cx-fg")}
                >
                  <span className="truncate">加载项目失败（{projectsError}），重试</span>
                </button>
              ) : null}
            </div>
          ) : !hideWorkspaceControls ? <ComposerContextStrip
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
          /> : null}
          {!hideReadinessGuide && readiness && onRetryReadiness && onProbeAgent && onDismissGuide ? (
            <div className="mt-3">
              <ConversationReadinessBanner
                readiness={readiness}
                probing={probing}
                onRetry={onRetryReadiness}
                onProbe={onProbeAgent}
                onDismissGuide={onDismissGuide}
              />
            </div>
          ) : null}
        </motion.div>

        {introAccessory ? <div className="mt-8 w-full">{introAccessory}</div> : null}

        <motion.div
          className={cn("mt-8 flex flex-wrap justify-center gap-2", !composerEmpty && "pointer-events-none")}
          aria-hidden={!composerEmpty || undefined}
          initial={reduced ? { opacity: 0 } : { opacity: 0, transform: "translateY(10px)" }}
          animate={reduced
            ? { opacity: composerEmpty ? 1 : 0, transition: { duration: 0.14, ease: EASE_OUT } }
            : {
              opacity: composerEmpty ? 1 : 0,
              transform: "translateY(0px)",
              transition: { duration: 0.2, delay: staggerChips ? 0.12 : 0, ease: EASE_OUT },
            }}
        >
          {resolvedSuggestions.map((item) => (
            <button
              key={item.label}
              type="button"
              tabIndex={composerEmpty ? 0 : -1}
              disabled={Boolean(item.workspace && !selectedProjectId)}
              title={item.workspace && !selectedProjectId ? "先在输入框下方选一个工作目录即可使用" : undefined}
              onClick={() => applySuggestion(item.prompt)}
              className="cx-press inline-flex h-8 items-center gap-1.5 rounded-xl bg-cx-hover px-3 text-cx-sm text-cx-fg-2 hover:bg-cx-active hover:text-cx-fg disabled:opacity-50"
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
