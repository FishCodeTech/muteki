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
import { ActionSwapIcon } from "../primitives/ActionSwap";
import { AgentDisclosure } from "./AgentDisclosure";
import { DiffStat } from "../primitives/Feedback";
import { AgentCodeLine, useAgentCodeTokens, type AgentCodeLanguage } from "./CodeBlock";

export type FileDiffStatus = "preview" | "streaming" | "complete";
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
  label?: ReactNode;
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
    if (!viewport || !currentOpen || !streaming) {
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
      data-slot="file-diff"
      data-state={status}
      aria-busy={streaming || undefined}
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
        <Icon name="fileCode" size={15} className="shrink-0 text-cx-fg-3" />
        {label ? <span className="shrink-0 font-medium text-cx-fg/90">{label}</span> : null}
        <span className="min-w-0 flex-1 truncate font-cx-mono text-cx-xs text-cx-fg/80">
          {file}
        </span>
        <DiffStat additions={additions} deletions={deletions} className="shrink-0" />
        <span className="grid size-4 shrink-0 place-items-center text-cx-fg-3/60">
          {status === "preview" ? (
            <Icon name="fileCode" size={14} title="改动预览" />
          ) : streaming ? (
            <Icon name="loader" size={14} title="正在应用改动" className="cx-spin" />
          ) : (
            <Icon name="check" size={14} title="改动已应用" />
          )}
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
              data-slot="file-diff-viewport"
              aria-live="polite"
              className="cx-scroll overflow-auto"
              style={{ maxHeight }}
            >
              <div className="py-1 font-cx-mono text-cx-xs leading-5">
                <span className="sr-only">文件改动</span>
                {lines.map((line, index) => {
                  const type = line.type ?? "context";
                  return (
                    <div
                      key={line.id}
                      className={cn(
                        "grid grid-cols-[2.25rem_2.25rem_1rem_minmax(0,1fr)]",
                        type === "added" && "bg-cx-add-bg",
                        type === "removed" && "bg-cx-del-bg",
                      )}
                    >
                      <span className="cx-tabular select-none pr-2 text-right text-cx-fg-4/60">
                        {line.oldLine}
                      </span>
                      <span className="cx-tabular select-none pr-2 text-right text-cx-fg-4/60">
                        {line.newLine}
                      </span>
                      <span
                        className={cn(
                          "select-none text-center text-cx-fg-4",
                          type === "added" && "text-cx-add",
                          type === "removed" && "text-cx-del",
                        )}
                      >
                        {type === "added" ? "+" : type === "removed" ? "−" : ""}
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
              <div className="flex items-center justify-end gap-0.5 border-t border-cx-border-subtle px-2 py-1">
                {actions}
                {canCopy ? (
                  <button
                    type="button"
                    aria-label={copied ? "已复制" : "复制改动"}
                    title={copied ? "已复制" : "复制改动"}
                    onClick={() => void handleCopy()}
                    className="cx-press grid size-7 place-items-center rounded-[6px] text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                  >
                    <ActionSwapIcon value={copied ? "check" : "copy"}>
                      <Icon name={copied ? "check" : "copy"} size={14} className={copied ? "text-cx-success" : undefined} />
                    </ActionSwapIcon>
                  </button>
                ) : null}
              </div>
            ) : null}
          </div>
        </div>
      </AgentDisclosure>
    </div>
  );
}
