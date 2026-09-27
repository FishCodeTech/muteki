"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import {
  Check,
  ChevronDown,
  Copy,
  FileCode2,
  LoaderCircle,
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
  type AgentCodeLanguage,
  AgentCodeLine,
  useAgentCodeTokens,
} from "@/components/agentui/agents/agent-code";
import { AgentDisclosure } from "@/components/agentui/agents/agent-disclosure";
import { SPRING_PRESS, SPRING_SWAP } from "@/components/agentui/lib/ease";
import { cn } from "@/lib/cn";

export type FileDiffStatus = "streaming" | "complete";
export type FileDiffLineType = "added" | "removed" | "context";

export interface FileDiffLine {
  id: string;
  type?: FileDiffLineType;
  oldLine?: number;
  newLine?: number;
  content: string;
}

export interface FileDiffProps {
  file: ReactNode;
  /** Verb shown before the file name, e.g. "Edited". */
  label?: ReactNode;
  /** Extra footer controls rendered next to copy. */
  actions?: ReactNode;
  lines: FileDiffLine[];
  status?: FileDiffStatus;
  open?: boolean;
  defaultOpen?: boolean;
  onOpenChange?: (open: boolean) => void;
  collapseOnComplete?: boolean;
  maxHeight?: number;
  language?: AgentCodeLanguage;
  copyText?: string;
  onCopy?: () => void | Promise<void>;
  className?: string;
}

function ChangeCount({ value, type }: { value: number; type: "added" | "removed" }) {
  if (!value) return null;
  return (
    <span
      className={cn(
        "font-cx-mono text-xs tabular-nums",
        type === "added"
          ? "text-cx-success "
          : "text-cx-danger ",
      )}
    >
      {type === "added" ? "+" : "−"}
      {value}
    </span>
  );
}

export function FileDiff({
  file,
  label,
  actions,
  lines,
  status = "streaming",
  open,
  defaultOpen = true,
  onOpenChange,
  collapseOnComplete = true,
  maxHeight = 220,
  language = "typescript",
  copyText,
  onCopy,
  className,
}: FileDiffProps) {
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
  const streaming = status === "streaming";
  const additions = lines.filter((line) => line.type === "added").length;
  const deletions = lines.filter((line) => line.type === "removed").length;
  const canCopy = Boolean(copyText || onCopy);
  const code = lines.map((line) => line.content).join("\n");
  const tokens = useAgentCodeTokens(code, language);

  const setOpen = useCallback(
    (next: boolean) => {
      if (open === undefined) setInternalOpen(next);
      onOpenChange?.(next);
    },
    [onOpenChange, open],
  );

  useEffect(() => {
    if (previousStatus.current !== "streaming" && status === "streaming") {
      setOpen(true);
    }
    if (
      previousStatus.current === "streaming" &&
      status === "complete" &&
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
    if (!viewport || !currentOpen || !streaming) return;

    const frame = requestAnimationFrame(() => {
      if (viewport.scrollHeight <= viewport.clientHeight) return;
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
      aria-busy={streaming}
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
        <FileCode2
          aria-hidden="true"
          className="size-4 shrink-0 text-cx-fg-3"
        />
        {label ? (
          <span className="shrink-0 font-medium text-cx-fg/90">{label}</span>
        ) : null}
        <span className="min-w-0 flex-1 truncate font-cx-mono text-xs text-cx-fg/80">
          {file}
        </span>
        <span className="flex shrink-0 items-center gap-2">
          <ChangeCount value={additions} type="added" />
          <ChangeCount value={deletions} type="removed" />
        </span>
        <span className="grid size-4 shrink-0 place-items-center text-cx-fg-3/60">
          {streaming ? (
            <LoaderCircle
              aria-label="正在应用改动"
              className={cn("size-3.5", !reduce && "animate-spin")}
            />
          ) : (
            <Check aria-label="改动已应用" className="size-3.5" />
          )}
        </span>
        <motion.span
          aria-hidden="true"
          animate={{ rotate: currentOpen ? 180 : 0 }}
          transition={reduce ? { duration: 0 } : SPRING_SWAP}
          className="shrink-0 text-cx-fg-3/45 transition-colors group-hover:text-cx-fg-3"
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
              data-slot="file-diff-viewport"
              aria-live="polite"
              className="scrollbar-hide overflow-auto"
              style={{ maxHeight }}
            >
              <div className="font-cx-mono text-xs leading-5">
                <span className="sr-only">文件改动</span>
                {lines.map((line, index) => {
                  const type = line.type ?? "context";
                  return (
                    <div
                      key={line.id}
                      className={cn(
                        "grid grid-cols-[2.25rem_2.25rem_1rem_minmax(0,1fr)]",
                        type === "added" && "bg-cx-success/[0.07]",
                        type === "removed" && "bg-cx-danger/[0.07]",
                      )}
                    >
                      <span className="select-none pr-2 text-right tabular-nums text-cx-fg-3/40">
                        {line.oldLine}
                      </span>
                      <span className="select-none pr-2 text-right tabular-nums text-cx-fg-3/40">
                        {line.newLine}
                      </span>
                      <span
                        className={cn(
                          "select-none text-center text-cx-fg-3/45",
                          type === "added" &&
                            "text-cx-success ",
                          type === "removed" &&
                            "text-cx-danger ",
                        )}
                      >
                        {type === "added"
                          ? "+"
                          : type === "removed"
                            ? "−"
                            : ""}
                      </span>
                      <AgentCodeLine
                        code={line.content}
                        tokens={tokens?.[index]}
                        className="min-w-0 whitespace-pre px-1.5"
                      />
                    </div>
                  );
                })}
              </div>
            </div>

            {canCopy || actions ? (
              <div className="flex items-center justify-end gap-0.5 px-2 pb-1.5 pt-1">
                {actions}
                {canCopy ? (
                <motion.button
                  type="button"
                  aria-label={copied ? "已复制" : "复制改动"}
                  title={copied ? "已复制" : "复制改动"}
                  onClick={handleCopy}
                  whileTap={reduce ? undefined : { scale: 0.9 }}
                  transition={SPRING_PRESS}
                  className="grid size-7 place-items-center rounded-md text-cx-fg-3 outline-none transition-colors hover:bg-cx-bg/70 hover:text-cx-fg focus-visible:ring-2 focus-visible:ring-cx-focus"
                >
                  {copied ? (
                    <Check className="size-3.5" />
                  ) : (
                    <Copy className="size-3.5" />
                  )}
                </motion.button>
                ) : null}
              </div>
            ) : null}
          </div>
        </div>
      </AgentDisclosure>
    </div>
  );
}
