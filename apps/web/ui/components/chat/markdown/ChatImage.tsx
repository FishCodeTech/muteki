"use client";

import { useEffect, useRef, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import { markdownImageApiPath } from "@/lib/resourcePreview";
import { ExpandableImage } from "@/components/chat/media/ImageViewer";

export function isRemoteMarkdownImage(src: string): boolean {
  return /^https?:\/\//i.test(src) || src.startsWith("//");
}

function imageName(src: string): string {
  const artifact = /^(?:artifact:|muteki-artifact:)/i.test(src);
  if (artifact) return "artifact image";
  try {
    const path = src.replace(/^file:\/\//i, "").split(/[?#]/, 1)[0];
    return decodeURIComponent(path.split("/").filter(Boolean).pop() || path || "image");
  } catch {
    return src.split("/").filter(Boolean).pop() || "image";
  }
}

export function ChatImage({ src, alt, threadId, className }: {
  src: string;
  alt: string;
  threadId?: string;
  className?: string;
}) {
  const [retry, setRetry] = useState(0);
  const [result, setResult] = useState<{ url?: string; error?: string }>({});
  const name = alt.trim() || imageName(src);
  const objectUrlRef = useRef<string | null>(null);

  useEffect(() => {
    if (!threadId) return;
    const controller = new AbortController();
    let objectUrl: string | undefined;
    setResult({});
    void (async () => {
      try {
        const response = await apiFetch(markdownImageApiPath(threadId, src), {
          cache: "no-store",
          signal: controller.signal,
        });
        if (!response.ok) {
          throw new Error(response.status === 401
            ? "登录已失效，请重新登录后重试"
            : `读取失败（HTTP ${response.status}）`);
        }
        const downloaded = await response.blob();
        // The image route serves SVG as an attachment/octet-stream to keep
        // direct navigation safe. Restore its type for passive image display.
        const imageType = response.headers.get("X-Muteki-Image-Media-Type");
        const blob = imageType === "image/svg+xml"
          ? new Blob([downloaded], { type: imageType })
          : downloaded;
        if (controller.signal.aborted) return;
        objectUrl = URL.createObjectURL(blob);
        objectUrlRef.current = objectUrl;
        setResult({ url: objectUrl });
      } catch (error) {
        if (controller.signal.aborted) return;
        setResult({ error: error instanceof Error ? error.message : "读取图片失败" });
      }
    })();
    return () => {
      controller.abort();
      if (objectUrl) {
        URL.revokeObjectURL(objectUrl);
        if (objectUrlRef.current === objectUrl) objectUrlRef.current = null;
      }
    };
  }, [src, threadId, retry]);

  if (!threadId) {
    return <span className="cx-md-img-error" role="group" aria-label={`图片 ${name} 加载失败`}>
      <span>图片 {name} 加载失败：当前消息没有可用的会话 ID</span>
    </span>;
  }
  if (result.url) {
    return <ExpandableImage src={result.url} alt={alt} name={name} className={className} onError={() => {
      const url = objectUrlRef.current;
      if (url) URL.revokeObjectURL(url);
      objectUrlRef.current = null;
      setResult({ error: "图片解码失败或格式不受支持" });
    }} />;
  }
  if (result.error) {
    return (
      <span className="cx-md-img-error" role="group" aria-label={`图片 ${name} 加载失败`}>
        <span>图片 {name} 加载失败：{result.error}</span>
        <button type="button" onClick={() => setRetry((value) => value + 1)}>重试</button>
      </span>
    );
  }
  return <span className="cx-md-img-loading" role="status">正在加载图片 {name}…</span>;
}
