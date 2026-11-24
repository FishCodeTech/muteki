"use client";

import {
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type ReactNode,
} from "react";
import { cn } from "../utils/cn";
import { Icon } from "../icons/Icon";
import { Button } from "../primitives/Button";
import { AgentDisclosure } from "./AgentDisclosure";
import { Callout } from "../primitives/Feedback";
import { Shortcut } from "../primitives/Kbd";
import { languageFromPath } from "../engines/shiki";
import { AgentCode, CopyButton, type AgentCodeLanguage } from "./CodeBlock";
import { FileDiff, type FileDiffLine } from "./FileDiff";

export type ToolApprovalStatus =
  | "pending"
  | "approving"
  | "approved"
  | "denied"
  | "running"
  | "complete"
  | "error"
  | "expired";

export interface ToolApprovalParameter {
  id: string;
  label: ReactNode;
  value: ReactNode;
}

export interface ToolApprovalCodeProps {
  code: string;
  language?: AgentCodeLanguage;
  className?: string;
}

export interface ToolApprovalProps {
  tool: ReactNode;
  title?: ReactNode;
  description?: ReactNode;
  parameters?: ToolApprovalParameter[];
  status?: ToolApprovalStatus;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  onApprove?: () => void;
  onAlwaysAllow?: () => void;
  onDeny?: () => void;
  children?: ReactNode;
  approveLabel?: ReactNode;
  alwaysAllowLabel?: ReactNode;
  denyLabel?: ReactNode;
  disabled?: boolean;
  footer?: ReactNode;
  actions?: ReactNode;
  actionsAside?: ReactNode;
  className?: string;
}

function getStatusCopy(status: ToolApprovalStatus) {
  if (status === "approving") return "决定投递中";
  if (status === "approved") return "已批准";
  if (status === "denied") return "已拒绝";
  if (status === "running") return "执行中";
  if (status === "complete") return "已完成";
  if (status === "error") return "失败";
  if (status === "expired") return "已过期";
  return "等待审批";
}

function getStatusBadgeClass(status: ToolApprovalStatus) {
  if (status === "pending") return "border-cx-warning/30 bg-cx-warning-soft text-cx-warning";
  if (status === "approving" || status === "running") return "border-cx-accent/30 bg-cx-accent-soft text-cx-accent";
  if (status === "approved" || status === "complete") return "border-cx-success/30 bg-cx-success-soft text-cx-success";
  if (status === "expired") return "border-cx-border bg-cx-hover text-cx-fg-3";
  return "border-cx-danger/30 bg-cx-danger-soft text-cx-danger";
}

export function ToolApprovalCode({ code, language = "bash", className }: ToolApprovalCodeProps) {
  return (
    <AgentCode
      code={code}
      language={language}
      className={cn(
        "whitespace-pre-wrap break-words rounded-[8px] border border-cx-border/50 bg-cx-hover/30 px-2.5 py-2",
        className,
      )}
    />
  );
}

export function ToolApproval({
  tool,
  title = "允许执行这个工具吗？",
  description,
  parameters = [],
  status = "pending",
  open,
  defaultOpen = false,
  onOpenChange,
  onApprove,
  onAlwaysAllow,
  onDeny,
  children,
  approveLabel = "允许一次",
  alwaysAllowLabel = "始终允许",
  denyLabel = "拒绝",
  disabled = false,
  footer,
  actions,
  actionsAside,
  className,
}: ToolApprovalProps) {
  const baseId = useId();
  const detailsId = `${baseId}-details`;
  const previousStatus = useRef(status);
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const currentOpen = open ?? internalOpen;
  const setOpen = useCallback(
    (next: boolean) => {
      if (open === undefined) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [onOpenChange, open],
  );
  const busy = status === "approving" || status === "running";
  const pending = status === "pending";
  const error = status === "error";

  useEffect(() => {
    if (previousStatus.current === "pending" && status !== "pending") {
      setOpen(false);
    }
    previousStatus.current = status;
  }, [setOpen, status]);

  return (
    <div
      data-slot="tool-approval"
      data-state={status}
      aria-busy={busy || undefined}
      className={cn(
        "ui-surface-ring w-full overflow-hidden rounded-[12px] bg-cx-elevated text-cx-sm",
        className,
      )}
    >
      <div className="flex items-start gap-3 p-4">
        <span
          aria-hidden="true"
          className={cn(
            "mt-0.5 grid size-8 shrink-0 place-items-center rounded-[10px] border border-cx-border bg-cx-bg text-cx-fg-3",
            error && "text-cx-danger",
          )}
        >
          {busy ? (
            <Icon name="loader" size={16} className="cx-spin" />
          ) : error ? (
            <Icon name="circleAlert" size={16} />
          ) : status === "expired" ? (
            <Icon name="clock" size={16} />
          ) : status === "denied" ? (
            <Icon name="x" size={16} />
          ) : status === "approved" || status === "complete" ? (
            <Icon name="check" size={16} />
          ) : (
            <Icon name="shield" size={16} />
          )}
        </span>

        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="font-medium text-cx-fg">{title}</div>
              <div className="mt-0.5 truncate font-cx-mono text-cx-xs text-cx-fg-3">{tool}</div>
            </div>
            <span
              className={cn(
                "shrink-0 rounded-full border px-2 py-0.5 text-cx-xs font-medium transition-colors",
                getStatusBadgeClass(status),
              )}
            >
              {getStatusCopy(status)}
            </span>
          </div>
          {description ? <p className="mt-2 leading-5 text-cx-fg-3">{description}</p> : null}
          {children ? <div className="mt-3 flex flex-col gap-3">{children}</div> : null}

          {parameters.length ? (
            <button
              type="button"
              aria-expanded={currentOpen}
              aria-controls={detailsId}
              onClick={() => setOpen(!currentOpen)}
              className="mt-2 inline-flex items-center gap-1 rounded-[6px] text-cx-xs font-medium text-cx-fg-3 outline-none transition-colors hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
            >
              查看详情
              <Icon
                name="chevronDown"
                size={14}
                className={cn("transition-transform duration-150 ease-cx-out", currentOpen && "rotate-180")}
              />
            </button>
          ) : null}
        </div>
      </div>

      <AgentDisclosure open={currentOpen}>
        <dl
          id={detailsId}
          className="mx-4 mb-4 grid gap-2 rounded-[10px] border border-cx-border bg-cx-bg-subtle p-3"
        >
          {parameters.map((parameter) => (
            <div key={parameter.id} className="grid grid-cols-[minmax(0,7rem)_minmax(0,1fr)] items-center gap-3 text-cx-xs">
              <dt className="text-cx-fg-3">{parameter.label}</dt>
              <dd className="min-w-0 break-words font-cx-mono text-cx-fg/85">{parameter.value}</dd>
            </div>
          ))}
        </dl>
      </AgentDisclosure>

      {pending ? (
        <div className="flex flex-wrap items-center gap-2 border-t border-cx-border-subtle px-4 py-3">
          {actions ?? (
            <>
              <Button
                variant="primary"
                size="sm"
                agentConfirm="hitl"
                disabled={disabled}
                onClick={onApprove}
              >
                {approveLabel}
              </Button>
              {onAlwaysAllow ? (
                <Button
                  variant="secondary"
                  size="sm"
                  agentConfirm="hitl"
                  disabled={disabled}
                  onClick={onAlwaysAllow}
                >
                  {alwaysAllowLabel}
                </Button>
              ) : null}
              {onDeny ? (
                <Button
                  variant="ghost"
                  size="sm"
                  agentConfirm="destructive"
                  disabled={disabled}
                  onClick={onDeny}
                >
                  {denyLabel}
                </Button>
              ) : null}
            </>
          )}
          {actionsAside}
        </div>
      ) : footer ? (
        <div className="border-t border-cx-border-subtle px-4 py-3 text-cx-xs text-cx-fg-3">{footer}</div>
      ) : null}
    </div>
  );
}

export type ApprovalCardFile = {
  path: string;
  status?: string;
  raw?: string;
  additions?: number;
  deletions?: number;
};

export interface ApprovalCardProps {
  title?: string;
  action?: string;
  args?: string;
  command?: string;
  cwd?: string;
  diff?: string;
  files?: ApprovalCardFile[];
  missingPaths?: boolean;
  missingDiff?: boolean;
  scope?: string;
  expires?: string;
  reason?: string;
  status?: string;
  approvalId?: string;
  busy?: boolean;
  onAllow?: (scopeMode?: "once" | "session") => void;
  onDeny?: () => void;
  nativeOptions?: Array<{ option_id: string; kind: string; name: string }>;
  onNativeOption?: (optionId: string, kind: string) => void;
  className?: string;
}

function isEditableEventTarget(target: EventTarget | null): boolean {
  if (!(target instanceof Element)) return false;
  if (target.closest("input, textarea, select, [contenteditable='true']")) return true;
  return false;
}

function parsePatchLines(raw: string): FileDiffLine[] {
  let oldNum = 0;
  let newNum = 0;
  return raw.split("\n").map((line, index) => {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line);
    if (hunk) {
      oldNum = parseInt(hunk[1], 10);
      newNum = parseInt(hunk[2], 10);
      return { id: `h-${index}`, type: "context", content: line };
    }
    if (line.startsWith("+") && !line.startsWith("+++")) {
      return { id: `a-${index}`, type: "added", newLine: newNum++, content: line.slice(1) };
    }
    if (line.startsWith("-") && !line.startsWith("---")) {
      return { id: `d-${index}`, type: "removed", oldLine: oldNum++, content: line.slice(1) };
    }
    const clean = line.startsWith(" ") ? line.slice(1) : line;
    return {
      id: `c-${index}`,
      type: "context",
      oldLine: oldNum ? oldNum++ : undefined,
      newLine: newNum ? newNum++ : undefined,
      content: clean,
    };
  });
}

function statusLabel(status?: string): string {
  if (!status) return "";
  const key = status.toLowerCase();
  if (["a", "add", "added", "create", "created"].includes(key)) return "新建";
  if (["d", "del", "delete", "deleted", "remove"].includes(key)) return "删除";
  if (["r", "rename", "renamed"].includes(key)) return "重命名";
  if (["m", "modify", "modified", "edit"].includes(key)) return "修改";
  return status;
}

export function ApprovalCard({
  title = "操作需要审批",
  action = "Runtime 操作",
  args,
  command,
  cwd,
  diff,
  files = [],
  missingPaths = false,
  missingDiff = false,
  scope,
  expires,
  reason,
  status = "pending",
  approvalId,
  busy = false,
  onAllow,
  onDeny,
  nativeOptions,
  onNativeOption,
  className,
}: ApprovalCardProps) {
  const expired = status === "expired";
  const resolving = status === "resolving";
  const approvalStatus: ToolApprovalStatus = expired ? "expired" : busy || resolving ? "approving" : "pending";

  const nativeActions = nativeOptions ? (
    <>
      {nativeOptions.length ? (
        nativeOptions.map((option) => {
          const known = ["allow_once", "allow_always", "reject_once", "reject_always"].includes(option.kind);
          const isAllow = option.kind === "allow_once" || option.kind === "allow_always";
          const label =
            option.name ||
            ({
              allow_once: "允许一次",
              allow_always: "记住此选择",
              reject_once: "拒绝一次",
              reject_always: "记住拒绝选择",
            } as Record<string, string>)[option.kind] ||
            option.kind;
          return (
            <Button
              key={option.option_id}
              size="sm"
              variant={option.kind === "allow_once" ? "primary" : isAllow ? "secondary" : "ghost"}
              agentConfirm={isAllow ? "hitl" : "destructive"}
              disabled={busy || resolving || !known || !onNativeOption}
              onClick={() => onNativeOption?.(option.option_id, option.kind)}
            >
              {label}
            </Button>
          );
        })
      ) : (
        <span className="text-cx-xs text-cx-fg-3">此请求未提供可用的决定选项。</span>
      )}
      {nativeOptions.some((option) => option.kind === "allow_always" || option.kind === "reject_always") ? (
        <span className="w-full text-cx-xs text-cx-fg-3">记住选择的范围由 Runtime 决定。</span>
      ) : null}
    </>
  ) : undefined;

  const decidable = status === "pending" && !busy && !resolving;
  const nativeOption = (kind: string) => nativeOptions?.find((option) => option.kind === kind);
  const keyActions: Record<string, (() => void) | undefined> =
    status === "pending"
      ? nativeOptions
        ? {
            y:
              nativeOption("allow_once") && onNativeOption
                ? () => onNativeOption(nativeOption("allow_once")!.option_id, "allow_once")
                : undefined,
            a:
              nativeOption("allow_always") && onNativeOption
                ? () => onNativeOption(nativeOption("allow_always")!.option_id, "allow_always")
                : undefined,
            n:
              nativeOption("reject_once") && onNativeOption
                ? () => onNativeOption(nativeOption("reject_once")!.option_id, "reject_once")
                : undefined,
          }
        : {
            y: onAllow ? () => onAllow("once") : undefined,
            n: onDeny,
          }
      : {};
  const hasKeyActions = Object.values(keyActions).some(Boolean);

  const handleKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!hasKeyActions || !decidable || event.nativeEvent.isComposing) return;
    if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return;
    if (isEditableEventTarget(event.target)) return;
    const onCard = event.target === event.currentTarget;
    const key = event.key.toLowerCase();
    const run = key === "enter" && onCard ? keyActions.y : keyActions[key];
    if (key === "escape" && onCard) {
      event.currentTarget.blur();
      return;
    }
    if (!run) return;
    event.preventDefault();
    event.stopPropagation();
    const card = event.currentTarget;
    const queue = Array.from(document.querySelectorAll<HTMLElement>('[data-approval-keyboard="true"]'));
    const next = queue[queue.indexOf(card) + 1] ?? queue.find((item) => item !== card);
    run();
    next?.focus();
  };

  const shortcutHints = [
    keyActions.y ? { keys: "y", label: "批准" } : null,
    keyActions.a ? { keys: "a", label: "记住" } : null,
    keyActions.n ? { keys: "n", label: "拒绝" } : null,
  ].filter((hint): hint is { keys: string; label: string } => Boolean(hint));

  const pathList = files.filter((f) => f.path);
  const diffFiles = useMemo(() => {
    const withRaw = files
      .filter((f) => f.path && f.raw && f.raw.trim())
      .map((f) => ({ path: f.path, raw: f.raw as string }));
    if (withRaw.length) return withRaw;
    if (diff && diff.trim()) return [{ path: files.find((f) => f.path)?.path || "change.diff", raw: diff }];
    return [];
  }, [files, diff]);
  const showMissingBanner = missingPaths || missingDiff;
  const diffFilePaths = new Set(diffFiles.map((f) => f.path));
  const noDiffFiles = pathList.filter((f) => !diffFilePaths.has(f.path));

  const parameters: ToolApprovalParameter[] = [];
  if (cwd) parameters.push({ id: "cwd", label: "工作目录", value: <code data-testid="approval-cwd" className="break-all">{cwd}</code> });
  if (scope) parameters.push({ id: "scope", label: "权限范围", value: scope });
  if (expires) parameters.push({ id: "expires", label: "有效期", value: expires });
  if (args && !command) {
    parameters.push({ id: "args", label: "执行参数", value: <ToolApprovalCode code={args} language="json" className="max-h-52 overflow-auto" /> });
  }

  return (
    <div
      data-slot="approval-card"
      className={cn(
        "group/approval relative flex w-full flex-col items-stretch rounded-[12px] outline-none",
        hasKeyActions && "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
        className,
      )}
      tabIndex={hasKeyActions ? 0 : undefined}
      onKeyDown={hasKeyActions ? handleKeyDown : undefined}
      aria-keyshortcuts={hasKeyActions ? shortcutHints.map((hint) => hint.keys.toUpperCase()).join(" ") : undefined}
      aria-label={hasKeyActions ? `${title}：${action}` : undefined}
      data-approval-keyboard={hasKeyActions ? "true" : undefined}
      data-approval-id={approvalId || undefined}
      data-approval-status={status}
      data-approval-missing-paths={missingPaths ? "true" : undefined}
      data-approval-missing-diff={missingDiff ? "true" : undefined}
    >
      <ToolApproval
        title={expired ? "审批已过期" : title}
        tool={
          <span className="flex min-w-0 flex-col gap-0.5">
            <span className="whitespace-normal break-words font-cx-sans text-cx-base font-semibold text-cx-fg">
              {action}
            </span>
          </span>
        }
        description={reason}
        status={approvalStatus}
        parameters={parameters}
        defaultOpen={Boolean(cwd)}
        disabled={busy || resolving}
        actions={nativeActions}
        approveLabel="批准执行"
        alwaysAllowLabel="记住此选择（由 Runtime 决定范围）"
        onApprove={onAllow ? () => onAllow("once") : undefined}
        onDeny={onDeny}
        footer={expired ? "旧批准不能作用于新请求；请处理仍有效的审批项。" : undefined}
        actionsAside={
          shortcutHints.length ? (
            <span
              className="ml-auto hidden items-center gap-2.5 text-cx-xs text-cx-fg-4 opacity-0 transition-opacity group-focus-within/approval:opacity-100 sm:inline-flex"
              data-testid="approval-shortcut-hints"
            >
              {shortcutHints.map((hint) => (
                <span key={hint.keys} className="inline-flex items-center gap-1">
                  <Shortcut keys={hint.keys} tone="subtle" />
                  {hint.label}
                </span>
              ))}
            </span>
          ) : undefined
        }
        className={cn(
          "bg-cx-elevated",
          expired ? "opacity-75" : "[--ui-surface-ring:color-mix(in_srgb,var(--amber)_38%,transparent)] shadow-cx-md",
        )}
      >
        {showMissingBanner ? (
          <div data-testid="approval-missing-preview">
            <Callout role="status" tone="warning" title="路径/补丁缺失">
              {missingPaths && missingDiff
                ? "审批载荷未提供目标文件路径与变更补丁，无法在批准前审阅具体修改。"
                : missingPaths
                  ? "审批载荷未提供目标文件路径，无法确认修改对象。"
                  : "审批载荷未提供变更补丁（Diff），无法在批准前审阅具体修改。"}
            </Callout>
          </div>
        ) : null}

        {command ? (
          <div className="overflow-hidden rounded-[10px] border border-cx-border bg-cx-hover/60">
            <div className="flex h-8 items-center gap-2 border-b border-cx-border-subtle pl-3 pr-1">
              <Icon name="terminal" size={13} className="text-cx-fg-3/70" />
              <span className="flex-1 text-cx-xs font-medium text-cx-fg-3">完整命令</span>
              <CopyButton text={command} label="复制命令" />
            </div>
            <ToolApprovalCode
              code={`$ ${command}`}
              language="bash"
              className="max-h-44 overflow-auto rounded-none border-0 bg-transparent px-3 py-2.5 text-cx-xs"
            />
          </div>
        ) : null}

        {noDiffFiles.length > 0 ? (
          <div data-testid="approval-path-list">
            <span className="mb-1.5 block text-cx-xs font-medium text-cx-fg-3">
              变更文件（{noDiffFiles.length}）
            </span>
            <ul className="m-0 flex list-none flex-col rounded-[10px] border border-cx-border bg-cx-bg-subtle p-1">
              {noDiffFiles.map((file) => (
                <li
                  key={file.path}
                  className="flex items-start gap-2 rounded-[6px] px-2 py-1.5 font-cx-mono text-cx-xs text-cx-fg"
                  data-approval-path={file.path}
                >
                  <Icon name="file" size={13} className="mt-[3px] shrink-0 text-cx-fg-4" />
                  <span className="min-w-0 flex-1 break-all">{file.path}</span>
                  {file.status ? (
                    <span className="shrink-0 rounded bg-cx-bg px-1.5 font-cx-sans text-cx-xs leading-5 text-cx-fg-3">
                      {statusLabel(file.status)}
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        {diffFiles.length > 0 ? (
          <div data-testid="approval-diff-preview" className="flex flex-col gap-1">
            <span className="block text-cx-xs font-medium text-cx-fg-3">
              变更预览{pathList.length > 1 ? `（${pathList.length} 个文件）` : ""}
            </span>
            <div data-testid="approval-path-list" className="flex flex-col">
              {diffFiles.map((file) => {
                const meta = pathList.find((row) => row.path === file.path);
                return (
                  <div key={file.path} data-approval-path={file.path}>
                    <FileDiff
                      label={meta?.status ? statusLabel(meta.status) : undefined}
                      file={file.path}
                      lines={parsePatchLines(file.raw)}
                      status="preview"
                      defaultOpen={diffFiles.length <= 3}
                      collapseOnComplete={false}
                      language={languageFromPath(file.path) ?? "text"}
                      maxHeight={260}
                      copyText={file.raw}
                    />
                  </div>
                );
              })}
            </div>
          </div>
        ) : null}
      </ToolApproval>
    </div>
  );
}
