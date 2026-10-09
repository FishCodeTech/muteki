"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
// Local additions: wrap toggle, actions slot, display label, streaming-only status.

import { Check, Copy, FileCode2, LoaderCircle, WrapText } from "lucide-react";
import { motion, useReducedMotion } from "motion/react";
import {
  type ReactNode,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  type AgentCodeLanguage,
  AgentCodeLine,
  useAgentCodeTokens,
} from "@/components/agentui/agents/agent-code";
import { ActionSwapIcon } from "@/components/agentui/motion/action-swap";
import { SPRING_PRESS } from "@/components/agentui/lib/ease";
import { cn } from "@/lib/cn";

export type CodeBlockStatus = "streaming" | "complete";

export interface CodeBlockProps {
  code: string;
  language?: AgentCodeLanguage;
  /** Human label for the language chip; defaults to the raw language id. */
  languageLabel?: ReactNode;
  filename?: ReactNode;
  status?: CodeBlockStatus;
  showLineNumbers?: boolean;
  highlightLines?: number[];
  maxHeight?: number;
  wrap?: boolean;
  /** Show a soft-wrap toggle in the header. */
  wrapToggle?: boolean;
  copyable?: boolean;
  onCopy?: () => void | Promise<void>;
  /** Extra header controls rendered before copy. */
  actions?: ReactNode;
  className?: string;
}

function HeaderButton({
  label,
  active,
  onClick,
  children,
}: {
  label: string;
  active?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  const reduce = useReducedMotion() ?? false;
  return (
    <motion.button
      type="button"
      aria-label={label}
      aria-pressed={active}
      title={label}
      onClick={onClick}
      whileTap={reduce ? undefined : { scale: 0.9 }}
      transition={SPRING_PRESS}
      className={cn(
        "grid size-7 shrink-0 place-items-center rounded-full text-cx-fg-3 outline-none transition-colors hover:bg-cx-bg/70 hover:text-cx-fg focus-visible:ring-2 focus-visible:ring-cx-focus",
        active && "bg-cx-bg/70 text-cx-fg",
      )}
    >
      {children}
    </motion.button>
  );
}

export function CodeBlock({
  code,
  language = "typescript",
  languageLabel,
  filename,
  status = "complete",
  showLineNumbers = true,
  highlightLines = [],
  maxHeight = 280,
  wrap: wrapDefault = false,
  wrapToggle = false,
  copyable = true,
  onCopy,
  actions,
  className,
}: CodeBlockProps) {
  const reduce = useReducedMotion() ?? false;
  const viewportRef = useRef<HTMLDivElement>(null);
  const copyTimer = useRef<number | undefined>(undefined);
  const [copied, setCopied] = useState(false);
  const [wrap, setWrap] = useState(wrapDefault);
  const streaming = status === "streaming";
  const tokens = useAgentCodeTokens(code, language);
  const highlighted = useMemo(
    () => new Set(highlightLines),
    [highlightLines],
  );
  let offset = 0;
  const lines = code.split("\n").map((content) => {
    const line = { content, offset };
    offset += content.length + 1;
    return line;
  });

  useEffect(
    () => () => {
      if (copyTimer.current) window.clearTimeout(copyTimer.current);
    },
    [],
  );

  useLayoutEffect(() => {
    const viewport = viewportRef.current;
    if (!viewport || !streaming) return;

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
    else await navigator.clipboard?.writeText(code);

    setCopied(true);
    if (copyTimer.current) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1600);
  }, [code, onCopy]);

  return (
    <div
      data-state={status}
      aria-busy={streaming}
      className={cn(
        "group/code w-full overflow-hidden rounded-2xl bg-cx-hover/80 text-sm",
        className,
      )}
    >
      <div className="flex h-10 items-center gap-2.5 pl-3 pr-1.5">
        <FileCode2
          aria-hidden="true"
          className="size-3.5 shrink-0 text-cx-fg-3/70"
        />
        {filename ? (
          <span className="min-w-0 truncate font-cx-mono text-xs text-cx-fg/80">
            {filename}
          </span>
        ) : null}
        <span className="text-[12px] font-medium uppercase tracking-wide text-cx-fg-3/55">
          {languageLabel ?? language}
        </span>
        <span className="ml-auto" />
        {streaming ? (
          <span className="inline-flex shrink-0 items-center gap-1 text-[12px] font-medium text-cx-accent">
            <LoaderCircle className={cn("size-3", !reduce && "animate-spin")} />
            生成中
          </span>
        ) : null}
        <span className="flex items-center gap-0.5 opacity-70 transition-opacity group-hover/code:opacity-100 group-focus-within/code:opacity-100">
          {actions}
          {wrapToggle ? (
            <HeaderButton label={wrap ? "不换行" : "自动换行"} active={wrap} onClick={() => setWrap((value) => !value)}>
              <WrapText className="size-3.5" />
            </HeaderButton>
          ) : null}
          {copyable || onCopy ? (
            <HeaderButton label={copied ? "已复制" : "复制代码"} onClick={handleCopy}>
              <ActionSwapIcon value={copied ? "copied" : "copy"}>
                {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
              </ActionSwapIcon>
            </HeaderButton>
          ) : null}
        </span>
      </div>

      <div
        ref={viewportRef}
        role={streaming ? "log" : undefined}
        aria-live={streaming ? "polite" : undefined}
        className="cx-scroll overflow-auto border-t border-cx-fg/[0.06] py-2"
        style={{ maxHeight }}
      >
        <pre className={cn("m-0 font-cx-mono text-xs leading-5 text-cx-fg/85", wrap ? "min-w-0" : "min-w-max")}>
          <code>
            {lines.map((line, index) => {
              const lineNumber = index + 1;
              return (
                <span
                  key={line.offset}
                  className={cn(
                    "grid min-h-5",
                    showLineNumbers
                      ? "grid-cols-[2.75rem_minmax(0,1fr)]"
                      : "grid-cols-1",
                    highlighted.has(lineNumber) && "bg-cx-accent/[0.07]",
                  )}
                >
                  {showLineNumbers ? (
                    <span className="select-none pr-3 text-right tabular-nums text-cx-fg-3/35">
                      {lineNumber}
                    </span>
                  ) : null}
                  <AgentCodeLine
                    code={line.content || "\u200b"}
                    tokens={tokens?.[index]}
                    className={cn(
                      "pr-4",
                      showLineNumbers ? "pl-1" : "pl-4",
                      wrap ? "whitespace-pre-wrap break-words" : "whitespace-pre",
                    )}
                  />
                </span>
              );
            })}
          </code>
        </pre>
      </div>
    </div>
  );
}
