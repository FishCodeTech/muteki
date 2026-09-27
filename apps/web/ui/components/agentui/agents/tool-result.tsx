"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import {
  Ban,
  Braces,
  Check,
  ChevronDown,
  CircleCheck,
  CircleX,
  Copy,
  LoaderCircle,
  RotateCcw,
  SquareTerminal,
  Wrench,
} from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import {
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import {
  AgentCode,
  type AgentCodeLanguage,
} from "@/components/agentui/agents/agent-code";
import { ActionSwapRollText } from "@/components/agentui/motion/action-swap-roll";
import { AgentDisclosure } from "@/components/agentui/agents/agent-disclosure";
import { SPRING_PRESS, SPRING_SWAP } from "@/components/agentui/lib/ease";
import { cn } from "@/lib/cn";

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
  /** Extra footer controls rendered next to copy / retry. */
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

function getSwapKey(value: ReactNode, fallback: string) {
  return typeof value === "string" || typeof value === "number"
    ? String(value)
    : fallback;
}

function getStatusClass(status: ToolResultStatus) {
  if (status === "running") {
    return "text-cx-accent ";
  }
  // Success is the common case; keep it quiet so failures stand out.
  if (status === "success") {
    return "text-cx-fg-3/55";
  }
  if (status === "error") {
    return "text-cx-danger ";
  }
  if (status === "declined") {
    return "text-cx-warning";
  }
  return "text-cx-fg-3";
}

function KindIcon({ kind }: { kind: ToolResultKind }) {
  if (kind === "terminal") return <SquareTerminal className="size-4" />;
  if (kind === "request") return <Braces className="size-4" />;
  return <Wrench className="size-4" />;
}

function StatusIcon({
  status,
  reduce,
}: {
  status: ToolResultStatus;
  reduce: boolean;
}) {
  if (status === "running") {
    return <LoaderCircle className={cn("size-3", !reduce && "animate-spin")} />;
  }
  if (status === "success") return <CircleCheck className="size-3" />;
  if (status === "error") return <CircleX className="size-3" />;
  return <Ban className="size-3" />;
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
  const reduce = useReducedMotion() ?? false;

  return (
    <motion.button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      whileTap={reduce ? undefined : { scale: 0.9 }}
      transition={SPRING_PRESS}
      className="grid size-7 place-items-center rounded-md text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:ring-2 focus-visible:ring-cx-focus"
    >
      {children}
    </motion.button>
  );
}

export function ToolResultOutput({
  children,
  language = "bash",
  className,
}: ToolResultOutputProps) {
  return (
    <AgentCode
      code={children}
      language={language}
      className={cn(
        "whitespace-pre-wrap break-words text-cx-fg/80",
        className,
      )}
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
  const reduce = useReducedMotion() ?? false;
  const baseId = useId();
  const triggerId = `${baseId}-trigger`;
  const contentId = `${baseId}-content`;
  const viewportRef = useRef<HTMLDivElement>(null);
  const previousStatus = useRef(status);
  const copyTimer = useRef<number | undefined>(undefined);
  const [copied, setCopied] = useState(false);
  const [internalOpen, setInternalOpen] = useState(defaultOpen);
  const currentOpen = open ?? internalOpen;
  const running = status === "running";
  const canCopy = Boolean(copyText || onCopy);
  const titleKey = getSwapKey(title, status);
  const metaKey = getSwapKey(meta, `${status}-meta`);
  const toolKey = getSwapKey(tool, `${status}-tool`);
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
    if (
      previousStatus.current === "running" &&
      status !== "running" &&
      collapseOnComplete
    ) {
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
    if (!viewport || !currentOpen || !running) return;

    const frame = requestAnimationFrame(() => {
      if (typeof viewport.scrollTo === "function") {
        viewport.scrollTo({
          top: viewport.scrollHeight,
          behavior: reduce ? "auto" : "smooth",
        });
      } else {
        viewport.scrollTop = viewport.scrollHeight;
      }
    });
    return () => cancelAnimationFrame(frame);
  });

  const handleCopy = useCallback(async () => {
    if (onCopy) await onCopy();
    else if (copyText) await navigator.clipboard?.writeText(copyText);

    setCopied(true);
    if (copyTimer.current) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1600);
  }, [copyText, onCopy]);

  return (
    <div
      data-state={status}
      aria-busy={running}
      className={cn("w-full text-sm", className)}
    >
      <button
        id={triggerId}
        type="button"
        aria-expanded={currentOpen}
        aria-controls={contentId}
        onClick={() => setOpen(!currentOpen)}
        className="group flex min-h-9 w-full items-center gap-2 rounded-md py-1 text-left outline-none focus-visible:ring-2 focus-visible:ring-cx-focus focus-visible:ring-offset-2 focus-visible:ring-offset-cx-bg"
      >
        <span
          aria-hidden="true"
          className="grid size-4 shrink-0 place-items-center text-cx-fg-3"
        >
          {icon ?? <KindIcon kind={kind} />}
        </span>
        <span className="flex min-w-0 flex-1 items-baseline gap-2">
          <span className="max-w-[45%] shrink-0 truncate font-medium text-cx-fg/90">
            <ActionSwapRollText value={titleKey}>
              {title}
            </ActionSwapRollText>
          </span>
          {meta ? (
            <span className="shrink-0 text-xs text-cx-fg-3/60">
              <ActionSwapRollText value={metaKey}>
                {meta}
              </ActionSwapRollText>
            </span>
          ) : null}
          <span className="min-w-0 truncate font-cx-mono text-[11px] text-cx-fg-3/55">
            <ActionSwapRollText value={toolKey}>
              {tool}
            </ActionSwapRollText>
          </span>
        </span>
        <span
          className={cn(
            "inline-flex shrink-0 items-center gap-1 text-[11px] font-medium",
            getStatusClass(status),
          )}
        >
          <StatusIcon status={status} reduce={reduce} />
          <ActionSwapRollText value={status}>{statusLabel}</ActionSwapRollText>
        </span>
        <motion.span
          aria-hidden="true"
          animate={{ rotate: currentOpen ? 180 : 0 }}
          transition={reduce ? { duration: 0 } : SPRING_SWAP}
          className="shrink-0 text-cx-fg-3/50 transition-colors group-hover:text-cx-fg-3"
        >
          <ChevronDown className="size-3.5" />
        </motion.span>
      </button>

      <AgentDisclosure
        id={contentId}
        role="region"
        aria-labelledby={triggerId}
        open={currentOpen}
      >
        <div className="pl-6 pt-1.5">
          <div className="overflow-hidden rounded-xl bg-cx-hover/80">
          <div
            ref={viewportRef}
            role="log"
            aria-live="polite"
            className="scrollbar-hide overflow-y-auto"
            style={{ maxHeight }}
          >
            <div className={cn("p-3", contentClassName)}>{children}</div>
          </div>

            {canCopy || onRetry || actions ? (
              <div className="flex items-center gap-0.5 px-2 pb-1.5">
              {canCopy ? (
                <ToolResultAction
                  label={copied ? "已复制" : "复制输出"}
                  onClick={handleCopy}
                >
                  {copied ? (
                    <Check className="size-3.5" />
                  ) : (
                    <Copy className="size-3.5" />
                  )}
                </ToolResultAction>
              ) : null}
              {onRetry ? (
                <ToolResultAction label="重新运行" onClick={onRetry}>
                  <RotateCcw className="size-3.5" />
                </ToolResultAction>
              ) : null}
              {actions}
              <span className="ml-auto text-[11px] text-cx-fg-3/55">
                <ActionSwapRollText value={status}>
                  {statusLabel}
                </ActionSwapRollText>
              </span>
              </div>
            ) : null}
          </div>
        </div>
      </AgentDisclosure>
    </div>
  );
}
