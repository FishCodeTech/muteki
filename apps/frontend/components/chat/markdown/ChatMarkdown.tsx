"use client";

import {
  Children,
  Fragment,
  isValidElement,
  memo,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactElement,
  type ReactNode,
  type RefObject,
} from "react";
import { jsx, jsxs } from "react/jsx-runtime";
import type { Element as HastElement, Parents as HastParents, Root as HastRoot } from "hast";
import {
  remarkGfm,
  remarkMath,
  remarkParse,
  remarkRehype,
  toJsxRuntime,
  unified,
  type Components,
  type PluggableList,
  type UrlTransform,
} from "@muteki/ui/engines/markdown";
import { urlAttributes } from "html-url-attributes";
import { visit } from "unist-util-visit";
import { VFile } from "vfile";
import { remarkChatMath } from "@/lib/chatMath";
import { closeOpenLink, closeOpenTable, rehypeStreamWords } from "@/lib/rehypeStreamWords";
import { ChatMath } from "./ChatMath";
import { ChatVisualization, parseVisualization, splitVisualizationMarkers } from "./ChatVisualization";
import { cn } from "@/lib/cn";
import { useLang } from "@/lib/i18n";
import { hasCrossBlockReferences, splitMarkdownBlocks } from "@/lib/chatMarkdownBlocks";
import { rehypeMessageSource } from "@/lib/rehypeMessageSource";
export { splitMarkdownBlocks } from "@/lib/chatMarkdownBlocks";
import { Icon } from "@muteki/ui";
import { CodeBlock, languageLabel } from "@muteki/ui/agent";
import { normalizeLanguage } from "@muteki/ui/engines/shiki";
import {
  parseResourceLink,
  safeMarkdownUrlTransform,
  type ResourceLinkTarget,
} from "@/lib/resourcePreview";
import { isLocalPreviewUrl } from "@/lib/previewUrlDetect";
import { openLocalUrl } from "@/components/chat/timeline/LocalUrlChips";
import { ChatImage, isRemoteMarkdownImage } from "./ChatImage";
import { INLINE_IMAGE_SRC } from "@/lib/reportMarkdown";
import { ExpandableImage } from "@/components/chat/media/ImageViewer";

export interface ChatMarkdownProps {
  text: string;
  streaming?: boolean;
  threadId?: string;
  messageId?: string;
  visualizationSources?: VisualizationSources;
  onResourceLink?: (target: ResourceLinkTarget) => void;
  size?: "md" | "sm";
  className?: string;
  inlineImages?: boolean;
  onExpandImage?: (src: string, name: string) => void;
  annotateSourceOffsets?: boolean;
}

export interface VisualizationSources {
  spans: readonly { messageId: string; start: number; end: number; sourceStart?: number }[];
  messages?: readonly { message_id: string; text: string }[];
}

function inlinePathAt(text: string, sourceStart: number, displayOffset: number): string | undefined {
  let offset = displayOffset;
  if (sourceStart) {
    let remaining = sourceStart;
    for (const character of text) {
      if (remaining-- === 0) break;
      offset += character.length;
    }
  }
  const fences = /^[ \t]*(`{3,}|~{3,})muteki-visualize[ \t]*\r?\n[\s\S]*?^\1[ \t]*\r?$/gm;
  let ordinal = 0;
  for (const match of text.matchAll(fences)) {
    const start = match.index + match[0].search(/[^ \t]/);
    if (start >= offset) return start === offset ? `inline:${ordinal}` : undefined;
    ordinal++;
  }
}

export const CHAT_REMARK_PLUGINS: PluggableList = [remarkGfm, remarkMath, remarkChatMath];
const CHAT_REMARK_PLUGINS_NO_MATH: PluggableList = [remarkGfm];
const EMPTY_PLUGINS: PluggableList = [];
const SOURCE_PLUGINS: PluggableList = [rehypeMessageSource];
const STREAM_WORD_PLUGINS: PluggableList = [rehypeMessageSource, rehypeStreamWords];
const WORD_PLUGINS: PluggableList = [rehypeStreamWords];
const WORD_REVEAL_LINGER_MS = 400;

function useLinger(value: boolean, ms: number): boolean {
  const [held, setHeld] = useState(value);
  useEffect(() => {
    if (value) {
      setHeld(true);
      return;
    }
    if (!held) return;
    const timer = window.setTimeout(() => setHeld(false), ms);
    return () => window.clearTimeout(timer);
  }, [value, held, ms]);
  return value || held;
}
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

function resourceTestId(kind: "workspace" | "artifact" | "legacy_artifact" | "message"): string {
  switch (kind) {
    case "workspace":
      return "markdown-workspace-link";
    case "artifact":
    case "legacy_artifact":
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
  inlineImages: boolean;
  onExpandImage?: (src: string, name: string) => void;
}): Components {
  const { threadId, resourceLinkRef, resourceLinks, streamingTail, inlineImages, onExpandImage } = options;
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
        <div data-md-local-start={node?.position?.start.offset} data-md-local-end={node?.position?.end.offset} data-md-code={code}><CodeBlock
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
    code({ className, children, node, ...props }) {
      return (
        <code className={cn("cx-md-inline-code", className)} {...props} data-md-text-start={node?.position?.start.offset} data-md-text-end={node?.position?.end.offset}>
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
      if (onResourceLink && (target.kind === "workspace" || target.kind === "artifact" || target.kind === "legacy_artifact" || target.kind === "message")) {
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
    img({ src, alt }) {
      const safeSrc = typeof src === "string" ? (inlineImages && INLINE_IMAGE_SRC.test(src) ? src : safeMarkdownUrlTransform(src)) : "";
      if (!safeSrc) return alt ? <span className="cx-md-img-alt">{alt}</span> : null;
      if (!isRemoteMarkdownImage(safeSrc) && !(inlineImages && INLINE_IMAGE_SRC.test(safeSrc))) {
        return <ChatImage src={safeSrc} alt={alt || ""} threadId={threadId} className="cx-md-img" />;
      }
      return <ExpandableImage src={safeSrc} alt={alt || ""} className="cx-md-img" onExpand={onExpandImage} />;
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

const inlineImageUrlTransform: UrlTransform = (url, key, node) =>
  key === "src" && node.tagName === "img" && INLINE_IMAGE_SRC.test(url) ? url : safeMarkdownUrlTransform(url);

const REMARK_REHYPE_OPTIONS = { allowDangerousHtml: true } as const;
const MAX_HAST_CACHE_CHARS = 120_000;
const MAX_HAST_CACHE_ENTRIES = 400;

function buildMarkdownProcessor(hasMath: boolean, annotateSourceOffsets: boolean, words: boolean) {
  const remarkPlugins = hasMath ? CHAT_REMARK_PLUGINS : CHAT_REMARK_PLUGINS_NO_MATH;
  const rehypePlugins = annotateSourceOffsets
    ? (words ? STREAM_WORD_PLUGINS : SOURCE_PLUGINS)
    : (words ? WORD_PLUGINS : EMPTY_PLUGINS);
  return unified()
    .use(remarkParse)
    .use(remarkPlugins)
    .use(remarkRehype, REMARK_REHYPE_OPTIONS)
    .use(rehypePlugins)
    .freeze();
}

type MarkdownProcessor = ReturnType<typeof buildMarkdownProcessor>;

const processorCache = new Map<string, MarkdownProcessor>();
const hastCache = new Map<string, HastRoot>();

function getMarkdownProcessor(hasMath: boolean, annotateSourceOffsets: boolean, words: boolean): MarkdownProcessor {
  const key = `${hasMath ? 1 : 0}:${annotateSourceOffsets ? 1 : 0}:${words ? 1 : 0}`;
  let processor = processorCache.get(key);
  if (!processor) {
    processor = buildMarkdownProcessor(hasMath, annotateSourceOffsets, words);
    processorCache.set(key, processor);
  }
  return processor;
}

type RawHastNode = { type: "raw"; value: string };

function sanitizeHastTree(tree: HastRoot, urlTransform: UrlTransform): void {
  visit(tree, (node, index, parent) => {
    const nodeType = (node as { type: string }).type;
    if (nodeType === "raw" && parent && typeof index === "number") {
      const rawNode = node as unknown as RawHastNode;
      (parent as HastParents).children[index] = { type: "text", value: rawNode.value };
      return index;
    }
    if (node.type === "element" && node.properties) {
      const element = node as HastElement;
      for (const key in urlAttributes) {
        if (Object.hasOwn(urlAttributes, key) && Object.hasOwn(element.properties, key)) {
          const value = element.properties[key];
          const tags = urlAttributes[key];
          if (tags === null || tags.includes(element.tagName)) {
            element.properties[key] = urlTransform(String(value || ""), key, element);
          }
        }
      }
    }
  });
}

function parseMarkdownToHast(
  text: string,
  options: { annotateSourceOffsets: boolean; words: boolean; inlineImages: boolean },
): HastRoot {
  const { annotateSourceOffsets, words, inlineImages } = options;
  const cacheable = !words && text.length <= MAX_HAST_CACHE_CHARS;
  const cacheKey = cacheable ? `${annotateSourceOffsets ? 1 : 0}:${inlineImages ? 1 : 0}\0${text}` : "";
  if (cacheable) {
    const cached = hastCache.get(cacheKey);
    if (cached) {
      hastCache.delete(cacheKey);
      hastCache.set(cacheKey, cached);
      return cached;
    }
  }
  const processor = getMarkdownProcessor(text.includes("$"), annotateSourceOffsets, words);
  const file = new VFile(text);
  const tree = processor.runSync(processor.parse(file), file) as HastRoot;
  sanitizeHastTree(tree, inlineImages ? inlineImageUrlTransform : safeMarkdownUrlTransform);
  if (cacheable) {
    hastCache.set(cacheKey, tree);
    if (hastCache.size > MAX_HAST_CACHE_ENTRIES) {
      const oldestKey = hastCache.keys().next().value;
      if (oldestKey !== undefined) hastCache.delete(oldestKey);
    }
  }
  return tree;
}

type HastJsx = NonNullable<Parameters<typeof toJsxRuntime>[1]["jsx"]>;

function estimateMarkdownBlockHeightPx(text: string): number {
  if (!text) return 32;
  let height = 0;
  let fenceChar = "";
  let fenceSize = 0;
  let fenceLines = 0;
  let start = 0;
  const total = text.length;
  while (start <= total) {
    const nl = text.indexOf("\n", start);
    const end = nl === -1 ? total : nl;
    let lineEnd = end;
    if (lineEnd > start && text.charCodeAt(lineEnd - 1) === 13) lineEnd -= 1;

    let leading = start;
    while (leading < lineEnd && leading - start < 3 && text.charCodeAt(leading) === 32) {
      leading += 1;
    }
    const firstNonIndent = leading < lineEnd ? text.charCodeAt(leading) : -1;
    let markerLen = 0;
    if (firstNonIndent === 96 || firstNonIndent === 126) {
      let cursor = leading;
      while (cursor < lineEnd && text.charCodeAt(cursor) === firstNonIndent) cursor += 1;
      if (cursor - leading >= 3) markerLen = cursor - leading;
    }

    if (markerLen >= 3) {
      const markerChar = text[leading];
      const restTrimmed = text.slice(leading + markerLen, lineEnd).trim();
      if (!fenceChar) {
        if (firstNonIndent !== 96 || !restTrimmed.includes("`")) {
          fenceChar = markerChar;
          fenceSize = markerLen;
          fenceLines = 0;
        }
      } else if (markerChar === fenceChar && markerLen >= fenceSize && restTrimmed === "") {
        height += Math.min(600, 64 + fenceLines * 20);
        fenceChar = "";
        fenceSize = 0;
        fenceLines = 0;
      } else {
        fenceLines += 1;
      }
    } else if (fenceChar) {
      fenceLines += 1;
    } else {
      let ws = start;
      while (ws < lineEnd) {
        const code = text.charCodeAt(ws);
        if (code !== 32 && code !== 9) break;
        ws += 1;
      }
      if (ws >= lineEnd) {
        height += 12;
      } else {
        const ch = text.charCodeAt(ws);
        if (ch === 35) height += 44;
        else if (ch === 124) height += 38;
        else height += Math.max(1, Math.ceil((lineEnd - start) / 62)) * 25.5;
      }
    }

    if (nl === -1) break;
    start = nl + 1;
  }
  if (fenceChar) {
    height += Math.min(600, 64 + fenceLines * 20);
  }
  return Math.round(Math.min(8000, Math.max(32, height)));
}

const MarkdownBlock = memo(function MarkdownBlock({
  text, components, sourceStart, words, inlineImages, annotateSourceOffsets, deferOffscreen = false,
}: {
  text: string;
  components: Components;
  sourceStart: number;
  words: boolean;
  inlineImages: boolean;
  annotateSourceOffsets: boolean;
  deferOffscreen?: boolean;
}) {
  const rendered = useMemo(
    () => toJsxRuntime(
      parseMarkdownToHast(text, { annotateSourceOffsets, words, inlineImages }),
      {
        Fragment,
        components: components as Parameters<typeof toJsxRuntime>[1]["components"],
        ignoreInvalidStyle: true,
        jsx: jsx as HastJsx,
        jsxs: jsxs as HastJsx,
        passKeys: true,
        passNode: true,
      },
    ),
    [text, annotateSourceOffsets, words, inlineImages, components],
  );
  const intrinsicHeight = useMemo(
    () => (deferOffscreen ? estimateMarkdownBlockHeightPx(text) : 0),
    [deferOffscreen, text],
  );
  return (
    <div
      className={deferOffscreen ? "cx-md-block cx-md-block-deferred" : "cx-md-block"}
      style={deferOffscreen ? { containIntrinsicBlockSize: `auto ${intrinsicHeight}px` } : undefined}
      data-md-source-start={sourceStart}
      data-md-source-end={sourceStart + text.length}
    >
      {rendered}
    </div>
  );
});

export const ChatMarkdown = memo(function ChatMarkdown({ text, streaming = false, threadId, messageId, visualizationSources, onResourceLink, size = "md", className, inlineImages = false, onExpandImage, annotateSourceOffsets = true }: ChatMarkdownProps) {
  const { lang } = useLang();
  const resourceLinkRef = useRef(onResourceLink);
  resourceLinkRef.current = onResourceLink;
  const resourceLinks = Boolean(onResourceLink);
  const components = useMemo(
    () => buildComponents({ threadId, resourceLinkRef, resourceLinks, streamingTail: false, inlineImages, onExpandImage }),
    [threadId, resourceLinks, inlineImages, onExpandImage],
  );
  const tailComponents = useMemo(
    () => buildComponents({ threadId, resourceLinkRef, resourceLinks, streamingTail: true, inlineImages, onExpandImage }),
    [threadId, resourceLinks, inlineImages, onExpandImage],
  );
  const crossReferences = useMemo(() => streaming && hasCrossBlockReferences(text), [streaming, text]);
  const source = useMemo(() => (streaming ? closeOpenTable(closeOpenLink(text)) : text), [text, streaming]);
  const blocks = useMemo(() => splitMarkdownBlocks(source, !streaming), [source, streaming]);
  const sourceStarts = useMemo(() => { let offset = 0; return blocks.map(block => { const start = source.indexOf(block, offset); offset = start + block.length; return start; }); }, [blocks, source]);
  const tailOpenFence = streaming && blocks.length > 0 && endsInOpenFence(blocks[blocks.length - 1]);
  const words = useLinger(streaming, WORD_REVEAL_LINGER_MS);
  const deferOffscreenBlocks = !streaming && blocks.length >= 6 && source.length >= 3000;
  const inlineOrdinals = new Map<string | undefined, number>();
  const sourceAt = (offset: number) => visualizationSources?.spans.find(span => span.start <= offset && offset < span.end);
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
      data-words={words || undefined}
    >
      {streaming && crossReferences ? <p className="text-cx-xs text-cx-fg-4" role="status">{lang === "en" ? "Footnotes and cross-paragraph references will be linked when the answer finishes." : "脚注与跨段引用会在回答完成后连接。"}</p> : null}
      {blocks.map((block, index) => {
        const visual = threadId ? parseVisualization(block) : null;
        if (visual && threadId) {
          const offset = sourceStarts[index] + block.length - block.trimStart().length;
          const span = sourceAt(offset);
          const owner = span?.messageId ?? messageId;
          let path: string | undefined = visual.path;
          if (visual.inlineHtml !== undefined) {
            const ordinal = inlineOrdinals.get(owner) ?? 0;
            inlineOrdinals.set(owner, ordinal + 1);
            path = `inline:${ordinal}`;
            const message = span && visualizationSources?.messages?.find(item => item.message_id === span.messageId);
            if (span && message) path = inlinePathAt(message.text, span.sourceStart ?? 0, offset - span.start);
          }
          if (path) return <ChatVisualization key={index} threadId={threadId} messageId={owner} {...visual} path={path} streaming={streaming} />;
        }
        const blockComponents = tailOpenFence && index === blocks.length - 1 ? tailComponents : components;
        const segments = threadId ? splitVisualizationMarkers(block) : [{ text: block, offset: 0 }];
        return segments.map((segment, part) => "visual" in segment
          ? <ChatVisualization key={`${index}:${part}`} threadId={threadId!} messageId={sourceAt(sourceStarts[index] + segment.offset)?.messageId ?? messageId} {...segment.visual} streaming={streaming} />
          : (
            <MarkdownBlock
              key={`${index}:${part}`}
              text={segment.text}
              sourceStart={sourceStarts[index] + segment.offset}
              words={words && index >= blocks.length - 2 && part === segments.length - 1}
              inlineImages={inlineImages}
              annotateSourceOffsets={annotateSourceOffsets}
              deferOffscreen={deferOffscreenBlocks}
              components={blockComponents}
            />
          ));
      })}
    </div>
  );
});
