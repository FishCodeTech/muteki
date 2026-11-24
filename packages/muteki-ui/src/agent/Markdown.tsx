"use client";

import type { ReactNode } from "react";
import { cn } from "../utils/cn";
import {
  ReactMarkdown,
  remarkGfm,
  type MarkdownComponents,
  type UrlTransform,
} from "../engines/markdown";
import { CodeBlock } from "./CodeBlock";

export interface AgentMarkdownProps {
  text: string;
  className?: string;
  urlTransform?: UrlTransform;
  renderLink?: (props: { href?: string; children?: ReactNode }) => ReactNode;
  components?: MarkdownComponents;
}

/** Pure, domain-agnostic GFM + syntax-highlighted Markdown renderer for Agent views. */
export function AgentMarkdown({
  text,
  className,
  urlTransform,
  renderLink,
  components: customComponents,
}: AgentMarkdownProps) {
  if (!text) return null;

  const components: MarkdownComponents = {
    pre({ children }) {
      return <>{children}</>;
    },
    code({ className: codeClass, children, ...props }) {
      const match = /language-([\w-]+)/.exec(codeClass || "");
      const code = String(children).replace(/\n$/, "");
      if (match || code.includes("\n")) {
        return (
          <CodeBlock
            code={code}
            language={match?.[1] || "text"}
            showLineNumbers
            className="my-2"
          />
        );
      }
      return (
        <code className="ai-markdown-inline-code" {...props}>
          {children}
        </code>
      );
    },
    a({ href, children, ...props }) {
      if (renderLink) return <>{renderLink({ href, children })}</>;
      return (
        <a
          href={href}
          target="_blank"
          rel="noopener noreferrer"
          className="ai-markdown-link"
          {...props}
        >
          {children}
        </a>
      );
    },
    table({ children, ...props }) {
      return (
        <div className="ai-markdown-table-wrap">
          <table className="ai-markdown-table" {...props}>
            {children}
          </table>
        </div>
      );
    },
    ...customComponents,
  };

  return (
    <div
      data-slot="agent-markdown"
      className={cn("ai-markdown-body min-w-0 max-w-full leading-relaxed", className)}
    >
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components} urlTransform={urlTransform}>
        {text}
      </ReactMarkdown>
    </div>
  );
}
