"use client";

import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import { cn } from "../utils/cn";
import { copyToClipboard } from "../utils/clipboard";
import { useReducedMotion } from "../utils/reducedMotion";
import { Icon } from "../icons/Icon";
import { IconButton } from "../primitives/Button";
import { toast } from "../primitives/Toast";
import {
  FONT_STYLE_BOLD,
  FONT_STYLE_ITALIC,
  FONT_STYLE_UNDERLINE,
  highlightCode,
  languageFromPath,
  normalizeLanguage,
  useHighlightedCode,
  type CodeToken,
} from "../engines/shiki";

export type AgentCodeLanguage = string;
export type CodeBlockStatus = "streaming" | "complete";

export interface AgentCodeToken {
  content: string;
  offset: number;
  light?: string;
  dark?: string;
  fontStyle?: number;
}

export type AgentCodeTokenLines = AgentCodeToken[][];

async function tokenizeWithOffsets(
  code: string,
  language: AgentCodeLanguage,
): Promise<AgentCodeTokenLines | null> {
  const lines = await highlightCode(code, language);
  if (!lines) return null;
  return lines.map((line) => {
    let offset = 0;
    return line.map((token) => {
      const next: AgentCodeToken = {
        content: token.content,
        offset,
        light: token.light,
        dark: token.dark,
        fontStyle: token.fontStyle,
      };
      offset += token.content.length;
      return next;
    });
  });
}

export function useAgentCodeTokens(
  code: string,
  language: AgentCodeLanguage,
): AgentCodeTokenLines | null {
  const key = `${language}\u0000${code}`;
  const [result, setResult] = useState<{
    key: string;
    code: string;
    language: AgentCodeLanguage;
    lines: AgentCodeTokenLines;
  } | null>(null);

  useEffect(() => {
    let cancelled = false;
    void tokenizeWithOffsets(code, language).then((lines) => {
      if (cancelled || !lines) return;
      setResult({ key, code, language, lines });
    });
    return () => {
      cancelled = true;
    };
  }, [code, key, language]);

  if (result?.key === key) return result.lines;
  if (result?.language === language && code.startsWith(result.code)) {
    return result.lines;
  }
  return null;
}

/** Renders one highlighted line; light/dark colors switch via CSS vars. */
export const TokenLine = memo(function TokenLine({
  tokens,
  fallback,
}: {
  tokens?: CodeToken[] | null;
  fallback: string;
}) {
  if (!tokens) return <>{fallback || "\u200b"}</>;
  if (!tokens.length) return <>{"\u200b"}</>;
  return (
    <>
      {tokens.map((token, index) => (
        <span
          key={index}
          className="cx-tk"
          style={
            {
              "--cx-tk": token.light,
              "--cx-tk-dark": token.dark ?? token.light,
              fontStyle:
                token.fontStyle && token.fontStyle & FONT_STYLE_ITALIC ? "italic" : undefined,
              fontWeight:
                token.fontStyle && token.fontStyle & FONT_STYLE_BOLD ? 600 : undefined,
              textDecoration:
                token.fontStyle && token.fontStyle & FONT_STYLE_UNDERLINE ? "underline" : undefined,
            } as CSSProperties
          }
        >
          {token.content}
        </span>
      ))}
    </>
  );
});

export function AgentCodeLine({
  code,
  tokens,
  className,
}: {
  code: string;
  tokens?: CodeToken[] | null;
  className?: string;
}) {
  return (
    <span className={className}>
      <TokenLine tokens={tokens} fallback={code} />
    </span>
  );
}

export function AgentCode({
  code,
  language = "bash",
  className,
}: {
  code: string;
  language?: AgentCodeLanguage;
  className?: string;
}) {
  const tokens = useAgentCodeTokens(code, language);
  let offset = 0;
  const lines = code.split("\n").map((content) => {
    const line = { content, offset };
    offset += content.length + 1;
    return line;
  });

  return (
    <pre
      data-slot="agent-code"
      className={cn(
        "m-0 overflow-x-auto whitespace-pre font-cx-mono text-cx-xs leading-5 text-cx-fg/85",
        className,
      )}
    >
      <code>
        {lines.map((line, index) => (
          <Fragment key={line.offset}>
            <AgentCodeLine code={line.content} tokens={tokens?.[index]} />
            {index < lines.length - 1 ? "\n" : null}
          </Fragment>
        ))}
      </code>
    </pre>
  );
}

export { useCopy, CopyButton } from "../primitives/CopyButton";

const LANG_LABEL: Record<string, string> = {
  typescript: "TypeScript",
  tsx: "TSX",
  javascript: "JavaScript",
  jsx: "JSX",
  python: "Python",
  bash: "Shell",
  json: "JSON",
  jsonc: "JSON",
  go: "Go",
  rust: "Rust",
  java: "Java",
  cpp: "C++",
  c: "C",
  csharp: "C#",
  html: "HTML",
  css: "CSS",
  scss: "SCSS",
  yaml: "YAML",
  toml: "TOML",
  sql: "SQL",
  diff: "Diff",
  markdown: "Markdown",
  docker: "Dockerfile",
  php: "PHP",
  ruby: "Ruby",
  kotlin: "Kotlin",
  swift: "Swift",
  xml: "XML",
  ini: "INI",
  lua: "Lua",
  powershell: "PowerShell",
  graphql: "GraphQL",
  makefile: "Makefile",
};

export function languageLabel(lang: string | null | undefined): string {
  const normalized = normalizeLanguage(lang);
  if (normalized) return LANG_LABEL[normalized] ?? normalized;
  return lang || "文本";
}

export interface CodeBlockProps {
  code: string;
  language?: AgentCodeLanguage | null;
  languageLabel?: ReactNode;
  filename?: ReactNode;
  status?: CodeBlockStatus;
  showLineNumbers?: boolean;
  highlightLines?: number[];
  focusLine?: number | null;
  /** Collapse after this many lines with an expand toggle. `0` disables. */
  collapseAfter?: number;
  streaming?: boolean;
  wrap?: boolean;
  wrapByDefault?: boolean;
  wrapToggle?: boolean;
  copyable?: boolean;
  onCopy?: () => void | Promise<void>;
  actions?: ReactNode;
  maxHeight?: number;
  bare?: boolean;
  className?: string;
}

export function CodeBlock({
  code,
  language,
  languageLabel: customLanguageLabel,
  filename,
  status,
  showLineNumbers = false,
  highlightLines = [],
  focusLine = null,
  collapseAfter = 24,
  streaming: streamingProp,
  wrap: wrapProp,
  wrapByDefault = false,
  wrapToggle = true,
  copyable = true,
  onCopy,
  actions,
  maxHeight,
  bare = false,
  className,
}: CodeBlockProps) {
  const reduced = useReducedMotion();
  const isStreaming = Boolean(streamingProp || status === "streaming");
  const filenameStr = typeof filename === "string" ? filename : undefined;
  const lang = normalizeLanguage(language) ?? languageFromPath(filenameStr) ?? language ?? null;
  const text = code.replace(/\n$/, "");
  const [wrap, setWrap] = useState(wrapProp ?? wrapByDefault);
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const viewportRef = useRef<HTMLDivElement>(null);
  const wasStreamingRef = useRef(false);
  const copyTimer = useRef<number | undefined>(undefined);

  const lines = useMemo(() => {
    let offset = 0;
    return text.split("\n").map((content) => {
      const entry = { content, offset };
      offset += content.length + 1;
      return entry;
    });
  }, [text]);

  const tokens = useHighlightedCode(text, lang, !isStreaming || lines.length < 400);
  const highlighted = useMemo(() => {
    const next = new Set(highlightLines);
    if (focusLine != null) next.add(focusLine);
    return next;
  }, [focusLine, highlightLines]);

  const collapsible = collapseAfter > 0 && lines.length > collapseAfter + 4 && !isStreaming;
  const visible = collapsible && !expanded ? lines.slice(0, collapseAfter) : lines;
  const gutter = String(lines.length).length;
  const displayLang = customLanguageLabel ?? (lang ? languageLabel(lang) : language || "文本");

  useEffect(
    () => () => {
      if (copyTimer.current) window.clearTimeout(copyTimer.current);
    },
    [],
  );

  useLayoutEffect(() => {
    const viewport = viewportRef.current;
    if (!viewport || !isStreaming) {
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
    const ok = onCopy ? (await onCopy(), true) : await copyToClipboard(text);
    if (!ok) {
      toast({ title: "复制失败", description: "请重试，或手动选中内容复制。", tone: "danger" });
      return;
    }
    setCopied(true);
    if (copyTimer.current) window.clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopied(false), 1600);
  }, [onCopy, text]);

  return (
    <div
      data-slot="code-block"
      data-state={isStreaming ? "streaming" : "complete"}
      data-language={lang ?? undefined}
      aria-busy={isStreaming || undefined}
      className={cn(
        "cx-code group/code relative my-3 overflow-hidden text-cx-xs",
        !bare && "rounded-[10px] border border-cx-border bg-cx-code shadow-[var(--ui-sheen)]",
        className,
      )}
    >
      {!bare ? (
        <div className="flex h-9 items-center gap-2 border-b border-cx-border-subtle bg-cx-bg-subtle/60 pl-3 pr-1.5">
          {filename ? <Icon name="fileCode" size={13} className="shrink-0 text-cx-fg-4" /> : null}
          <span className="min-w-0 flex-1 truncate font-cx-sans text-cx-xs font-medium text-cx-fg-2">
            {filename ?? displayLang}
          </span>
          {filename && displayLang ? (
            <span className="shrink-0 font-cx-mono text-cx-2xs text-cx-fg-4">{displayLang}</span>
          ) : null}
          {isStreaming ? (
            <span className="inline-flex shrink-0 items-center gap-1 text-cx-2xs font-medium text-cx-accent">
              <Icon name="loader" size={12} className="cx-spin" />
              生成中
            </span>
          ) : null}
          <div className="flex items-center gap-0.5 opacity-75 transition-opacity group-hover/code:opacity-100 group-focus-within/code:opacity-100">
            {actions}
            {wrapToggle ? (
              <IconButton
                size="xs"
                icon="wrapText"
                label={wrap ? "不换行" : "自动换行"}
                active={wrap}
                onClick={() => setWrap((v) => !v)}
              />
            ) : null}
            {copyable || onCopy ? (
              <IconButton
                size="xs"
                icon={copied ? "check" : "copy"}
                label={copied ? "已复制" : "复制代码"}
                className={cn(copied && "text-cx-success hover:text-cx-success")}
                onClick={() => void handleCopy()}
              />
            ) : null}
          </div>
        </div>
      ) : null}

      <div
        ref={viewportRef}
        role={isStreaming ? "log" : undefined}
        aria-live={isStreaming ? "polite" : undefined}
        className={cn("cx-scroll relative overflow-auto", collapsible && !expanded && "max-h-none")}
        style={maxHeight ? { maxHeight } : undefined}
      >
        <pre
          className={cn(
            "m-0 py-3 font-cx-mono leading-[1.65] text-cx-fg-2",
            wrap ? "whitespace-pre-wrap break-words" : "whitespace-pre",
          )}
        >
          <code className={cn("block", wrap ? "min-w-0" : "min-w-max")}>
            {visible.map((line, index) => {
              const lineNo = index + 1;
              return (
                <span
                  key={line.offset}
                  className={cn(
                    "flex px-4",
                    wrap && "min-w-0",
                    highlighted.has(lineNo) && "bg-cx-selected",
                  )}
                >
                  {showLineNumbers ? (
                    <span
                      className="cx-tabular mr-4 inline-block shrink-0 select-none text-right text-cx-fg-4"
                      style={{ width: `${gutter}ch` }}
                    >
                      {lineNo}
                    </span>
                  ) : null}
                  <span
                    data-code-start={line.offset}
                    data-code-length={line.content.length}
                    className={cn("min-w-0 flex-1", wrap && "whitespace-pre-wrap break-words")}
                  >
                    <TokenLine tokens={tokens?.[index]} fallback={line.content} />
                    {isStreaming && index === visible.length - 1 ? (
                      <span className="cx-caret" aria-hidden />
                    ) : null}
                  </span>
                </span>
              );
            })}
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
          className="flex h-8 w-full items-center justify-center gap-1.5 border-t border-cx-border-subtle text-cx-xs font-medium text-cx-fg-3 transition-colors hover:bg-cx-hover hover:text-cx-fg"
        >
          <Icon name={expanded ? "chevronsDownUp" : "chevronsUpDown"} size={13} />
          {expanded ? "收起" : `展开全部 ${lines.length} 行`}
        </button>
      ) : null}
    </div>
  );
}
