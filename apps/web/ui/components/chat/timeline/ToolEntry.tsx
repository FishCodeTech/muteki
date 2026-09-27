"use client";

import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Button, Spinner } from "@/components/chat/ui";
import { languageFromPath } from "@/lib/chatHighlighter";
import { FileDiff } from "@/components/agentui/agents/file-diff";
import { ToolResult, ToolResultOutput, type ToolResultStatus } from "@/components/agentui/agents/tool-result";
import { ActivityItem } from "./ActivityItem";
import { LocalUrlChips } from "./LocalUrlChips";
import {
  formatToolDuration,
  toolArgSummary,
  toolEditLines,
  toolEditStat,
  toolKind,
  toolKindIcon,
  toolLabel,
  toolOutputText,
  type PresentableTool,
} from "./toolPresentation";

const MAX_OUTPUT_CHARS = 20_000;

const STATUS_LABEL: Record<PresentableTool["status"], string> = {
  pending: "等待执行",
  running: "正在执行",
  completed: "已完成",
  failed: "执行失败",
  cancelled: "已取消",
  declined: "已拒绝",
};

function resultStatus(status: PresentableTool["status"]): ToolResultStatus {
  switch (status) {
    case "pending":
    case "running":
      return "running";
    case "completed":
      return "success";
    case "failed":
      return "error";
    case "cancelled":
      return "cancelled";
    case "declined":
      return "declined";
    default: {
      const exhaustive: never = status;
      return exhaustive;
    }
  }
}

function StatusGlyph({ status }: { status: PresentableTool["status"] }) {
  switch (status) {
    case "running":
      return <Spinner size={12} className="text-cx-accent" />;
    case "completed":
      return <Icon name="check" size={13} className="text-cx-fg-3/55" />;
    case "failed":
      return <Icon name="x" size={13} className="text-cx-danger" />;
    case "cancelled":
      return <Icon name="minus" size={13} className="text-cx-fg-3/55" />;
    case "declined":
      return <Icon name="minus" size={13} className="text-cx-warning" />;
    case "pending":
      return <Icon name="circleDashed" size={12} className="text-cx-fg-3/55" />;
    default: {
      const exhaustive: never = status;
      return exhaustive;
    }
  }
}

function FooterAction({ label, onClick }: { label: string; onClick: () => void }) {
  return (
    <Button size="xs" variant="ghost" onClick={onClick} className="text-cx-fg-3">
      {label}
    </Button>
  );
}

export function ToolEntry({
  tool,
  active = false,
  threadId,
  onSelect,
  onOpenDiff,
}: {
  tool: PresentableTool;
  active?: boolean;
  threadId?: string;
  onSelect?: () => void;
  /** Edit tools: jump to this turn's changes, optionally focused on a file. */
  onOpenDiff?: (filePath?: string) => void;
}) {
  const kind = toolKind(tool);
  const label = toolLabel(tool, kind);
  const summary = toolArgSummary(tool, kind);
  const duration = formatToolDuration(tool.durationMs);
  const running = tool.status === "running" || tool.status === "pending";
  const failed = tool.status === "failed";
  const urlChips = kind === "command" || kind === "browser" ? (
    <LocalUrlChips
      threadId={threadId}
      text={`${tool.outputSummary || ""}`}
      source="tool"
      disabled={tool.status === "running"}
      className={cn("mb-1.5", kind === "command" && "pl-6")}
    />
  ) : null;
  const details = onSelect ? <FooterAction label="查看详情" onClick={onSelect} /> : null;

  if (kind === "command") {
    const declined = tool.status === "declined";
    const raw = toolOutputText(tool)
      || ((failed || declined || tool.status === "cancelled") && tool.error ? String(tool.error) : "");
    const output = raw.length > MAX_OUTPUT_CHARS ? `${raw.slice(0, MAX_OUTPUT_CHARS)}\n…` : raw;
    return (
      <div className="min-w-0 px-1.5" data-testid="cx-tool-entry" data-status={tool.status} data-tool-kind={kind}>
        <ToolResult
          kind="terminal"
          icon={<Icon name={toolKindIcon(kind)} size={15} className={cn(failed ? "text-cx-danger" : "text-cx-fg-3/70")} />}
          title={label}
          tool={summary}
          meta={duration || undefined}
          status={resultStatus(tool.status)}
          defaultOpen={running || failed}
          maxHeight={280}
          copyText={output || undefined}
          actions={details}
          className={cn(active && "rounded-lg bg-cx-selected")}
        >
          {output ? (
            <ToolResultOutput language="text" className="text-[11.5px] leading-[1.6]">{output}</ToolResultOutput>
          ) : (
            <span className="font-cx-mono text-[11.5px] text-cx-fg-3/60">{
              running ? "等待输出…" : declined ? "未执行" : "没有输出"
            }</span>
          )}
        </ToolResult>
        {urlChips}
      </div>
    );
  }

  const editLines = kind === "edit" && !failed ? toolEditLines(tool, kind) : null;
  if (editLines) {
    const editStat = toolEditStat(tool, kind);
    const path = editStat?.paths[0];
    return (
      <div className="min-w-0 px-1.5" data-testid="cx-tool-entry" data-status={tool.status} data-tool-kind={kind}>
        <FileDiff
          label={label}
          file={summary || path || tool.name}
          lines={editLines}
          status={running ? "streaming" : "complete"}
          defaultOpen={running}
          language={languageFromPath(path) ?? "text"}
          maxHeight={280}
          copyText={editLines.map((line) => `${line.type === "added" ? "+" : line.type === "removed" ? "-" : " "}${line.content}`).join("\n")}
          actions={
            <>
              {onOpenDiff && !running ? <FooterAction label="在变更中查看" onClick={() => onOpenDiff(path)} /> : null}
              {details}
            </>
          }
          className={cn(active && "rounded-lg bg-cx-selected")}
        />
      </div>
    );
  }

  const editStat = toolEditStat(tool, kind);
  return (
    <ActivityItem
      icon={toolKindIcon(kind)}
      tone={failed ? "danger" : running ? "running" : "default"}
      testId="cx-tool-entry"
    >
      <div className="group/tool flex min-w-0 items-center gap-1" data-status={tool.status} data-tool-kind={kind}>
        <button
          type="button"
          onClick={onSelect}
          aria-label={`${label}${summary ? ` ${summary}` : ""}，${STATUS_LABEL[tool.status]}，查看详情`}
          className={cn(
            "cx-press -ml-1.5 flex h-8 min-w-0 flex-1 items-center gap-2.5 rounded-lg px-1.5 text-left",
            active ? "bg-cx-selected" : "hover:bg-cx-hover",
          )}
        >
          <span className={cn("shrink-0 text-[13px] font-medium", failed ? "text-cx-danger" : "text-cx-fg/90")}>{label}</span>
          {summary ? (
            <span className="min-w-0 truncate rounded-lg bg-cx-hover/80 px-2 py-0.5 font-cx-mono text-[11.5px] text-cx-fg-3">{summary}</span>
          ) : null}
          <span className="flex-1" />
          {duration ? <span className="cx-tabular shrink-0 text-[11.5px] text-cx-fg-3/55">{duration}</span> : null}
          <span className="grid size-4 shrink-0 place-items-center">
            <StatusGlyph status={tool.status} />
          </span>
        </button>
        {kind === "edit" && onOpenDiff && !running ? (
          <Button
            size="xs"
            variant="ghost"
            onClick={() => onOpenDiff(editStat?.paths[0])}
            className="shrink-0 text-cx-fg-3 opacity-0 transition-opacity group-hover/tool:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100"
          >
            在变更中查看
          </Button>
        ) : null}
      </div>
      {failed && tool.error ? (
        <p className="mb-1 line-clamp-2 break-words font-cx-mono text-[11.5px] leading-5 text-cx-danger">{String(tool.error)}</p>
      ) : null}
      {urlChips}
    </ActivityItem>
  );
}
