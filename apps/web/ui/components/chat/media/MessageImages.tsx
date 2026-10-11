"use client";

import { useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@/components/Icon";
import { apiFetch } from "@/lib/useRun";
import { artifactUrl } from "@/lib/resourcePreview";
import { authenticatedResourceHttpError, downloadAuthenticatedResource } from "@/lib/authenticatedResource";
import { toast } from "@/components/chat/ui";
import { useImageLightbox, type ViewerImage } from "./ImageViewer";

export interface MessageImageAttachment {
  sha256: string;
  name: string;
}

export function isImageAttachment(attachment: { mediaType?: string; name: string }): boolean {
  if (attachment.mediaType) return /^image\//i.test(attachment.mediaType);
  return /\.(png|jpe?g|gif|webp|bmp|avif|svg)$/i.test(attachment.name);
}

type Loaded = { url?: string; error?: string };

/** Fetches artifact bytes with auth and exposes them as object URLs. */
function useArtifactObjectUrls(threadId: string, items: MessageImageAttachment[]) {
  const key = items.map((item) => item.sha256).join(",");
  const [loaded, setLoaded] = useState<Record<string, Loaded>>({});
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    const urls: string[] = [];
    setLoaded({});
    for (const item of items) {
      void (async () => {
        try {
          const response = await apiFetch(artifactUrl(threadId, item.sha256), { cache: "no-store", signal: controller.signal });
          if (!response.ok) throw new Error(authenticatedResourceHttpError(response.status, "read"));
          const blob = await response.blob();
          if (controller.signal.aborted) return;
          const url = URL.createObjectURL(blob);
          urls.push(url);
          setLoaded((prev) => ({ ...prev, [item.sha256]: { url } }));
        } catch (error) {
          if (controller.signal.aborted) return;
          setLoaded((prev) => ({ ...prev, [item.sha256]: { error: error instanceof Error ? error.message : "读取图片失败" } }));
        }
      })();
    }
    return () => {
      controller.abort();
      urls.forEach((url) => URL.revokeObjectURL(url));
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [threadId, key, retry]);
  return { loaded, retry: () => setRetry((value) => value + 1) };
}

export function MessageImages({ threadId, items, className }: {
  threadId: string;
  items: MessageImageAttachment[];
  className?: string;
}) {
  const { loaded, retry } = useArtifactObjectUrls(threadId, items);
  const ready = useMemo(() => items
    .map((item) => ({ item, url: loaded[item.sha256]?.url }))
    .filter((entry): entry is { item: MessageImageAttachment; url: string } => Boolean(entry.url)), [items, loaded]);
  const viewerImages: ViewerImage[] = useMemo(() => ready.map(({ item, url }) => ({
    src: url,
    name: item.name,
    onDownload: () => {
      void downloadAuthenticatedResource(artifactUrl(threadId, item.sha256, { download: true }), item.name).then((result) => {
        if (!result.ok) toast({ title: "下载失败", description: result.message, tone: "danger" });
      });
    },
  })), [ready, threadId]);
  const lightbox = useImageLightbox(viewerImages);
  const single = items.length === 1;

  return (
    <div className={cn("flex flex-wrap justify-end gap-1.5", className)} data-testid="message-images">
      {items.map((item) => {
        const state = loaded[item.sha256];
        const viewerIndex = ready.findIndex((entry) => entry.item.sha256 === item.sha256);
        if (state?.error) {
          return (
            <button
              key={item.sha256}
              type="button"
              onClick={retry}
              title={`${item.name}：${state.error}，点击重试`}
              className="grid size-24 place-items-center rounded-2xl border border-cx-border-subtle bg-cx-sunken p-2 text-center text-[11px] leading-4 text-cx-fg-3"
            >
              <span className="flex flex-col items-center gap-1">
                <Icon name="circleAlert" size={16} />
                加载失败，重试
              </span>
            </button>
          );
        }
        return (
          <button
            key={item.sha256}
            type="button"
            aria-label={`查看图片 ${item.name}`}
            disabled={!state?.url}
            onClick={() => lightbox.openAt(viewerIndex)}
            className={cn(
              "cx-press overflow-hidden rounded-2xl bg-cx-sunken shadow-[0_0_0_1px_var(--cx-border-subtle)] focus-visible:outline focus-visible:outline-2 focus-visible:outline-cx-accent",
              single ? "max-h-[320px] max-w-[min(320px,100%)]" : "size-24",
              !state?.url && (single ? "h-40 w-56" : ""),
              state?.url ? "cursor-zoom-in" : "cursor-default",
            )}
          >
            {state?.url ? (
              // eslint-disable-next-line @next/next/no-img-element
              <img
                src={state.url}
                alt={item.name}
                draggable={false}
                className={single ? "block max-h-[320px] max-w-full object-contain" : "size-full object-cover"}
              />
            ) : (
              <span className="grid size-full place-items-center text-cx-fg-4">
                <Icon name="loader" size={16} className="animate-spin" />
              </span>
            )}
          </button>
        );
      })}
      {lightbox.element}
    </div>
  );
}
