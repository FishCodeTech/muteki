"use client";

import type { ReactNode } from "react";
import { AgentMarkdown, CodeBlock } from "@muteki/ui/agent";
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

function splitLiteralUrlProse(
  href: string,
  children: ReactNode,
): { url: string; trailing: string } | null {
  const label =
    typeof children === "string"
      ? children
      : Array.isArray(children) && children.length === 1 && typeof children[0] === "string"
        ? children[0]
        : null;
  if (!label) return null;
  const match = /^(https?:\/\/[^\s；，。！？、]+)([；，。！？、].+)$/u.exec(label);
  if (!match) return null;
  try {
    if (new URL(label).href !== new URL(href).href) return null;
  } catch {
    return null;
  }
  return { url: match[1], trailing: match[2] };
}

export function MessageMarkdown({
  text,
  className = "",
  onResourceLink,
}: MessageMarkdownProps) {
  if (!text) return null;

  return (
    <AgentMarkdown
      text={text}
      className={className}
      urlTransform={safeMarkdownUrlTransform}
      components={{
        code({ className: codeClass, children, ...props }) {
          const match = /language-([\w-]+)/.exec(codeClass || "");
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
            const split = splitLiteralUrlProse(target.href, children);
            return (
              <>
                <a
                  href={split ? split.url : target.href}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="ai-markdown-link"
                  data-link-kind="external"
                  {...props}
                >
                  {split ? split.url : children}
                </a>
                {split?.trailing}
              </>
            );
          }
          if (
            onResourceLink &&
            (target.kind === "workspace" ||
              target.kind === "artifact" ||
              target.kind === "legacy_artifact" ||
              target.kind === "message")
          ) {
            return (
              <button
                type="button"
                className="ai-markdown-link ai-markdown-resource-link"
                data-link-kind={target.kind}
                data-testid={
                  target.kind === "workspace"
                    ? "markdown-workspace-link"
                    : target.kind === "artifact" || target.kind === "legacy_artifact"
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
          if (!safeHref) {
            return (
              <span
                className="ai-markdown-link ai-markdown-link-blocked"
                data-link-kind="blocked"
              >
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
      }}
    />
  );
}
