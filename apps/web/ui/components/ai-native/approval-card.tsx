"use client";

/* ─────────────────────────────────────────────────────────
 * APPROVAL CARD — Human-in-the-loop approval & permission card.
 *
 * Built on AgentUI's ToolApproval (MIT, vendored in components/agentui).
 * C23: command/cwd preview, file Diff preview, expired binding by approval_id.
 * #123: path list + Diff from files[], explicit 路径/补丁缺失 empty state.
 * ───────────────────────────────────────────────────────── */

import { useMemo } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { Callout, CopyButton } from "@/components/chat/ui";
import { languageFromPath } from "@/lib/chatHighlighter";
import { linesFromPatch } from "@/components/chat/timeline/toolPresentation";
import { FileDiff } from "@/components/agentui/agents/file-diff";
import {
  ToolApproval,
  ToolApprovalCode,
  type ToolApprovalParameter,
  type ToolApprovalStatus,
} from "@/components/agentui/agents/tool-approval";

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
  /** Structured file list for file_change approvals (#123). */
  files?: ApprovalCardFile[];
  /** Show explicit missing-path banner (file_change with no paths). */
  missingPaths?: boolean;
  /** Show explicit missing-diff banner (file_change with no patch). */
  missingDiff?: boolean;
  scope?: string;
  expires?: string;
  reason?: string;
  status?: string;
  approvalId?: string;
  busy?: boolean;
  onAllow?: (scopeMode?: "once" | "session") => void;
  onDeny?: () => void;
  className?: string;
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
  className = "",
}: ApprovalCardProps) {
  const expired = status === "expired";
  const approvalStatus: ToolApprovalStatus = expired ? "expired" : busy ? "approving" : "pending";

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

  const parameters: ToolApprovalParameter[] = [];
  if (cwd) parameters.push({ id: "cwd", label: "工作目录", value: <code data-testid="approval-cwd" className="break-all">{cwd}</code> });
  if (scope) parameters.push({ id: "scope", label: "权限范围", value: scope });
  if (expires) parameters.push({ id: "expires", label: "有效期", value: expires });
  if (args && !command) {
    parameters.push({ id: "args", label: "执行参数", value: <ToolApprovalCode code={args} language="json" className="max-h-52 overflow-auto" /> });
  }

  return (
    <div
      className={cn("flex w-full flex-col items-stretch", className)}
      data-approval-id={approvalId || undefined}
      data-approval-status={status}
      data-approval-missing-paths={missingPaths ? "true" : undefined}
      data-approval-missing-diff={missingDiff ? "true" : undefined}
    >
      <ToolApproval
        title={expired ? "审批已过期" : title}
        tool={
          <span className="flex min-w-0 flex-col gap-0.5">
            <span className="whitespace-normal break-words font-cx-sans text-[13.5px] font-semibold text-cx-fg">{action}</span>
          </span>
        }
        description={reason}
        status={approvalStatus}
        parameters={parameters}
        defaultOpen={Boolean(cwd)}
        disabled={busy}
        approveLabel="批准执行"
        alwaysAllowLabel="本会话始终允许"
        onApprove={onAllow ? () => onAllow("once") : undefined}
        onAlwaysAllow={onAllow ? () => onAllow("session") : undefined}
        onDeny={onDeny}
        footer={expired ? "旧批准不能作用于新请求；请处理仍有效的审批项。" : undefined}
        className={cn(
          "bg-cx-elevated",
          expired ? "opacity-75" : "border-[color-mix(in_srgb,var(--amber)_38%,transparent)] shadow-cx-md",
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
          <div className="overflow-hidden rounded-xl bg-cx-hover/80">
            <div className="flex h-8 items-center gap-2 pl-3 pr-1">
              <Icon name="terminal" size={13} className="text-cx-fg-3/70" />
              <span className="flex-1 text-[12px] font-medium text-cx-fg-3">完整命令</span>
              <CopyButton text={command} label="复制命令" />
            </div>
            <ToolApprovalCode
              code={`$ ${command}`}
              language="bash"
              className="max-h-44 overflow-auto rounded-none border-0 border-t border-cx-fg/[0.06] bg-transparent px-3 py-2.5 text-[12px]"
            />
          </div>
        ) : null}

        {pathList.length > 0 && diffFiles.length === 0 ? (
          <div data-testid="approval-path-list">
            <span className="mb-1.5 block text-[12px] font-medium text-cx-fg-3">变更文件（{pathList.length}）</span>
            <ul className="m-0 flex list-none flex-col rounded-xl bg-cx-hover/60 p-1">
              {pathList.map((file) => (
                <li
                  key={file.path}
                  className="flex items-start gap-2 rounded-lg px-2 py-1.5 font-cx-mono text-[12px] text-cx-fg"
                  data-approval-path={file.path}
                >
                  <Icon name="file" size={13} className="mt-[3px] shrink-0 text-cx-fg-4" />
                  <span className="min-w-0 flex-1 break-all">{file.path}</span>
                  {file.status ? (
                    <span className="shrink-0 rounded-md bg-cx-bg/70 px-1.5 font-cx-sans text-[11px] leading-5 text-cx-fg-3">
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
            <span className="block text-[12px] font-medium text-cx-fg-3">
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
                      lines={linesFromPatch(file.raw)}
                      status="complete"
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
