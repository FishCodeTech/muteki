"use client";

import { memo, useCallback, useMemo, useState, type ReactNode } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { copyToClipboard } from "@/lib/clipboard";
import {
  FONT_STYLE_BOLD,
  FONT_STYLE_ITALIC,
  FONT_STYLE_UNDERLINE,
  languageFromPath,
  normalizeLanguage,
  useHighlightedCode,
  type CodeToken,
} from "@/lib/chatHighlighter";
import { IconButton } from "./Button";
import { toast } from "./Toast";

/** Renders one tokenized line; colors switch with `.dark` via CSS vars. */
export const TokenLine = memo(function TokenLine({ tokens, fallback }: { tokens?: CodeToken[] | null; fallback: string }) {
  if (!tokens) return <>{fallback || "\u200b"}</>;
  if (!tokens.length) return <>{"\u200b"}</>;
  return (
    <>
      {tokens.map((token, index) => (
        <span
          key={index}
          className="cx-tk"
          style={{
            "--cx-tk": token.light,
            "--cx-tk-dark": token.dark ?? token.light,
            fontStyle: token.fontStyle && token.fontStyle & FONT_STYLE_ITALIC ? "italic" : undefined,
            fontWeight: token.fontStyle && token.fontStyle & FONT_STYLE_BOLD ? 600 : undefined,
            textDecoration: token.fontStyle && token.fontStyle & FONT_STYLE_UNDERLINE ? "underline" : undefined,
          } as React.CSSProperties}
        >
          {token.content}
        </span>
      ))}
    </>
  );
});

export function useCopy(timeout = 1600) {
  const [copied, setCopied] = useState(false);
  const copy = useCallback(async (text: string) => {
    if (!await copyToClipboard(text)) {
      toast({ title: "复制失败", description: "请重试，或手动选中内容复制。", tone: "danger" });
      return false;
    }
    setCopied(true);
    window.setTimeout(() => setCopied(false), timeout);
    return true;
  }, [timeout]);
  return { copied, copy };
}

export function CopyButton({ text, label = "复制", size = "xs", className }: { text: string | (() => string); label?: string; size?: "xs" | "sm"; className?: string }) {
  const { copied, copy } = useCopy();
  return (
    <IconButton
      size={size}
      icon={copied ? "check" : "copy"}
      label={copied ? "已复制" : label}
      className={cn(copied && "text-cx-success hover:text-cx-success", className)}
      onClick={() => void copy(typeof text === "function" ? text() : text)}
    />
  );
}

const LANG_LABEL: Record<string, string> = {
  typescript: "TypeScript", tsx: "TSX", javascript: "JavaScript", jsx: "JSX", python: "Python", bash: "Shell",
  json: "JSON", jsonc: "JSON", go: "Go", rust: "Rust", java: "Java", cpp: "C++", c: "C", csharp: "C#",
  html: "HTML", css: "CSS", scss: "SCSS", yaml: "YAML", toml: "TOML", sql: "SQL", diff: "Diff",
  markdown: "Markdown", docker: "Dockerfile", php: "PHP", ruby: "Ruby", kotlin: "Kotlin", swift: "Swift",
  xml: "XML", ini: "INI", lua: "Lua", powershell: "PowerShell", graphql: "GraphQL", makefile: "Makefile",
};

export function languageLabel(lang: string | null | undefined): string {
  const normalized = normalizeLanguage(lang);
  if (normalized) return LANG_LABEL[normalized] ?? normalized;
  return lang ? lang : "文本";
}

export interface CodeBlockProps {
  code: string;
  language?: string | null;
  filename?: string;
  showLineNumbers?: boolean;
  highlightLines?: number[];
  /** Collapse after this many lines with an expand affordance. 0 disables. */
  collapseAfter?: number;
  streaming?: boolean;
  className?: string;
  actions?: ReactNode;
  wrapByDefault?: boolean;
  maxHeight?: number;
  bare?: boolean;
}

export function CodeBlock({
  code,
  language,
  filename,
  showLineNumbers,
  highlightLines,
  collapseAfter = 24,
  streaming,
  className,
  actions,
  wrapByDefault = false,
  maxHeight,
  bare,
}: CodeBlockProps) {
  const lang = normalizeLanguage(language) ?? languageFromPath(filename) ?? language ?? null;
  const text = code.replace(/\n$/, "");
  const rawLines = useMemo(() => text.split("\n"), [text]);
  const tokens = useHighlightedCode(text, lang, !streaming || rawLines.length < 400);
  const [wrap, setWrap] = useState(wrapByDefault);
  const [expanded, setExpanded] = useState(false);
  const collapsible = collapseAfter > 0 && rawLines.length > collapseAfter + 4 && !streaming;
  const visible = collapsible && !expanded ? rawLines.slice(0, collapseAfter) : rawLines;
  const highlighted = useMemo(() => new Set(highlightLines ?? []), [highlightLines]);
  const gutter = String(rawLines.length).length;

  return (
    <div
      className={cn(
        "cx-code group/code relative my-3 overflow-hidden text-[12.5px]",
        !bare && "rounded-xl border border-cx-border-subtle bg-cx-code",
        className,
      )}
    >
      {!bare ? (
        <div className="flex h-9 items-center gap-2 border-b border-cx-border-subtle pl-3 pr-1.5">
          {filename ? <Icon name="fileCode" size={13} className="text-cx-fg-4" /> : null}
          <span className="min-w-0 flex-1 truncate font-cx-sans text-[12px] font-medium text-cx-fg-3">
            {filename ?? languageLabel(lang)}
          </span>
          {filename && lang ? <span className="text-[11px] text-cx-fg-4">{languageLabel(lang)}</span> : null}
          <div className="flex items-center gap-0.5 opacity-70 transition-opacity group-hover/code:opacity-100">
            {actions}
            <IconButton size="xs" icon="wrapText" label={wrap ? "不换行" : "自动换行"} active={wrap} onClick={() => setWrap((v) => !v)} />
            <CopyButton text={text} label="复制代码" />
          </div>
        </div>
      ) : null}
      <div
        className={cn("cx-scroll relative overflow-auto", collapsible && !expanded && "max-h-none")}
        style={maxHeight ? { maxHeight } : undefined}
      >
        <pre className={cn("m-0 py-3 font-cx-mono leading-[1.65] text-cx-fg-2", wrap ? "whitespace-pre-wrap break-words" : "whitespace-pre")}>
          <code className="block min-w-max">
            {visible.map((line, index) => (
              <span
                key={index}
                className={cn(
                  "flex px-4",
                  wrap && "min-w-0",
                  highlighted.has(index + 1) && "bg-cx-selected",
                )}
              >
                {showLineNumbers ? (
                  <span className="mr-4 inline-block shrink-0 select-none text-right text-cx-fg-4" style={{ width: `${gutter}ch` }}>
                    {index + 1}
                  </span>
                ) : null}
                <span className={cn("min-w-0 flex-1", wrap && "whitespace-pre-wrap break-words")}>
                  <TokenLine tokens={tokens?.[index]} fallback={line} />
                  {streaming && index === visible.length - 1 ? <span className="cx-caret" aria-hidden /> : null}
                </span>
              </span>
            ))}
          </code>
        </pre>
        {collapsible && !expanded ? (
          <div className="pointer-events-none absolute inset-x-0 bottom-0 h-20 bg-gradient-to-t from-[var(--cx-code)] to-transparent" />
        ) : null}
      </div>
      {collapsible ? (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          className="flex h-8 w-full items-center justify-center gap-1.5 border-t border-cx-border-subtle text-[12px] font-medium text-cx-fg-3 transition-colors hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name={expanded ? "chevronsDownUp" : "chevronsUpDown"} size={13} />
          {expanded ? "收起" : `展开全部 ${rawLines.length} 行`}
        </button>
      ) : null}
    </div>
  );
}
