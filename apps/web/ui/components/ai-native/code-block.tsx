"use client";

/* ─────────────────────────────────────────────────────────
 * CODE BLOCK — Styled code container with line numbers and copy.
 *
 * Source: https://github.com/TurboKach/ai-native-react-components
 * Pinned Commit: 05dab2d2b5f1f3e40029776e339a486d70491079
 * License: MIT
 * Adapted for Project Muteki: Full code viewer with line numbers, syntax
 * highlighting accents, copy state, and optional filename header.
 * C17: real token highlight via shared highlightCodeLine (same as Diff).
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useState } from "react";
import { Icon } from "../Icon";
import { highlightCodeLine } from "@/lib/conversationDiff";

export interface CodeBlockProps {
  code: string;
  language?: string;
  filename?: string;
  showLineNumbers?: boolean;
  focusLine?: number | null;
  className?: string;
}

export function CodeBlock({
  code = "",
  language = "bash",
  filename,
  showLineNumbers = true,
  focusLine = null,
  className = "",
}: CodeBlockProps) {
  const [copied, setCopied] = useState(false);

  const copy = useCallback(() => {
    void navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  }, [code]);

  const lines = code.split("\n");
  const lang = (language || "text").toLowerCase();

  return (
    <div
      className={`ai-code-block w-full overflow-hidden rounded-card border border-line bg-inset shadow-card ${className}`}
      data-testid="code-block"
      data-language={lang}
    >
      {/* Header */}
      <div className="flex items-center justify-between border-b border-line bg-surface/80 px-3 py-1.5">
        <div className="flex items-center gap-2 min-w-0">
          {filename ? (
            <span className="font-mono text-[12px] font-medium text-ink truncate">
              {filename}
            </span>
          ) : (
            <span className="font-mono text-[11px] uppercase tracking-wider text-ink-3">
              {language}
            </span>
          )}
        </div>

        <button
          type="button"
          aria-label="复制代码"
          onClick={copy}
          className={`flex h-6 items-center gap-1 rounded-chip px-2 text-[11px] font-medium transition-colors hover:bg-hover ${
            copied ? "text-green" : "text-ink-3 hover:text-ink"
          }`}
        >
          {copied ? (
            <>
              <Icon name="check" size={11} />
              <span>已复制</span>
            </>
          ) : (
            <>
              <Icon name="copy" size={11} />
              <span>复制</span>
            </>
          )}
        </button>
      </div>

      {/* Code Body */}
      <pre className="max-h-96 overflow-auto p-3 font-mono text-[12px] leading-[1.65] text-ink">
        {lines.map((line, i) => {
          const lineNo = i + 1;
          const focused = focusLine === lineNo;
          return (
            <div
              key={i}
              className={`flex min-w-0${focused ? " ai-code-line-focus" : ""}`}
              data-preview-line={lineNo}
            >
              {showLineNumbers && (
                <span className="w-8 shrink-0 select-none text-right font-mono text-[10.5px] text-ink-3/60 pr-3">
                  {lineNo}
                </span>
              )}
              <span
                className="flex-1 whitespace-pre ai-code-line-tokens"
                dangerouslySetInnerHTML={{ __html: highlightCodeLine(line, lang) }}
              />
            </div>
          );
        })}
      </pre>
    </div>
  );
}
