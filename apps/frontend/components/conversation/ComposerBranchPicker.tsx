"use client";

import { useCallback, useEffect, useId, useMemo, useState } from "react";
import Link from "next/link";
import { cn } from "@/lib/cn";
import { OptionList, Popover, Spinner, Tooltip, useListKeyboard, type ListOption } from "@muteki/ui";
import { stripPillClass } from "@/components/chat/composer/stripPill";
import { checkoutSharedBranch, useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { Icon } from "@muteki/ui";

export interface ComposerBranchPickerProps {
  projectId?: string;
  threadId?: string;
  disabled?: boolean;
  className?: string;
}

export function ComposerBranchPicker({
  projectId = "",
  threadId = "",
  disabled = false,
  className = "",
}: ComposerBranchPickerProps) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState("");
  const shared = useSharedGitStatus(threadId, projectId);
  const status = shared.status;
  const loading = shared.loading && !status;
  const error = actionError || shared.error;
  const refreshStatus = shared.refresh;

  const loadStatus = useCallback(async () => {
    setActionError("");
    await refreshStatus();
  }, [refreshStatus]);

  useEffect(() => {
    if (!open) return;
    const timer = window.setInterval(() => { void refreshStatus(); }, 5000);
    return () => window.clearInterval(timer);
  }, [open, refreshStatus]);

  const filteredBranches = useMemo(() => {
    const branches = status?.branches ?? [];
    const needle = query.trim().toLocaleLowerCase();
    if (!needle) return branches;
    return branches.filter((branch) => branch.toLocaleLowerCase().includes(needle));
  }, [query, status?.branches]);

  const trimmedQuery = query.trim();
  const canCreate =
    Boolean(status?.is_repo)
    && trimmedQuery.length > 0
    && !(status?.branches ?? []).includes(trimmedQuery);

  const switchBlocked = Boolean(
    disabled || status?.read_only || status?.branch_switch_blocked,
  );

  const close = () => {
    setOpen(false);
    setQuery("");
  };

  const switchBranch = async (branch: string, create = false) => {
    if ((!threadId && !projectId) || busy || loading || switchBlocked) return;
    setBusy(true);
    setActionError("");
    try {
      await checkoutSharedBranch(threadId, projectId, branch, { create });
      close();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "切换分支失败");
      await refreshStatus();
    } finally {
      setBusy(false);
    }
  };

  const listId = useId();
  const options = useMemo<ListOption[]>(() => {
    const rows: ListOption[] = filteredBranches.map((branch) => ({
      value: branch,
      label: branch,
      icon: "gitBranch",
      disabled: switchBlocked || busy || loading,
      trailing: branch === status?.current_branch ? <span className="text-cx-xs text-cx-fg-4">当前</span> : undefined,
    }));
    if (canCreate) {
      rows.push({ value: `\u0000create:${trimmedQuery}`, label: `创建并切换到 ${trimmedQuery}`, textValue: trimmedQuery, icon: "plus", disabled: switchBlocked || busy || loading });
    }
    return rows;
  }, [canCreate, filteredBranches, status?.current_branch, trimmedQuery, switchBlocked, busy, loading]);
  const choose = (value: string) => {
    if (value.startsWith("\u0000create:")) void switchBranch(value.slice("\u0000create:".length), true);
    else if (value !== status?.current_branch) void switchBranch(value);
    else close();
  };
  const keyboard = useListKeyboard(options, choose);

  if (!threadId && !projectId) {
    return (
      <span className={cn(stripPillClass(false), "text-cx-fg-4", className)}>
        <Icon name="gitBranch" size={13} />
        <span>未选择目录</span>
      </span>
    );
  }

  if (loading && !status) {
    return (
      <span className={cn(stripPillClass(false), "text-cx-fg-4", className)}>
        <Spinner size={12} />
        <span>读取分支…</span>
      </span>
    );
  }

  if (!status?.is_repo) {
    return (
      <Tooltip content={error || "当前工作目录不是 Git 仓库"}>
        <span className={cn("max-w-[220px]", stripPillClass(false), "text-cx-fg-4", className)}>
          <Icon name="gitBranch" size={13} />
          <span className="truncate">{error || "非 Git 仓库"}</span>
        </span>
      </Tooltip>
    );
  }

  const branches = status.branches ?? [];
  const current = status.current_branch || "";
  const deleted =
    Boolean(current)
    && branches.length > 0
    && !branches.includes(current);
  const label = status.detached_head
    ? (`detached ${(status.detached_sha || "").slice(0, 7)}`.trim() || "detached HEAD")
    : deleted
      ? `${current}（已删除）`
      : (current || "未知分支");
  const occupiedHint = status.read_only
    ? "当前仅支持读取 Git 状态和差异"
    : status.branch_switch_blocked
      ? "此目录有正在运行的对话，暂不能切换分支。等待任务结束，或选择新建 worktree 使用独立分支。"
      : disabled
        ? "当前对话正在处理请求，暂不能切换分支。"
        : "切换分支会影响使用此检出目录的所有对话；需要并行使用不同分支时，请新建 worktree。";
  const trigger = (
    <button
      type="button"
      disabled={busy}
      className={cn("max-w-[240px]", stripPillClass(true), className)}
      aria-label={`Git 分支：${label}`}
    >
      {busy ? <Spinner size={12} /> : <Icon name="gitBranch" size={13} />}
      <span className="min-w-0 truncate font-cx-mono text-cx-xs">{label}</span>
      {status.dirty ? <span className="size-1.5 shrink-0 rounded-full bg-cx-warning" aria-label="有未提交改动" /> : null}
      <Icon name="chevronDown" size={12} className="text-cx-fg-4" />
    </button>
  );

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (next) {
          void loadStatus();
          keyboard.setActiveIndex(0);
        } else setQuery("");
      }}
      placement="top-end"
      ariaLabel="选择 Git 分支"
      className="w-[320px]"
      trigger={(
        <span className="inline-flex">
          <Tooltip content={occupiedHint || (status.dirty ? `${label}（工作区有未提交改动）` : label)}>{trigger}</Tooltip>
        </span>
      )}
    >
      <div className="flex items-center gap-2 border-b border-cx-border-subtle px-3">
        <Icon name="search" size={14} className="shrink-0 text-cx-fg-4" />
        <input
          data-autofocus
          aria-label="搜索分支"
          role="combobox"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={keyboard.activeIndex >= 0 ? `${listId}-opt-${keyboard.activeIndex}` : undefined}
          value={query}
          onChange={(event) => { setQuery(event.target.value); keyboard.setActiveIndex(0); }}
          onKeyDown={keyboard.onKeyDown}
          placeholder="搜索或创建分支"
          autoComplete="off"
          className="h-10 min-w-0 flex-1 bg-transparent text-cx-sm text-cx-fg outline-none placeholder:text-cx-fg-4"
        />
        {loading ? <Spinner size={12} className="text-cx-fg-4" /> : null}
      </div>
      {error ? <div className="border-b border-cx-border-subtle px-3 py-2 text-cx-xs text-cx-danger">{error}</div> : null}
      {occupiedHint ? <div className="border-b border-cx-border-subtle px-3 py-2 text-cx-xs text-cx-fg-3">{occupiedHint}</div> : null}
      {status.branch_switch_blockers?.length ? (
        <div className="cx-scroll max-h-32 overflow-y-auto border-b border-cx-border-subtle px-3 py-2 text-cx-xs">
          <p className="mb-1 text-cx-fg-3">正在使用此目录的对话</p>
          {status.branch_switch_blockers.map((thread) => (
            <Link key={thread.thread_id} href={`/chat/${encodeURIComponent(thread.thread_id)}`} className="block truncate rounded py-1 text-cx-fg hover:underline" onClick={close}>
              {thread.title}
            </Link>
          ))}
        </div>
      ) : null}
      <OptionList
        id={listId}
        options={options}
        selected={status.current_branch || null}
        onSelect={choose}
        activeIndex={keyboard.activeIndex}
        onActiveIndexChange={keyboard.setActiveIndex}
        emptyText={status.branches.length ? "没有匹配的分支" : "还没有本地分支"}
        className="max-h-64"
      />
    </Popover>
  );
}
