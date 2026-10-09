"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { Button, IconButton, Kbd } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import type { DesktopPickedElement } from "@/lib/desktopChatBridge";

const TEXT_PREVIEW = 160;

/** Markdown block the agent receives alongside the cropped screenshot. */
export function formatPickedElement(element: DesktopPickedElement, note: string, screenshotName: string | null): string {
  const lines = [`**页面元素**：\`${element.selector}\`（<${element.tag}>，${Math.round(element.rect.width)}×${Math.round(element.rect.height)}）`];
  lines.push(`页面：${element.title ? `${element.title} — ` : ""}${element.url}`);
  if (note.trim()) lines.push(`说明：${note.trim()}`);
  if (element.text) lines.push(`文本：${element.text}`);
  const attrs = Object.entries(element.attributes);
  if (attrs.length) lines.push(`属性：${attrs.map(([key, value]) => `${key}="${value}"`).join(" ")}`);
  const styles = Object.entries(element.styles).filter(([, value]) => value && value !== "none" && value !== "normal" && value !== "0px");
  if (styles.length) lines.push(`样式：${styles.map(([key, value]) => `${key}: ${value}`).join("; ")}`);
  if (screenshotName) lines.push(`截图：附件 ${screenshotName}${element.screenshot_clipped ? "（元素超出视口，只截到可见部分）" : ""}`);
  if (element.html) {
    lines.push(
      element.html_truncated
        ? `HTML（共 ${element.html_length} 字符，以下为前 ${element.html.length} 字符）：`
        : "HTML：",
    );
    lines.push("```html", element.html, "```");
  }
  return lines.join("\n");
}

export function screenshotFile(element: DesktopPickedElement): File | null {
  if (!element.screenshot) return null;
  const binary = atob(element.screenshot.data);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  const safe = element.tag.replace(/[^a-z0-9-]/gi, "") || "element";
  return new File([bytes], `element-${safe}-${Date.now()}.png`, { type: "image/png" });
}

export function ElementPickingBar({ onCancel }: { onCancel: () => void }) {
  return (
    <div role="status" className="flex shrink-0 items-center gap-2 border-b border-cx-border-subtle bg-cx-accent-soft px-3 py-1.5 text-[12px] text-cx-fg-2">
      <Icon name="mousePointer" size={13} className="text-cx-accent" />
      <span className="min-w-0 flex-1 truncate">
        在页面中点击要选取的元素 · <Kbd>↑</Kbd> 选父元素 · <Kbd>Esc</Kbd> 取消
      </span>
      <Button size="xs" variant="ghost" onClick={onCancel}>取消</Button>
    </div>
  );
}

export function ElementPickStrip({
  element,
  onDiscard,
  onSubmit,
  onRepick,
}: {
  element: DesktopPickedElement;
  onDiscard: () => void;
  onSubmit: (note: string, send: boolean) => void;
  onRepick: () => void;
}) {
  const [note, setNote] = useState("");
  const noteRef = useRef<HTMLTextAreaElement>(null);
  const thumb = useMemo(
    () => (element.screenshot ? `data:${element.screenshot.mime};base64,${element.screenshot.data}` : ""),
    [element.screenshot],
  );
  useEffect(() => {
    noteRef.current?.focus();
  }, [element]);

  const text = element.text.length > TEXT_PREVIEW ? `${element.text.slice(0, TEXT_PREVIEW)}…` : element.text;

  return (
    <div className="cx-animate-in shrink-0 border-b border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5" data-testid="preview-element-pick">
      <div className="flex items-start gap-3">
        {thumb ? (
          // eslint-disable-next-line @next/next/no-img-element
          <img
            src={thumb}
            alt={`${element.tag} 截图`}
            className="max-h-20 max-w-[7.5rem] shrink-0 rounded-md border border-cx-border bg-white object-contain"
          />
        ) : (
          <span
            className="grid size-12 shrink-0 place-items-center rounded-md border border-dashed border-cx-border text-cx-fg-4"
            title={element.screenshot_error || "无截图"}
          >
            <Icon name="image" size={16} />
          </span>
        )}
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5">
            <code className="min-w-0 truncate rounded bg-cx-hover px-1.5 py-0.5 font-cx-mono text-[11.5px] text-cx-fg" title={element.selector}>
              {element.selector}
            </code>
            <span className="cx-tabular shrink-0 text-[11px] text-cx-fg-4">
              {Math.round(element.rect.width)}×{Math.round(element.rect.height)}
            </span>
            <span className="flex-1" />
            <IconButton icon="mousePointer" label="重新选取" size="sm" onClick={onRepick} />
            <IconButton icon="x" label="丢弃选取" size="sm" onClick={onDiscard} />
          </div>
          {text ? <p className="mt-1 line-clamp-2 text-[12px] leading-[18px] text-cx-fg-3" title={element.text}>{text}</p> : null}
          {element.screenshot_error ? <p className="mt-1 text-[11.5px] text-cx-warning">{element.screenshot_error}</p> : null}
        </div>
      </div>
      <textarea
        ref={noteRef}
        value={note}
        onChange={(event) => setNote(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Escape") { event.preventDefault(); onDiscard(); return; }
          if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
          event.preventDefault();
          onSubmit(note, event.metaKey || event.ctrlKey);
        }}
        rows={2}
        placeholder="补充说明（可选），例如：把这个按钮改成主色"
        aria-label="元素补充说明"
        className="mt-2 block w-full resize-none rounded-lg border border-cx-border bg-cx-elevated px-2.5 py-1.5 text-[13px] leading-5 text-cx-fg outline-none placeholder:text-cx-fg-4 focus:border-cx-accent"
      />
      <div className="mt-2 flex items-center justify-end gap-2">
        <Button size="xs" variant="secondary" onClick={() => onSubmit(note, false)}>
          附加到输入框 <Kbd>↵</Kbd>
        </Button>
        <Button size="xs" variant="primary" icon="send" onClick={() => onSubmit(note, true)}>
          直接发送 <Kbd>⌘↵</Kbd>
        </Button>
      </div>
    </div>
  );
}
