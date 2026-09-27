"use client";

import React from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "./code-block";
import {
  parseResourceLink,
  safeMarkdownUrlTransform,
  type ResourceLinkTarget,
} from "@/lib/resourcePreview";

export interface MessageMarkdownProps {
  text: string;
  className?: string;
  onResourceLink?: (target: ResourceLinkTarget) => void;
}

function buildComponents(onResourceLink?: (target: ResourceLinkTarget) => void): Components {
  return {
    pre({ children }) {
      return <>{children}</>;
    },
    code({ className, children, ...props }) {
      const match = /language-([\w-]+)/.exec(className || "");
      const code = String(children).replace(/\n$/, "");
      if (match || code.includes("\n")) {
        return (
          <CodeBlock
            code={code}
            language={match?.[1] || "text"}
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
      const safeHref = safeMarkdownUrlTransform(String(href || ""));
      const target = parseResourceLink(safeHref);
      if (target.kind === "external") {
        return (
          <a
            href={target.href}
            target="_blank"
            rel="noopener noreferrer"
            className="ai-markdown-link"
            data-link-kind="external"
            {...props}
          >
            {children}
          </a>
        );
      }
      if (onResourceLink && (target.kind === "workspace" || target.kind === "artifact" || target.kind === "message")) {
        return (
          <button
            type="button"
            className="ai-markdown-link ai-markdown-resource-link"
            data-link-kind={target.kind}
            data-testid={
              target.kind === "workspace"
                ? "markdown-workspace-link"
                : target.kind === "artifact"
                  ? "markdown-artifact-link"
                  : "markdown-message-link"
            }
            onClick={(event) => {
              event.preventDefault();
              onResourceLink(target);
            }}
          >
            {children}
          </button>
        );
      }
      // Never echo stripped/unsafe schemes as a navigable href.
      if (!safeHref) {
        return (
          <span className="ai-markdown-link ai-markdown-link-blocked" data-link-kind="blocked">
            {children}
          </span>
        );
      }
      return (
        <a
          href={safeHref}
          className="ai-markdown-link"
          data-link-kind={target.kind}
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
  };
}

export function MessageMarkdown({ text, className = "", onResourceLink }: MessageMarkdownProps) {
  if (!text) return null;
  const components = buildComponents(onResourceLink);

  return (
    <div className={`ai-markdown-body min-w-0 max-w-full leading-relaxed ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={components}
        // Allow workspace:/file:/artifact: while blocking javascript:/data: etc.
        urlTransform={safeMarkdownUrlTransform}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
}
