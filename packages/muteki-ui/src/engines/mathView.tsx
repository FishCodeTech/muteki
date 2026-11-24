"use client";

import "katex/dist/katex.min.css";
import { renderLatexToHtml } from "./math";

export interface MathFormulaProps {
  value: string;
  source: string;
  display: boolean;
  /** Optional pre-rendered HTML from caller-side validation/budget guards. */
  renderedHtml?: string | null;
  fallbackTitle?: string;
  title?: string;
}

/** Shared KaTeX display/inline presentation wrapper owning KaTeX stylesheet loading. */
export function MathFormula({
  value,
  source,
  display,
  renderedHtml,
  fallbackTitle = "公式未完成或无法渲染，保留 LaTeX 原文",
  title = "LaTeX 公式（$…$ / $$…$$）；复制消息可获得原文",
}: MathFormulaProps) {
  const html = renderedHtml !== undefined ? renderedHtml : renderLatexToHtml(value, display);
  const Tag = display ? "div" : "span";
  if (!html) {
    return (
      <Tag className="cx-math-fallback" data-math-display={display || undefined} title={fallbackTitle}>
        {source}
      </Tag>
    );
  }
  return (
    <Tag
      className={display ? "cx-math-display cx-scroll" : "cx-math-inline"}
      data-latex-source={source}
      title={title}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
}
