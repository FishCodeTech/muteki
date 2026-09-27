"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.
// Tokenizes through the shared chat highlighter instead of a second full Shiki bundle.

import {
  type CSSProperties,
  Fragment,
  useEffect,
  useState,
} from "react";
import { highlightCode } from "@/lib/chatHighlighter";
import { cn } from "@/lib/cn";

/** Any language the shared highlighter knows; unknown languages render as plain text. */
export type AgentCodeLanguage = string;

export interface AgentCodeToken {
  content: string;
  offset: number;
  light?: string;
  dark?: string;
}

export type AgentCodeTokenLines = AgentCodeToken[][];

export interface AgentCodeProps {
  code: string;
  language?: AgentCodeLanguage;
  className?: string;
}

export interface AgentCodeLineProps {
  code: string;
  tokens?: AgentCodeToken[];
  className?: string;
}

async function tokenize(code: string, language: AgentCodeLanguage): Promise<AgentCodeTokenLines | null> {
  const lines = await highlightCode(code, language);
  if (!lines) return null;
  return lines.map((line) => {
    let offset = 0;
    return line.map((token) => {
      const next = { content: token.content, offset, light: token.light, dark: token.dark };
      offset += token.content.length;
      return next;
    });
  });
}

export function useAgentCodeTokens(
  code: string,
  language: AgentCodeLanguage,
) {
  const key = `${language}\u0000${code}`;
  const [result, setResult] = useState<{
    key: string;
    code: string;
    language: AgentCodeLanguage;
    lines: AgentCodeTokenLines;
  } | null>(null);

  useEffect(() => {
    let cancelled = false;
    void tokenize(code, language).then((lines) => {
      if (cancelled || !lines) return;
      setResult({ key, code, language, lines });
    });
    return () => {
      cancelled = true;
    };
  }, [code, key, language]);

  if (result?.key === key) return result.lines;
  // While streaming, keep the previous tokens until the longer prefix is ready.
  if (result?.language === language && code.startsWith(result.code)) {
    return result.lines;
  }
  return null;
}

export function AgentCodeLine({
  code,
  tokens,
  className,
}: AgentCodeLineProps) {
  return (
    <span className={className}>
      {tokens
        ? tokens.map((token) => (
            <span
              key={`${token.offset}-${token.content}`}
              style={
                {
                  "--agent-code-light": token.light ?? "currentColor",
                  "--agent-code-dark": token.dark ?? token.light ?? "currentColor",
                } as CSSProperties
              }
              className="text-[var(--agent-code-light)] dark:text-[var(--agent-code-dark)]"
            >
              {token.content}
            </span>
          ))
        : code}
    </span>
  );
}

export function AgentCode({
  code,
  language = "bash",
  className,
}: AgentCodeProps) {
  const tokens = useAgentCodeTokens(code, language);
  let offset = 0;
  const lines = code.split("\n").map((content) => {
    const line = { content, offset };
    offset += content.length + 1;
    return line;
  });

  return (
    <pre
      className={cn(
        "m-0 overflow-x-auto whitespace-pre font-cx-mono text-xs leading-5 text-cx-fg/85",
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
