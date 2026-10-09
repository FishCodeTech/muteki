"use client";

import { useLang } from "@/lib/i18n";
import { useEffect, useId, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import type { ConversationProject } from "@/lib/useConversation";
import {
  Button,
  Callout,
  Popover,
  SearchInput,
  Skeleton,
  Spinner,
  Tooltip,
  useListKeyboard,
  type ListOption,
} from "@/components/chat/ui";
import { Icon, type IconName } from "../Icon";
import { pathBasename } from "../chat/composer/format";
import { stripPillClass } from "../chat/composer/stripPill";
import { desktopChatBridge, type DesktopSelectedPath } from "@/lib/desktopChatBridge";
import { nativeCapability, nativeDisplayMessage, useNativeDesktopState } from "@/lib/nativeDesktop";

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
  | { kind: "add"; path: string }
  | { kind: "project"; project: ConversationProject }
  | { kind: "recent"; path: string };

function entryKey(entry: Entry): string {
  switch (entry.kind) {
    case "add":
      return `a:${entry.path}`;
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

const PATH_LIKE = /^(\/|~(\/|$)|\.{1,2}\/|[A-Za-z]:[\\/]|\\\\)/;

/** Home directories collapse to `~` so the distinguishing tail stays visible. */
function shortPath(path: string): string {
  return path.replace(/^(\/Users|\/home)\/[^/]+(?=\/|$)/, "~");
}

const footerRowClass = cn(
  "cx-press flex h-8 w-full items-center gap-2.5 rounded-lg px-2 text-left text-[13px] text-cx-fg-2",
  "hover:bg-cx-hover hover:text-cx-fg disabled:cursor-default disabled:opacity-50 disabled:hover:bg-transparent",
);

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
  const [pickingPath, setPickingPath] = useState(false);
  const [pathError, setPathError] = useState("");
  const [clientPick, setClientPick] = useState<DesktopSelectedPath | null>(null);
  const listId = useId();
  const pathErrorId = useId();
  const { desktop, state: nativeState, error: nativeStateError } = useNativeDesktopState();
  const nativePick = nativeCapability(nativeState, "pathSelection");
  const nativeAvailable = desktop && nativePick.available && Boolean(desktopChatBridge()?.selectPath);
  const nativeUnavailableReason = nativeStateError || (nativePick.available ? "桌面路径选择接口不可用，请在搜索框粘贴服务宿主路径" : nativeDisplayMessage(nativePick, lang === "en"));
  const currentProject = projects.find((project) => project.project_id === selectedProjectId);
  const trimmedQuery = query.trim();
  const needle = trimmedQuery.toLocaleLowerCase();
  const queryIsPath = Boolean(onCreateFromPath) && PATH_LIKE.test(trimmedQuery);
  const filteredProjects = useMemo(() => {
    if (!needle) return projects;
    return projects.filter((project) => {
      const name = project.name.toLocaleLowerCase();
      const root = project.root_path.toLocaleLowerCase();
      const base = pathBasename(project.root_path).toLocaleLowerCase();
      const short = shortPath(project.root_path).toLocaleLowerCase();
      return name.includes(needle) || root.includes(needle) || base.includes(needle) || short.includes(needle);
    });
  }, [needle, projects]);
  const filteredRecent = useMemo(() => {
    if (!onCreateFromPath) return [];
    const known = new Set(projects.map((project) => project.root_path));
    return recentPaths.filter((path) => !known.has(path) && (!needle || path.toLocaleLowerCase().includes(needle) || shortPath(path).toLocaleLowerCase().includes(needle)));
  }, [needle, onCreateFromPath, projects, recentPaths]);
  const entries = useMemo<Entry[]>(() => {
    const same = (path: string) => path === trimmedQuery || shortPath(path) === trimmedQuery;
    const exact = projects.some((project) => same(project.root_path)) || recentPaths.some(same);
    return [
      ...(queryIsPath && !exact ? [{ kind: "add" as const, path: trimmedQuery }] : []),
      ...filteredProjects.map((project) => ({ kind: "project" as const, project })),
      ...filteredRecent.map((path) => ({ kind: "recent" as const, path })),
    ];
  }, [filteredProjects, filteredRecent, projects, queryIsPath, recentPaths, trimmedQuery]);

  useEffect(() => {
    if (!requestPathInput) return;
    setOpen(true);
  }, [requestPathInput]);

  // A confirmation belongs to the service connection it was made against.
  useEffect(() => {
    setClientPick(null);
  }, [nativeState?.connectionVersion, nativeState?.serviceId, nativeState?.identityId]);

  const close = () => {
    setOpen(false);
    setQuery("");
    setPathError("");
    setClientPick(null);
  };

  const submitPath = async (path: string) => {
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

  const pickClientDirectory = async () => {
    const bridge = desktopChatBridge();
    if (!nativeAvailable || !bridge?.selectPath || pickingPath || creatingProject) return;
    setPickingPath(true);
    setPathError("");
    try {
      const result = await bridge.selectPath({ kind: "directory" });
      if (!result) return;
      if (!result.id || !result.path || result.host !== "desktop-client" || result.serverMapped !== false) {
        throw new Error("桌面返回了无效的路径或宿主范围。");
      }
      setClientPick(result);
    } catch (cause) {
      setPathError(nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, lang === "en"));
    } finally {
      setPickingPath(false);
    }
  };

  const activate = (entry: Entry) => {
    switch (entry.kind) {
      case "project":
        chooseProject(entry.project.project_id);
        return;
      case "add":
      case "recent":
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
      disabled: entry.kind !== "project" && creatingProject,
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
        <span className="shrink-0 text-[12px] font-normal text-cx-fg-4">{modeLabel}</span>
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

  const pickerUnavailableHint = preferPathInput && !desktop && onCreateFromPath
    ? "系统目录选择器不可用，请在上方粘贴目录路径"
    : "";
  const showSystemPicker = Boolean(onCreateProject) && !desktop && !preferPathInput;
  const showClientPicker = desktop && Boolean(onCreateFromPath);
  let lastGroup: "project" | "recent" | null = null;

  return (
    <Popover
      open={open}
      onOpenChange={(next) => { if (pickingPath) return; if (next) setOpen(true); else close(); }}
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
          onValueChange={(next) => { setQuery(next); setPathError(""); keyboard.setActiveIndex(0); }}
          onKeyDown={keyboard.onKeyDown}
          placeholder={onCreateFromPath ? "搜索项目，或粘贴路径添加" : "搜索项目"}
          aria-label={onCreateFromPath ? "搜索项目或输入工作目录路径" : "搜索项目"}
          aria-invalid={pathError ? true : undefined}
          aria-describedby={pathError ? pathErrorId : undefined}
          autoComplete="off"
          spellCheck={false}
          role="combobox"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={keyboard.activeIndex >= 0 && entries.length ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
          data-autofocus
          data-testid="conversation-directory-path"
          className={cn(
            "[&_input]:border-transparent [&_input]:bg-cx-hover [&_input]:shadow-none [&_input:focus]:bg-cx-elevated",
            queryIsPath && "[&_input]:font-cx-mono",
          )}
        />
        {pathError ? (
          <p
            id={pathErrorId}
            role="alert"
            aria-live="assertive"
            data-testid="conversation-directory-path-error"
            className="px-1 pt-1.5 text-[12px] leading-4 text-cx-danger"
          >
            {pathError}
          </p>
        ) : pickerUnavailableHint ? (
          <p className="px-1 pt-1.5 text-[12px] leading-4 text-cx-fg-4">{pickerUnavailableHint}</p>
        ) : null}
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
        <div id={listId} role="listbox" aria-label="已有项目" className="cx-scroll max-h-[min(340px,48vh)] overflow-y-auto overscroll-contain p-1.5">
          {entries.map((entry, index) => {
            const group = entry.kind === "add" ? null : entry.kind;
            const header = group && group !== lastGroup ? (group === "project" ? "项目" : "最近路径") : null;
            if (group) lastGroup = group;
            const active = index === keyboard.activeIndex;
            const selected = entry.kind === "project" && entry.project.project_id === selectedProjectId;
            const rowDisabled = entry.kind !== "project" && creatingProject;
            const fullPath = entry.kind === "project" ? entry.project.root_path : entry.path;
            const name = entry.kind === "add"
              ? "添加工作目录"
              : pathBasename(fullPath) || (entry.kind === "project" ? entry.project.name : fullPath);
            const icon: IconName = entry.kind === "add" ? "folderPlus" : entry.kind === "project" ? "folder" : "history";
            return (
              <div key={entryKey(entry)} className="contents">
                {header ? <div className="px-2 pb-1 pt-2 text-[11.5px] font-medium text-cx-fg-4 first:pt-1">{header}</div> : null}
                <div
                  id={`${listId}-opt-${index}`}
                  role="option"
                  aria-selected={selected}
                  aria-disabled={rowDisabled || undefined}
                  data-index={index}
                  data-active={active || undefined}
                  data-entry-kind={entry.kind}
                  title={fullPath}
                  onPointerMove={() => { if (!active) keyboard.setActiveIndex(index); }}
                  onPointerDown={(event) => event.preventDefault()}
                  onClick={() => { if (!rowDisabled) activate(entry); }}
                  className={cn(
                    "flex h-11 cursor-default select-none items-center gap-2.5 rounded-lg px-2 data-[active=true]:bg-cx-hover",
                    rowDisabled && "opacity-45",
                  )}
                >
                  <span
                    className={cn(
                      "grid size-7 shrink-0 place-items-center rounded-md",
                      selected ? "bg-cx-accent-soft text-cx-accent" : entry.kind === "add" ? "bg-cx-accent-soft text-cx-accent" : "bg-cx-hover text-cx-fg-3",
                    )}
                  >
                    {entry.kind === "add" && creatingProject ? <Spinner size={14} /> : <Icon name={icon} size={15} />}
                  </span>
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className={cn("truncate text-[13px] leading-5", selected ? "font-semibold text-cx-fg" : "font-medium text-cx-fg")}>
                      {name}
                    </span>
                    <span className="truncate font-cx-mono text-[11.5px] leading-4 text-cx-fg-4">{shortPath(fullPath)}</span>
                  </span>
                  {selected ? <Icon name="check" size={14} className="shrink-0 text-cx-accent" /> : null}
                  {entry.kind === "add" && active ? <Icon name="cornerDownLeft" size={13} className="shrink-0 text-cx-fg-4" /> : null}
                </div>
              </div>
            );
          })}
        </div>
      ) : (
        <div className="px-4 py-6 text-center text-[13px] text-cx-fg-4">
          {needle
            ? (onCreateFromPath ? "没有匹配的项目；输入以 / 或 ~ 开头的路径即可添加" : "没有匹配的项目")
            : "还没有项目，选择或粘贴一个工作目录"}
        </div>
      )}

      {clientPick ? (
        <div className="border-t border-cx-border-subtle p-2" data-testid="conversation-client-path-confirm">
          <div className="flex flex-col gap-2 rounded-lg bg-cx-hover p-2.5">
            <div className="flex min-w-0 items-center gap-2">
              <Icon name="monitor" size={14} className="shrink-0 text-cx-fg-3" />
              <span className="min-w-0 flex-1 truncate font-cx-mono text-[12px] text-cx-fg" title={clientPick.path}>{shortPath(clientPick.path)}</span>
            </div>
            <p className="text-[11.5px] leading-4 text-cx-fg-4">这是桌面本机路径。服务若在远程或容器中运行，请先确认它能访问该路径。</p>
            <div className="flex justify-end gap-1.5">
              <Button size="xs" variant="ghost" onClick={() => setClientPick(null)} disabled={creatingProject}>取消</Button>
              <Button size="xs" variant="primary" loading={creatingProject} onClick={() => void submitPath(clientPick.path)}>确认可访问并使用</Button>
            </div>
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-0.5 border-t border-cx-border-subtle p-1.5">
          {showSystemPicker ? (
            <button type="button" disabled={creatingProject} onClick={() => void createProject()} className={footerRowClass}>
              {creatingProject ? <Spinner size={15} className="text-cx-fg-3" /> : <Icon name="folderPlus" size={15} className="text-cx-fg-3" />}
              <span>{creatingProject ? "正在选择工作目录…" : "选择新的工作目录…"}</span>
            </button>
          ) : null}
          {showClientPicker ? (
            <Tooltip content={nativeAvailable ? "从桌面本机选择，确认服务可访问后使用" : nativeUnavailableReason} placement="right">
              <span className="flex">
                <button
                  type="button"
                  disabled={!nativeAvailable || pickingPath || creatingProject}
                  onClick={() => void pickClientDirectory()}
                  className={footerRowClass}
                  data-testid="conversation-client-path-pick"
                >
                  {pickingPath ? <Spinner size={15} className="text-cx-fg-3" /> : <Icon name="folderPlus" size={15} className="text-cx-fg-3" />}
                  <span>{pickingPath ? "正在选择…" : "从本机选择目录…"}</span>
                </button>
              </span>
            </Tooltip>
          ) : null}
          <button
            type="button"
            data-testid="conversation-skip-project"
            onClick={() => chooseProject("")}
            className={footerRowClass}
          >
            <Icon name="messages" size={15} className="text-cx-fg-3" />
            <span className="flex-1">不在项目中工作</span>
            {!selectedProjectId ? <Icon name="check" size={14} className="text-cx-accent" /> : null}
          </button>
        </div>
      )}
    </Popover>
  );
}
