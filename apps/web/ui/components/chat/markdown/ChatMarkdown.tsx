"use client";

import {
  Children,
  isValidElement,
  memo,
  useMemo,
  useRef,
  type ReactElement,
  type ReactNode,
  type RefObject,
} from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import type { PluggableList } from "unified";
import { remarkChatMath } from "@/lib/chatMath";
import { ChatMath } from "./ChatMath";
import { ChatVisualization, parseVisualization } from "./ChatVisualization";
import { cn } from "@/lib/cn";
import { useLang } from "@/lib/i18n";
import { hasCrossBlockReferences, splitMarkdownBlocks } from "@/lib/chatMarkdownBlocks";
export { splitMarkdownBlocks } from "@/lib/chatMarkdownBlocks";
import { Icon } from "@/components/Icon";
import { languageLabel } from "@/components/chat/ui";
import { CodeBlock } from "@/components/agentui/agents/code-block";
import { normalizeLanguage } from "@/lib/chatHighlighter";
import {
  parseResourceLink,
  safeMarkdownUrlTransform,
  type ResourceLinkTarget,
} from "@/lib/resourcePreview";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";
import { openLocalUrl } from "@/components/chat/timeline/LocalUrlChips";

export interface ChatMarkdownProps {
  text: string;
  /** Live SSE text: completed blocks stay memoized, the tail shows a caret. */
  streaming?: boolean;
  /** Enables in-app preview for localhost links. */
  threadId?: string;
  messageId?: string;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  size?: "md" | "sm";
  className?: string;
}

export const CHAT_REMARK_PLUGINS: PluggableList = [remarkGfm, remarkMath, remarkChatMath];
const FENCE_RE = /^\s{0,3}(`{3,}|~{3,})/;

function endsInOpenFence(block: string): boolean {
  let fence: { char: string; size: number } | null = null;
  for (const line of block.split("\n")) {
    const match = FENCE_RE.exec(line);
    if (!match) continue;
    const marker = match[1];
    if (!fence) fence = { char: marker[0], size: marker.length };
    else if (marker[0] === fence.char && marker.length >= fence.size && line.trim().replace(/[`~]/g, "") === "") fence = null;
  }
  return fence !== null;
}

function textOf(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(textOf).join("");
  if (isValidElement<{ children?: ReactNode }>(node)) return textOf(node.props.children);
  return "";
}

function resourceTestId(kind: "workspace" | "artifact" | "message"): string {
  switch (kind) {
    case "workspace":
      return "markdown-workspace-link";
    case "artifact":
      return "markdown-artifact-link";
    case "message":
      return "markdown-message-link";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

function buildComponents(options: {
  threadId?: string;
  resourceLinkRef: RefObject<((target: ResourceLinkTarget) => void) | undefined>;
  resourceLinks: boolean;
  streamingTail: boolean;
}): Components {
  const { threadId, resourceLinkRef, resourceLinks, streamingTail } = options;
  const onResourceLink = resourceLinks
    ? (target: ResourceLinkTarget) => resourceLinkRef.current?.(target)
    : undefined;
  return {
    p({ children, node, ...props }) {
      return <p {...props} data-md-local-start={node?.position?.start.offset} data-md-local-end={node?.position?.end.offset}>{children}</p>;
    },
    li({ children, node, ...props }) {
      return <li {...props} data-md-local-start={node?.position?.start.offset} data-md-local-end={node?.position?.end.offset}>{children}</li>;
    },
    span({ children, node, ...props }) {
      const source = node?.properties?.["data-math-source"];
      if (typeof source === "string") return <ChatMath value={String(node?.properties?.["data-chat-math"] || "")} source={source} display={false} />;
      return <span {...props}>{children}</span>;
    },
    div({ children, node, ...props }) {
      const source = node?.properties?.["data-math-source"];
      if (typeof source === "string") return <ChatMath value={String(node?.properties?.["data-chat-math"] || "")} source={source} display />;
      return <div {...props}>{children}</div>;
    },
    pre({ children, node }) {
      const child = Children.toArray(children)[0];
      const element = isValidElement(child)
        ? (child as ReactElement<{ className?: string; children?: ReactNode }>)
        : null;
      const className = element?.props.className || "";
      const language = /language-([\w+#.-]+)/.exec(className)?.[1];
      const code = textOf(element ? element.props.children : children).replace(/\n$/, "");
      const lang = normalizeLanguage(language) ?? language ?? "text";
      return (
        <div data-md-local-start={node?.position?.start.offset} data-md-local-end={node?.position?.end.offset}><CodeBlock
          code={code}
          language={lang}
          languageLabel={languageLabel(language)}
          status={streamingTail ? "streaming" : "complete"}
          showLineNumbers={code.split("\n").length > 1}
          maxHeight={560}
          wrapToggle
          className="cx-code cx-md-code"
        /></div>
      );
    },
    code({ className, children, node: _node, ...props }) {
      return (
        <code className={cn("cx-md-inline-code", className)} {...props}>
          {children}
        </code>
      );
    },
    a({ href, children, node: _node, ...props }) {
      const safeHref = safeMarkdownUrlTransform(String(href || ""));
      const target = parseResourceLink(safeHref);
      if (target.kind === "external") {
        if (threadId && isLocalPreviewUrl(target.href)) {
          return (
            <a
              {...props}
              href={target.href}
              target="_blank"
              rel="noopener noreferrer"
              className="cx-md-link"
              data-link-kind="local-preview"
              title="在预览面板中打开（⌘/Ctrl+点击在新标签页打开）"
              onClick={(event) => {
                if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return;
                event.preventDefault();
                openLocalUrl(threadId, target.href);
              }}
            >
              {children}
              <span className="cx-md-preview-pill" aria-hidden>
                <Icon name="globe" size={10} />
                预览
              </span>
            </a>
          );
        }
        return (
          <a {...props} href={target.href} target="_blank" rel="noopener noreferrer" className="cx-md-link" data-link-kind="external">
            {children}
            <Icon name="arrowUpRight" size={11} className="cx-md-ext" />
          </a>
        );
      }
      if (onResourceLink && (target.kind === "workspace" || target.kind === "artifact" || target.kind === "message")) {
        return (
          <button
            type="button"
            className="cx-md-link cx-md-resource-link"
            data-link-kind={target.kind}
            data-testid={resourceTestId(target.kind)}
            onClick={(event) => {
              event.preventDefault();
              onResourceLink(target);
            }}
          >
            {target.kind === "workspace" ? <Icon name="fileCode" size={12} className="cx-md-resource-icon" /> : null}
            {children}
          </button>
        );
      }
      if (!safeHref) {
        return (
          <span className="cx-md-link cx-md-link-blocked" data-link-kind="blocked">
            {children}
          </span>
        );
      }
      return (
        <a {...props} href={safeHref} className="cx-md-link" data-link-kind={target.kind}>
          {children}
        </a>
      );
    },
    table({ children, node: _node, ...props }) {
      return (
        <div className="cx-md-table-wrap cx-scroll">
          <table {...props}>{children}</table>
        </div>
      );
    },
    img({ src, alt, node: _node, ...props }) {
      const safeSrc = typeof src === "string" ? safeMarkdownUrlTransform(src) : "";
      if (!safeSrc) return alt ? <span className="cx-md-img-alt">{alt}</span> : null;
      // eslint-disable-next-line @next/next/no-img-element
      return <img {...props} src={safeSrc} alt={alt || ""} loading="lazy" decoding="async" className="cx-md-img" />;
    },
    input({ type, checked, node: _node, ...props }) {
      if (type !== "checkbox") return <input type={type} {...props} />;
      return (
        <span className="cx-md-check" data-checked={checked ? "true" : "false"} role="img" aria-label={checked ? "已完成" : "未完成"}>
          {checked ? <Icon name="check" size={10} /> : null}
        </span>
      );
    },
  };
}

const MarkdownBlock = memo(function MarkdownBlock({ text, components, sourceStart }: { text: string; components: Components; sourceStart: number }) {
  return (
    <div className="cx-md-block" data-md-source-start={sourceStart} data-md-source-end={sourceStart + text.length}>
      <ReactMarkdown remarkPlugins={CHAT_REMARK_PLUGINS} components={components} urlTransform={safeMarkdownUrlTransform}>
        {text}
      </ReactMarkdown>
    </div>
  );
});

/** Chat-only markdown renderer with ChatGPT/Claude-grade prose (`.cx-prose`). */
export function ChatMarkdown({ text, streaming = false, threadId, messageId, onResourceLink, size = "md", className }: ChatMarkdownProps) {
  const { lang } = useLang();
  const resourceLinkRef = useRef(onResourceLink);
  resourceLinkRef.current = onResourceLink;
  const resourceLinks = Boolean(onResourceLink);
  const components = useMemo(
    () => buildComponents({ threadId, resourceLinkRef, resourceLinks, streamingTail: false }),
    [threadId, resourceLinks],
  );
  const tailComponents = useMemo(
    () => buildComponents({ threadId, resourceLinkRef, resourceLinks, streamingTail: true }),
    [threadId, resourceLinks],
  );
  const crossReferences = useMemo(() => hasCrossBlockReferences(text), [text]);
  const blocks = useMemo(() => splitMarkdownBlocks(text, !streaming), [text, streaming]);
  const sourceStarts = useMemo(() => { let offset = 0; return blocks.map(block => { const start = text.indexOf(block, offset); offset = start + block.length; return start; }); }, [blocks, text]);
  const tailOpenFence = streaming && blocks.length > 0 && endsInOpenFence(blocks[blocks.length - 1]);
  if (!text) {
    return streaming ? (
      <div className={cn("cx-prose", size === "sm" && "cx-prose-sm", className)} data-streaming="true">
        <span className="cx-caret" aria-hidden />
      </div>
    ) : null;
  }
  return (
    <div
      className={cn("cx-prose", size === "sm" && "cx-prose-sm", className)}
      data-streaming={streaming ? "true" : undefined}
    >
      {streaming && crossReferences ? <p className="text-[12px] text-cx-fg-4" role="status">{lang === "en" ? "Footnotes and cross-paragraph references will be linked when the answer finishes." : "脚注与跨段引用会在回答完成后连接。"}</p> : null}
      {blocks.map((block, index) => {
        const visual = threadId ? parseVisualization(block) : null;
        if (visual && threadId) return <ChatVisualization key={index} threadId={threadId} messageId={messageId} {...visual} streaming={streaming} />;
        return (
        <MarkdownBlock
          key={index}
          text={block}
          sourceStart={sourceStarts[index]}
          components={tailOpenFence && index === blocks.length - 1 ? tailComponents : components}
        />
        );
      })}
    </div>
  );
}
