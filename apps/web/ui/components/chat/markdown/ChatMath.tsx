"use client";

import { useMemo } from "react";
import { renderChatMath } from "@/lib/chatMath";
import "katex/dist/katex.min.css";

export function ChatMath({ value, source, display }: { value: string; source: string; display: boolean }) {
  const html = useMemo(() => renderChatMath(value, display), [value, display]);
  const Tag = display ? "div" : "span";
  if (!html) {
    return <Tag className="cx-math-fallback" data-math-display={display || undefined} title="公式未完成或无法渲染，保留 LaTeX 原文">{source}</Tag>;
  }
  return <Tag className={display ? "cx-math-display cx-scroll" : "cx-math-inline"} data-latex-source={source} title="LaTeX 公式（$…$ / $$…$$）；复制消息可获得原文" dangerouslySetInnerHTML={{ __html: html }} />;
}
