"use client";

import { useEffect, useState, type RefObject } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { Badge, Button, Input, ScrollArea } from "@/components/chat/ui";
import { chatPanel, useDetectedServers, useRecentUrls, type DetectedServer } from "@/lib/chatPanelStore";
import { normalizePreviewUrl, previewUrlLabel } from "@/lib/previewUrlDetect";
import { relativeTime, SectionHeader, useNow } from "@/components/chat/surfaces/shared";

const SOURCE_LABEL: Record<DetectedServer["source"], string> = {
  terminal: "终端",
  tool: "工具",
  message: "消息",
};

const SOURCE_ICON: Record<DetectedServer["source"], "terminal" | "wrench" | "messageCircle"> = {
  terminal: "terminal",
  tool: "wrench",
  message: "messageCircle",
};

export function PreviewStart({
  threadId,
  active,
  inputRef,
  onNavigate,
}: {
  threadId: string;
  active: boolean;
  inputRef: RefObject<HTMLInputElement | null>;
  onNavigate: (url: string) => void;
}) {
  const [draft, setDraft] = useState("");
  const [invalid, setInvalid] = useState(false);
  const detected = useDetectedServers(threadId);
  const recent = useRecentUrls(threadId);
  const now = useNow(active);

  useEffect(() => {
    if (!active) return;
    const frame = requestAnimationFrame(() => inputRef.current?.focus({ preventScroll: true }));
    return () => cancelAnimationFrame(frame);
  }, [active, inputRef]);

  const submit = () => {
    const next = normalizePreviewUrl(draft);
    if (!next) { setInvalid(true); return; }
    onNavigate(next);
  };

  return (
    <ScrollArea className="flex-1">
      <div className="mx-auto flex w-full max-w-[480px] flex-col px-5 pb-10 pt-[max(32px,9vh)]">
        <div className="mb-5 flex flex-col items-center text-center">
          <span className="mb-3 grid size-11 place-items-center rounded-2xl bg-cx-hover text-cx-fg-2">
            <Icon name="globe" size={20} />
          </span>
          <h2 className="text-[15px] font-semibold tracking-[-0.01em] text-cx-fg">打开本地服务或网页</h2>
          <p className="mt-1 text-[12.5px] leading-5 text-cx-fg-3">在面板内预览正在开发的应用，与对话并排调试。</p>
        </div>

        <form
          onSubmit={(event) => { event.preventDefault(); submit(); }}
          className="flex items-center gap-2"
        >
          <Input
            ref={inputRef}
            size="lg"
            icon="globe"
            value={draft}
            invalid={invalid}
            spellCheck={false}
            autoCapitalize="off"
            aria-label="浏览器地址"
            placeholder="localhost:3000 或 https://…"
            onChange={(event) => { setDraft(event.target.value); setInvalid(false); }}
            className="min-w-0 flex-1"
          />
          <Button type="submit" variant="primary" size="lg" disabled={!draft.trim()} aria-label="打开地址">
            打开
          </Button>
        </form>
        {invalid ? <p className="mt-1.5 px-1 text-[12px] text-cx-danger">无法识别的地址</p> : null}

        <section className="mt-7">
          <SectionHeader title="检测到的本地服务" count={detected.length || undefined} />
          {detected.length ? (
            <div className="mt-1 flex flex-col gap-1.5" data-testid="preview-detected-servers">
              {detected.map((server) => (
                <div
                  key={server.url}
                  role="button"
                  tabIndex={0}
                  onClick={() => onNavigate(server.url)}
                  onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); onNavigate(server.url); } }}
                  className="cx-press group flex items-center gap-3 rounded-xl border border-cx-border-subtle bg-cx-elevated px-3 py-2.5 text-left shadow-cx-xs outline-none hover:border-cx-border hover:shadow-cx-sm focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"
                >
                  <span className="relative grid size-8 shrink-0 place-items-center rounded-lg bg-cx-success-soft text-cx-success">
                    <Icon name={SOURCE_ICON[server.source]} size={15} />
                    <span className="absolute -right-0.5 -top-0.5 size-2 rounded-full bg-cx-success ring-2 ring-[var(--cx-elevated)]" />
                  </span>
                  <span className="flex min-w-0 flex-1 flex-col">
                    <span className="truncate font-cx-mono text-[12.5px] font-medium text-cx-fg">{previewUrlLabel(server.url)}</span>
                    <span className="mt-0.5 flex items-center gap-1.5 text-[11.5px] text-cx-fg-4">
                      <Badge tone="neutral" className="h-[18px] px-1 text-[10.5px]">{SOURCE_LABEL[server.source]}</Badge>
                      <span>{relativeTime(server.seenAt, now)}</span>
                    </span>
                  </span>
                  <Button
                    size="sm"
                    variant="secondary"
                    iconRight="arrowRight"
                    tabIndex={-1}
                    onClick={(event) => { event.stopPropagation(); onNavigate(server.url); }}
                  >
                    打开
                  </Button>
                </div>
              ))}
            </div>
          ) : (
            <div className="mt-1 flex items-center gap-2.5 rounded-xl border border-dashed border-cx-border px-3 py-3 text-[12.5px] text-cx-fg-3">
              <span className="relative flex size-2 shrink-0">
                <span className="cx-pulse-dot absolute inset-0 rounded-full bg-cx-fg-4" />
              </span>
              Agent 或终端启动 localhost 服务后会自动出现在这里
            </div>
          )}
        </section>

        {recent.length ? (
          <section className="mt-6">
            <SectionHeader title="最近访问" />
            <ul className="mt-0.5 flex flex-col">
              {recent.map((item) => (
                <li key={item} className="group relative">
                  <button
                    type="button"
                    onClick={() => onNavigate(item)}
                    className={cn(
                      "flex h-8 w-full items-center gap-2.5 rounded-lg pl-2 pr-9 text-left text-[12.5px] text-cx-fg-2 outline-none transition-colors",
                      "hover:bg-cx-hover hover:text-cx-fg focus-visible:bg-cx-hover",
                    )}
                  >
                    <Icon name="history" size={13} className="shrink-0 text-cx-fg-4" />
                    <span className="truncate">{previewUrlLabel(item)}</span>
                  </button>
                  <button
                    type="button"
                    aria-label={`从最近访问中移除 ${previewUrlLabel(item)}`}
                    onClick={() => chatPanel.forgetUrl(threadId, item)}
                    className="absolute right-1 top-1 grid size-6 place-items-center rounded-md text-cx-fg-4 opacity-0 transition-opacity hover:bg-cx-active hover:text-cx-fg focus-visible:opacity-100 group-hover:opacity-100"
                  >
                    <Icon name="x" size={12} />
                  </button>
                </li>
              ))}
            </ul>
          </section>
        ) : null}
      </div>
    </ScrollArea>
  );
}
