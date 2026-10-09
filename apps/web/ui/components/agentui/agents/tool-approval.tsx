"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import {
  Check,
  ChevronDown,
  CircleAlert,
  Clock,
  LoaderCircle,
  ShieldCheck,
  X,
} from "lucide-react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  type ReactNode,
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
} from "react";
import {
  AgentCode,
  type AgentCodeLanguage,
} from "@/components/agentui/agents/agent-code";
import { AgentDisclosure } from "@/components/agentui/agents/agent-disclosure";
import { EASE_OUT, SPRING_PRESS, SPRING_SWAP } from "@/components/agentui/lib/ease";
import { cn } from "@/lib/cn";

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
  /** Always-visible body under the description (command, file list, diff). */
  children?: ReactNode;
  approveLabel?: ReactNode;
  alwaysAllowLabel?: ReactNode;
  denyLabel?: ReactNode;
  disabled?: boolean;
  /** Footer note shown instead of actions when the request is no longer pending. */
  footer?: ReactNode;
  actions?: ReactNode;
  /** Trailing content in the pending action row (e.g. keyboard hints). */
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
  if (status === "pending") {
    return "border-cx-warning/30 bg-cx-warning-soft text-cx-warning ";
  }
  if (status === "approving" || status === "running") {
    return "border-cx-accent/30 bg-cx-accent-soft text-cx-accent ";
  }
  if (status === "approved" || status === "complete") {
    return "border-cx-success/30 bg-cx-success-soft text-cx-success ";
  }
  if (status === "expired") {
    return "border-cx-border bg-cx-hover text-cx-fg-3";
  }
  return "border-cx-danger/30 bg-cx-danger-soft text-cx-danger ";
}

export function ToolApprovalCode({
  code,
  language = "bash",
  className,
}: ToolApprovalCodeProps) {
  return (
    <AgentCode
      code={code}
      language={language}
      className={cn(
        // Parameter values sit in a narrow grid column with nowhere to scroll
        // on touch, so they wrap instead of clipping (as ToolResultOutput does).
        "whitespace-pre-wrap break-words rounded-lg border border-cx-border/50 bg-cx-hover/30 px-2.5 py-2",
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
  const reduce = useReducedMotion() ?? false;
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
      data-state={status}
      aria-busy={busy}
      className={cn(
        "w-full overflow-hidden rounded-2xl border border-cx-border/60 bg-cx-hover/20 text-sm",
        className,
      )}
    >
      <div className="flex items-start gap-3 p-4">
        <span
          aria-hidden="true"
          className={cn(
            "mt-0.5 grid size-8 shrink-0 place-items-center rounded-xl border border-cx-border/60 bg-cx-bg text-cx-fg-3",
            error && "text-cx-danger",
          )}
        >
          {busy ? (
            <LoaderCircle className={cn("size-4", !reduce && "animate-spin")} />
          ) : error ? (
            <CircleAlert className="size-4" />
          ) : status === "expired" ? (
            <Clock className="size-4" />
          ) : status === "denied" ? (
            <X className="size-4" />
          ) : status === "approved" || status === "complete" ? (
            <Check className="size-4" />
          ) : (
            <ShieldCheck className="size-4" />
          )}
        </span>

        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="font-medium text-cx-fg">{title}</div>
              <div className="mt-0.5 truncate font-cx-mono text-xs text-cx-fg-3">
                {tool}
              </div>
            </div>
            <span
              className={cn(
                "shrink-0 rounded-full border px-2 py-0.5 text-[12px] font-medium transition-colors",
                getStatusBadgeClass(status),
              )}
            >
              {getStatusCopy(status)}
            </span>
          </div>
          {description ? (
            <p className="mt-2 leading-5 text-cx-fg-3">{description}</p>
          ) : null}
          {children ? <div className="mt-3 flex flex-col gap-3">{children}</div> : null}

          {parameters.length ? (
            <button
              type="button"
              aria-expanded={currentOpen}
              aria-controls={detailsId}
              onClick={() => setOpen(!currentOpen)}
              className="mt-2 inline-flex items-center gap-1 rounded-md text-xs font-medium text-cx-fg-3 outline-none transition-colors hover:text-cx-fg focus-visible:ring-2 focus-visible:ring-cx-focus"
            >
              查看详情
              <motion.span
                aria-hidden="true"
                animate={{ rotate: currentOpen ? 180 : 0 }}
                transition={reduce ? { duration: 0 } : SPRING_SWAP}
              >
                <ChevronDown className="size-3.5" />
              </motion.span>
            </button>
          ) : null}
        </div>
      </div>

      <AgentDisclosure
        id={detailsId}
        open={currentOpen}
      >
        <dl className="mx-4 mb-4 grid gap-2 rounded-xl border border-cx-border/50 bg-cx-bg/70 p-3">
          {parameters.map((parameter) => (
            <div
              key={parameter.id}
              className="grid grid-cols-[minmax(0,7rem)_minmax(0,1fr)] items-center gap-3 text-xs"
            >
              <dt className="text-cx-fg-3">{parameter.label}</dt>
              <dd className="min-w-0 break-words font-cx-mono text-cx-fg/85">
                {parameter.value}
              </dd>
            </div>
          ))}
        </dl>
      </AgentDisclosure>

      <AnimatePresence initial={false}>
        {pending ? (
          <motion.div
            initial={reduce ? { opacity: 0 } : { opacity: 0, y: 4 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0 }}
            transition={{ duration: reduce ? 0.12 : 0.22, ease: EASE_OUT }}
            className="flex flex-wrap items-center gap-2 border-t border-cx-border/60 px-4 py-3"
          >
            {actions ?? (<>
            <motion.button
              type="button"
              onClick={onApprove}
              disabled={disabled}
              whileTap={reduce || disabled ? undefined : { scale: 0.97 }}
              transition={SPRING_PRESS}
              className="rounded-xl bg-cx-fg px-3 py-1.5 text-xs font-medium text-cx-bg outline-none focus-visible:ring-2 focus-visible:ring-cx-focus focus-visible:ring-offset-2 disabled:opacity-50"
            >
              {approveLabel}
            </motion.button>
            {onAlwaysAllow ? (
              <motion.button
                type="button"
                onClick={onAlwaysAllow}
                disabled={disabled}
                whileTap={reduce || disabled ? undefined : { scale: 0.97 }}
                transition={SPRING_PRESS}
                className="rounded-xl border border-cx-border/60 bg-cx-bg px-3 py-1.5 text-xs font-medium text-cx-fg outline-none transition-colors hover:bg-cx-hover focus-visible:ring-2 focus-visible:ring-cx-focus disabled:opacity-50"
              >
                {alwaysAllowLabel}
              </motion.button>
            ) : null}
            {onDeny ? (
              <button
                type="button"
                onClick={onDeny}
                disabled={disabled}
                className="rounded-xl px-3 py-1.5 text-xs font-medium text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:ring-2 focus-visible:ring-cx-focus disabled:opacity-50"
              >
                {denyLabel}
              </button>
            ) : null}
            </>)}
            {actionsAside}
          </motion.div>
        ) : footer ? (
          <div className="border-t border-cx-border/60 px-4 py-3 text-xs text-cx-fg-3">{footer}</div>
        ) : null}
      </AnimatePresence>
    </div>
  );
}
