"use client";

import {
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { cn } from "../utils/cn";
import { copyToClipboard } from "../utils/clipboard";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon } from "../icons/Icon";
import { ActionSwapIcon, ActionSwapRollText } from "../primitives/ActionSwap";
import { AgentCode, type AgentCodeLanguage } from "./CodeBlock";
import { AgentDisclosure } from "./AgentDisclosure";

export interface ToolItem {
  id: string;
  name: string;
  title?: string;
  kind?: string;
  outputUnavailable?: boolean;
  status: "pending" | "running" | "completed" | "failed" | "cancelled" | "declined";
  durationMs?: number;
  chip?: string;
  mono?: boolean;
  argsSummary?: string;
  outputSummary?: string;
  error?: string;
  icon?: "think" | "write" | "run" | "read" | "search" | string;
  detailLines?: Array<{ text: string; tone?: "add" | "del" | "info" }>;
}

export type ToolResultStatus = "running" | "success" | "error" | "cancelled" | "declined";
export type ToolResultKind = "terminal" | "request" | "custom";

export interface ToolResultProps {
  tool: ReactNode;
  title: ReactNode;
  children: ReactNode;
  status?: ToolResultStatus;
  kind?: ToolResultKind;
  meta?: ReactNode;
  icon?: ReactNode;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  collapseOnComplete?: boolean;
  maxHeight?: number;
  copyText?: string;
  onCopy?: () => void | Promise<void>;
  onRetry?: () => void;
  actions?: ReactNode;
  className?: string;
  contentClassName?: string;
}

export interface ToolResultOutputProps {
  children: string;
  language?: AgentCodeLanguage;
  className?: string;
}

function getStatusLabel(status: ToolResultStatus) {
  if (status === "running") return "执行中";
  if (status === "success") return "已完成";
  if (status === "error") return "失败";
  if (status === "declined") return "已拒绝";
  return "已取消";
}

function getStatusClass(status: ToolResultStatus) {
  if (status === "running") return "text-cx-accent";
  if (status === "success") return "text-cx-fg-3/65";
  if (status === "error") return "text-cx-danger";
  if (status === "declined") return "text-cx-warning";
  return "text-cx-fg-3";
}

function KindIcon({ kind }: { kind: ToolResultKind }) {
  if (kind === "terminal") return <Icon name="terminal" size={15} />;
  if (kind === "request") return <Icon name="braces" size={15} />;
  return <Icon name="wrench" size={15} />;
}

function StatusIcon({ status }: { status: ToolResultStatus }) {
  if (status === "running") return <Icon name="loader" size={12} className="cx-spin" />;
  if (status === "success") return <Icon name="checkCircle" size={12} />;
  if (status === "error") return <Icon name="xCircle" size={12} />;
  return <Icon name="circleAlert" size={12} />;
}

function ToolResultAction({
  label,
  onClick,
  children,
}: {
  label: string;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className="cx-press grid size-7 place-items-center rounded-[6px] text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
    >
      {children}
    </button>
  );
}

export function ToolResultOutput({ children, language = "bash", className }: ToolResultOutputProps) {
  return (
    <AgentCode
      code={children}
      language={language}
      className={cn("whitespace-pre-wrap break-words text-cx-fg/80", className)}
    />
  );
}

export function ToolResult({
  tool,
  title,
  children,
  status = "running",
  kind = "custom",
  meta,
  icon,
  open,
  defaultOpen = true,
  onOpenChange,
  collapseOnComplete = true,
  maxHeight = 220,
  copyText,
  onCopy,
  onRetry,
  actions,
  className,
  contentClassName,
}: ToolResultProps) {
  const reduced = useReducedMotion();
  const baseId = useId();
  const triggerId = `${baseId}-trigger`;
  const contentId = `${baseId}-content`;
  const viewportRef = useRef<HTMLDivElement>(null);
  const previousStatus = useRef(status);
  const wasStreamingRef = useRef(false);
  const copyTimer = useRef<number | undefined>(undefined);
  const [copied, setCopied] = useState(false);
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const currentOpen = open ?? internalOpen;
  const running = status === "running";
  const canCopy = Boolean(copyText || onCopy);
  const statusLabel = getStatusLabel(status);

  const setOpen = useCallback(
    (next: boolean) => {
      if (open === undefined) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [onOpenChange, open],
  );

  useEffect(() => {
    if (previousStatus.current !== "running" && status === "running") {
      setOpen(true);
    }
    if (previousStatus.current === "running" && status !== "running" && collapseOnComplete) {
      setOpen(false);
    }
    previousStatus.current = status;
  }, [collapseOnComplete, setOpen, status]);

  useEffect(
    () => () => {
      if (copyTimer.current) window.clearTimeout(copyTimer.current);
    },
    [],
  );

  useLayoutEffect(() => {
    const viewport = viewportRef.current;
    if (!viewport || !currentOpen || !running) {
      wasStreamingRef.current = false;
      return;
    }
    const frame = requestAnimationFrame(() => {
      const shouldSmooth = !wasStreamingRef.current && !reduced;
      wasStreamingRef.current = true;
      if (viewport.scrollHeight <= viewport.clientHeight) return;
      if (shouldSmooth && typeof viewport.scrollTo === "function") {
        viewport.scrollTo({ top: viewport.scrollHeight, behavior: "smooth" });
      } else {
        viewport.scrollTop = viewport.scrollHeight;
      }
    });
    return () => cancelAnimationFrame(frame);
  });

  const handleCopy = useCallback(async () => {
    if (onCopy) await onCopy();
    else if (copyText && !(await copyToClipboard(copyText))) return;

    setCopied(true);
    if (copyTimer.current) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1600);
  }, [copyText, onCopy]);

  return (
    <div
      data-slot="tool-result"
      data-state={status}
      aria-busy={running || undefined}
      className={cn("w-full text-cx-sm", className)}
    >
      <button
        id={triggerId}
        type="button"
        aria-expanded={currentOpen}
        aria-controls={contentId}
        onClick={() => setOpen(!currentOpen)}
        className="group flex min-h-9 w-full items-center gap-2 rounded-[8px] px-1 py-1 text-left outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]"
      >
        <span aria-hidden="true" className="grid size-4 shrink-0 place-items-center text-cx-fg-3">
          {icon ?? <KindIcon kind={kind} />}
        </span>
        <span className="flex min-w-0 flex-1 items-baseline gap-2">
          <span className="max-w-[45%] shrink-0 truncate font-medium text-cx-fg/90">{title}</span>
          {meta ? <span className="shrink-0 text-cx-xs text-cx-fg-3/65">{meta}</span> : null}
          <span className="min-w-0 truncate font-cx-mono text-cx-xs text-cx-fg-3/55">{tool}</span>
        </span>
        <span className={cn("inline-flex shrink-0 items-center gap-1 text-cx-xs font-medium", getStatusClass(status))}>
          <ActionSwapIcon value={status}>
            <StatusIcon status={status} />
          </ActionSwapIcon>
          <ActionSwapRollText value={statusLabel}>{statusLabel}</ActionSwapRollText>
        </span>
        <Icon
          name="chevronDown"
          size={14}
          className={cn(
            "shrink-0 text-cx-fg-4 transition-[transform,color] duration-150 ease-cx-out group-hover:text-cx-fg-2",
            currentOpen && "rotate-180",
          )}
        />
      </button>

      <AgentDisclosure open={currentOpen}>
        <div id={contentId} role="region" aria-labelledby={triggerId} className="pl-6 pt-1.5">
          <div className="overflow-hidden rounded-[10px] border border-cx-border bg-cx-elevated shadow-[var(--ui-sheen)]">
            <div
              ref={viewportRef}
              role="log"
              aria-live="polite"
              className="cx-scroll overflow-y-auto"
              style={{ maxHeight }}
            >
              <div className={cn("p-3", contentClassName)}>{children}</div>
            </div>

            {canCopy || onRetry || actions ? (
              <div className="flex items-center gap-0.5 border-t border-cx-border-subtle px-2 py-1">
                {canCopy ? (
                  <ToolResultAction label={copied ? "已复制" : "复制输出"} onClick={() => void handleCopy()}>
                    <ActionSwapIcon value={copied ? "check" : "copy"}>
                      <Icon name={copied ? "check" : "copy"} size={14} className={copied ? "text-cx-success" : undefined} />
                    </ActionSwapIcon>
                  </ToolResultAction>
                ) : null}
                {onRetry ? (
                  <ToolResultAction label="重新运行" onClick={onRetry}>
                    <Icon name="retry" size={14} />
                  </ToolResultAction>
                ) : null}
                {actions}
                <span className="ml-auto text-cx-xs text-cx-fg-4">
                  <ActionSwapRollText value={statusLabel}>{statusLabel}</ActionSwapRollText>
                </span>
              </div>
            ) : null}
          </div>
        </div>
      </AgentDisclosure>
    </div>
  );
}
