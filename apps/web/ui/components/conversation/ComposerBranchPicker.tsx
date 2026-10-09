"use client";

import { useCallback, useId, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { OptionList, Popover, Spinner, Tooltip, useListKeyboard, type ListOption } from "@/components/chat/ui";
import { stripPillClass } from "@/components/chat/composer/stripPill";
import { checkoutSharedBranch, useSharedGitStatus } from "@/lib/threadGitStatusStore";
import { Icon } from "../Icon";

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
  const loading = shared.loading;
  const error = actionError || shared.error;
  const refreshStatus = shared.refresh;

  const loadStatus = useCallback(async () => {
    setActionError("");
    await refreshStatus();
  }, [refreshStatus]);

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
    disabled || status?.branch_switch_blocked || (status?.occupied && Boolean(threadId)),
  );

  const close = () => {
    setOpen(false);
    setQuery("");
  };

  const switchBranch = async (branch: string, create = false) => {
    if ((!threadId && !projectId) || busy || switchBlocked) return;
    setBusy(true);
    setActionError("");
    try {
      await checkoutSharedBranch(threadId, projectId, branch, { create });
      close();
    } catch (err) {
      setActionError(err instanceof Error ? err.message : "切换分支失败");
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
      trailing: branch === status?.current_branch ? <span className="text-[12px] text-cx-fg-4">当前</span> : undefined,
    }));
    if (canCreate) {
      rows.push({ value: `\u0000create:${trimmedQuery}`, label: `创建并切换到 ${trimmedQuery}`, textValue: trimmedQuery, icon: "plus" });
    }
    return rows;
  }, [canCreate, filteredBranches, status?.current_branch, trimmedQuery]);
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
  const occupiedHint = switchBlocked ? "被其他会话占用，已锁定分支切换" : "";
  const trigger = (
    <button
      type="button"
      disabled={switchBlocked || busy}
      className={cn("max-w-[240px]", stripPillClass(true), className)}
      aria-label={`Git 分支：${label}`}
    >
      {busy ? <Spinner size={12} /> : <Icon name="gitBranch" size={13} />}
      <span className="min-w-0 truncate font-cx-mono text-[12px]">{label}</span>
      {status.dirty ? <span className="size-1.5 shrink-0 rounded-full bg-cx-warning" aria-label="有未提交改动" /> : null}
      {!switchBlocked ? <Icon name="chevronDown" size={12} className="text-cx-fg-4" /> : null}
    </button>
  );

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        if (switchBlocked && next) return;
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
          className="h-10 min-w-0 flex-1 bg-transparent text-[13px] text-cx-fg outline-none placeholder:text-cx-fg-4"
        />
        {loading ? <Spinner size={12} className="text-cx-fg-4" /> : null}
      </div>
      {error ? <div className="border-b border-cx-border-subtle px-3 py-2 text-[12px] text-cx-danger">{error}</div> : null}
      {occupiedHint ? <div className="border-b border-cx-border-subtle px-3 py-2 text-[12px] text-cx-fg-3">{occupiedHint}</div> : null}
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
