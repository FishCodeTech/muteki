"use client";

import { useLang } from "@/lib/i18n";
import { useEffect, useId, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import type { ConversationProject } from "@/lib/useConversation";
import {
  Button,
  Callout,
  Input,
  Popover,
  SearchInput,
  Skeleton,
  Spinner,
  Tooltip,
  useListKeyboard,
  type ListOption,
} from "@/components/chat/ui";
import { Icon } from "../Icon";
import { pathBasename } from "../chat/composer/format";
import { stripPillClass } from "../chat/composer/stripPill";
import { NativePathPicker } from "@/components/NativePathPicker";
import { desktopChatBridge } from "@/lib/desktopChatBridge";

export interface ConversationContextPickerProps {
  projects: ConversationProject[];
  selectedProjectId: string;
  onProjectChange: (id: string) => void;
  onCreateProject?: () => Promise<string | null>;
  onCreateFromPath?: (path: string) => Promise<string | null>;
  creatingProject?: boolean;
  disabled?: boolean;
  loading?: boolean;
  error?: string;
  onRetry?: () => void;
  preferPathInput?: boolean;
  requestPathInput?: boolean;
  recentPaths?: string[];
  /** 触发器主文案：会话工作目录名（basename）。 */
  directoryLabel?: string;
  /** 完整路径，用于 title / tooltip。 */
  directoryTitle?: string;
  /** 次要模式标签，例如「本地检出」。 */
  modeLabel?: string;
  className?: string;
}

type Entry =
  | { kind: "project"; project: ConversationProject }
  | { kind: "recent"; path: string };

function entryKey(entry: Entry): string {
  switch (entry.kind) {
    case "project":
      return `p:${entry.project.project_id}`;
    case "recent":
      return `r:${entry.path}`;
    default: {
      const exhaustive: never = entry;
      return exhaustive;
    }
  }
}

export function ConversationContextPicker({
  projects,
  selectedProjectId,
  onProjectChange,
  onCreateProject,
  onCreateFromPath,
  creatingProject = false,
  disabled = false,
  loading = false,
  error = "",
  onRetry,
  preferPathInput = false,
  requestPathInput = false,
  recentPaths = [],
  directoryLabel,
  directoryTitle,
  modeLabel,
  className = "",
}: ConversationContextPickerProps) {
  const { lang } = useLang();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [pathDraft, setPathDraft] = useState("");
  const [pickingPath, setPickingPath] = useState(false);
  const [pathError, setPathError] = useState("");
  const listId = useId();
  const pathErrorId = useId();
  const currentProject = projects.find((project) => project.project_id === selectedProjectId);
  const needle = query.trim().toLocaleLowerCase();
  const filteredProjects = useMemo(() => {
    if (!needle) return projects;
    return projects.filter((project) => {
      const name = project.name.toLocaleLowerCase();
      const root = project.root_path.toLocaleLowerCase();
      const base = pathBasename(project.root_path).toLocaleLowerCase();
      return name.includes(needle) || root.includes(needle) || base.includes(needle);
    });
  }, [needle, projects]);
  const filteredRecent = useMemo(
    () => (onCreateFromPath ? recentPaths.filter((path) => !needle || path.toLocaleLowerCase().includes(needle)) : []),
    [needle, onCreateFromPath, recentPaths],
  );
  const entries = useMemo<Entry[]>(() => [
    ...filteredProjects.map((project) => ({ kind: "project" as const, project })),
    ...filteredRecent.map((path) => ({ kind: "recent" as const, path })),
  ], [filteredProjects, filteredRecent]);

  useEffect(() => {
    if (!requestPathInput) return;
    setOpen(true);
  }, [requestPathInput]);

  const close = () => {
    setOpen(false);
    setQuery("");
    setPathError("");
  };

  const submitPath = async (path = pathDraft) => {
    if (!onCreateFromPath || creatingProject) return;
    const trimmed = path.trim();
    if (!trimmed) {
      setPathError("请输入有效的工作目录路径");
      return;
    }
    setPathError("");
    try {
      const projectId = await onCreateFromPath(trimmed);
      if (!projectId) {
        // Caller swallowed the failure without throwing; keep the draft editable.
        setPathError("无法添加该工作目录，请检查路径后重试");
        return;
      }
      onProjectChange(projectId);
      setPathDraft("");
      setPathError("");
      close();
    } catch (exc) {
      const message = exc instanceof Error ? exc.message : String(exc);
      setPathError(message);
    }
  };

  const chooseProject = (projectId: string) => {
    onProjectChange(projectId);
    close();
  };

  const createProject = async () => {
    if (!onCreateProject || creatingProject) return;
    const projectId = await onCreateProject();
    if (!projectId) return;
    onProjectChange(projectId);
    close();
  };

  const activate = (entry: Entry) => {
    switch (entry.kind) {
      case "project":
        chooseProject(entry.project.project_id);
        return;
      case "recent":
        setPathDraft(entry.path);
        void submitPath(entry.path);
        return;
      default: {
        const exhaustive: never = entry;
        return exhaustive;
      }
    }
  };

  const keyboardOptions = useMemo<ListOption[]>(
    () => entries.map((entry) => ({
      value: entryKey(entry),
      label: entry.kind === "project" ? entry.project.name : entry.path,
      disabled: entry.kind === "recent" && creatingProject,
    })),
    [creatingProject, entries],
  );
  const keyboard = useListKeyboard(keyboardOptions, (key) => {
    const entry = entries.find((item) => entryKey(item) === key);
    if (entry) activate(entry);
  });

  useEffect(() => {
    if (!open) return;
    const index = entries.findIndex((entry) => entry.kind === "project" && entry.project.project_id === selectedProjectId);
    keyboard.setActiveIndex(index >= 0 ? index : 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const resolvedRoot = (directoryTitle || currentProject?.root_path || "").trim();
  const hasDirectory = Boolean(currentProject || directoryLabel);
  const triggerLabel = hasDirectory
    ? (directoryLabel || pathBasename(resolvedRoot) || currentProject?.name || "工作目录")
    : (disabled ? "未绑定工作目录" : "选择工作目录");
  const triggerTitle = currentProject || resolvedRoot
    ? [currentProject?.name, resolvedRoot || directoryTitle].filter(Boolean).join("\n")
    : (disabled
      ? "当前会话未绑定工作目录；文件/终端需新建已绑定目录的会话"
      : "选择本机工作目录");

  const labelBody = (
    <>
      <Icon name={hasDirectory || disabled ? "folder" : "folderPlus"} size={13} className="shrink-0" />
      <span className="min-w-0 truncate">{triggerLabel}</span>
      {modeLabel && hasDirectory ? (
        <span className="shrink-0 text-[11.5px] font-normal text-cx-fg-4">{modeLabel}</span>
      ) : null}
    </>
  );

  // 会话内项目已锁定：只读展示目录名，不呈现可切换选择器。
  if (disabled) {
    return (
      <Tooltip content={<span className="whitespace-pre-line">{triggerTitle}</span>}>
        <span
          tabIndex={0}
          className={cn(stripPillClass(false), "max-w-[260px]", className)}
          data-readonly="true"
          aria-label={`工作目录：${triggerTitle.replace(/\n/g, " ")}`}
        >
          {labelBody}
        </span>
      </Tooltip>
    );
  }

  let lastKind: Entry["kind"] | null = null;

  return (
    <Popover
      open={open}
      onOpenChange={(next) => { if (pickingPath) return; setOpen(next); if (!next) { setQuery(""); setPathError(""); } }}
      placement="top-start"
      offset={8}
      ariaLabel="选择项目"
      className="w-[min(380px,calc(100vw-16px))] p-0"
      trigger={(
        <button
          type="button"
          title={triggerTitle}
          className={cn(stripPillClass(true), "max-w-[260px]", className)}
        >
          {labelBody}
          <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
        </button>
      )}
    >
      <div className="border-b border-cx-border-subtle p-2">
        <SearchInput
          value={query}
          onValueChange={(next) => { setQuery(next); keyboard.setActiveIndex(0); }}
          onKeyDown={keyboard.onKeyDown}
          placeholder="搜索项目或最近路径"
          aria-label="搜索项目"
          autoComplete="off"
          role="combobox"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={keyboard.activeIndex >= 0 && entries.length ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
          data-autofocus
          className="[&_input]:border-transparent [&_input]:bg-cx-hover [&_input]:shadow-none [&_input:focus]:bg-cx-elevated"
        />
      </div>

      {loading ? (
        <div className="flex flex-col gap-2.5 p-3" data-testid="conversation-projects-loading" role="status" aria-label="正在加载项目">
          {[0, 1, 2].map((row) => (
            <div key={row} className="flex items-center gap-2.5">
              <Skeleton className="size-4 rounded" />
              <div className="flex flex-1 flex-col gap-1.5">
                <Skeleton className="h-3 w-1/3" />
                <Skeleton className="h-2.5 w-2/3" />
              </div>
            </div>
          ))}
        </div>
      ) : error ? (
        <div className="p-2" data-testid="conversation-projects-error">
          <Callout
            tone="danger"
            action={onRetry ? <Button size="xs" variant="secondary" icon="retry" onClick={onRetry}>重试</Button> : undefined}
          >
            {error}
          </Callout>
        </div>
      ) : entries.length ? (
        <div id={listId} role="listbox" aria-label="已有项目" className="cx-scroll max-h-72 overflow-y-auto overscroll-contain p-1.5">
          {entries.map((entry, index) => {
            const header = entry.kind !== lastKind ? (entry.kind === "project" ? "项目" : "最近路径") : null;
            lastKind = entry.kind;
            const active = index === keyboard.activeIndex;
            const selected = entry.kind === "project" && entry.project.project_id === selectedProjectId;
            const rowDisabled = entry.kind === "recent" && creatingProject;
            return (
              <div key={entryKey(entry)} className="contents">
                {header ? <div className="px-2 pb-1 pt-2 text-[11px] font-medium text-cx-fg-4 first:pt-1">{header}</div> : null}
                <div
                  id={`${listId}-opt-${index}`}
                  role="option"
                  aria-selected={selected}
                  aria-disabled={rowDisabled || undefined}
                  data-index={index}
                  data-active={active || undefined}
                  onPointerMove={() => { if (!active) keyboard.setActiveIndex(index); }}
                  onPointerDown={(event) => event.preventDefault()}
                  onClick={() => { if (!rowDisabled) activate(entry); }}
                  className={cn(
                    "flex min-h-10 cursor-default select-none items-center gap-2.5 rounded-lg px-2 py-1.5 data-[active=true]:bg-cx-hover",
                    rowDisabled && "opacity-45",
                  )}
                >
                  <Icon name={entry.kind === "project" ? "folder" : "history"} size={15} className="shrink-0 text-cx-fg-3" />
                  {entry.kind === "project" ? (
                    <span className="flex min-w-0 flex-1 flex-col">
                      <span className="truncate text-[13px] font-medium leading-5 text-cx-fg">
                        {pathBasename(entry.project.root_path) || entry.project.name}
                      </span>
                      <span className="truncate font-cx-mono text-[11px] leading-4 text-cx-fg-4">{entry.project.root_path}</span>
                    </span>
                  ) : (
                    <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px] text-cx-fg-2">{entry.path}</span>
                  )}
                  <Icon name="check" size={14} className={cn("shrink-0 text-cx-fg", selected ? "opacity-100" : "opacity-0")} />
                </div>
              </div>
            );
          })}
        </div>
      ) : (
        <div className="px-4 py-6 text-center text-[12.5px] text-cx-fg-4">
          {projects.length || recentPaths.length ? "没有匹配的项目" : "还没有项目，请选择一个工作目录"}
        </div>
      )}

      <div className="flex flex-col gap-1 border-t border-cx-border-subtle bg-cx-bg-subtle p-1.5">
        {onCreateFromPath ? (
          <form
            className="flex flex-col gap-1.5 px-1.5 pb-1 pt-1"
            data-testid="conversation-directory-path-form"
            onSubmit={(event) => {
              event.preventDefault();
              void submitPath();
            }}
          >
            {desktopChatBridge() ? <NativePathPicker id="conversation-directory-path" label={lang === "en" ? "Workspace path" : "工作目录路径"} kind="directory" value={pathDraft} onChange={path => { setPathDraft(path); setPathError(""); }} onServerPath={path => { void submitPath(path); }} disabled={creatingProject} onBusyChange={setPickingPath} inputTestId="conversation-directory-path" invalid={Boolean(pathError)} describedBy={pathError ? pathErrorId : undefined} placeholder={lang === "en" ? "Workspace directory accessible to the service" : "当前服务可访问的工作目录"} /> : <><label className="text-[11.5px] text-cx-fg-3" htmlFor="conversation-directory-path">
              {preferPathInput ? "系统目录选择器不可用，请输入路径" : "或输入目录路径"}
            </label>
            <div className="flex gap-1.5">
              <Input
                id="conversation-directory-path"
                aria-label="工作目录路径"
                aria-invalid={pathError ? true : undefined}
                aria-describedby={pathError ? pathErrorId : undefined}
                data-testid="conversation-directory-path"
                invalid={Boolean(pathError)}
                value={pathDraft}
                onChange={(event) => {
                  setPathDraft(event.target.value);
                  if (pathError) setPathError("");
                }}
                placeholder="/workspace 或 ~/src"
                autoComplete="off"
                spellCheck={false}
                size="sm"
                className="flex-1 font-cx-mono text-[12.5px]"
              />
              <Button size="sm" variant="secondary" disabled={creatingProject || !pathDraft.trim()} type="submit" className="h-8">
                添加
              </Button>
            </div>
            </>}
            {pathError ? (
              <p
                id={pathErrorId}
                role="alert"
                aria-live="assertive"
                data-testid="conversation-directory-path-error"
                className="text-[12px] leading-4 text-cx-danger"
              >
                {pathError}
              </p>
            ) : null}
          </form>
        ) : null}
        {onCreateProject ? (
          <button
            type="button"
            disabled={creatingProject}
            onClick={() => void createProject()}
            className="cx-press flex h-9 w-full items-center gap-2.5 rounded-lg px-2 text-left text-[13px] text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg disabled:opacity-60"
          >
            {creatingProject ? <Spinner size={15} className="text-cx-fg-3" /> : <Icon name="folderPlus" size={15} className="text-cx-fg-3" />}
            <span>{creatingProject ? (preferPathInput ? "正在添加工作目录…" : "正在选择工作目录…") : "选择新的工作目录…"}</span>
          </button>
        ) : null}
        <button
          type="button"
          data-testid="conversation-skip-project"
          onClick={() => chooseProject("")}
          className="cx-press flex h-9 w-full items-center gap-2.5 rounded-lg px-2 text-left text-[13px] text-cx-fg-2 hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name="messages" size={15} className="text-cx-fg-3" />
          <span className="flex-1">不在项目中工作</span>
          {!selectedProjectId ? <Icon name="check" size={14} className="text-cx-fg" /> : null}
        </button>
      </div>
    </Popover>
  );
}
