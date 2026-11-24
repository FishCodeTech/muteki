import katex from "katex";

/**
 * KaTeX engine facade: HTML-only + MathML output with strict caps and side-effect blocks.
 */
export function renderLatexToHtml(value: string, displayMode: boolean): string | null {
  try {
    return katex.renderToString(value, {
      displayMode,
      output: "htmlAndMathml",
      throwOnError: true,
      strict: "error",
      trust: false,
      maxExpand: 256,
      maxSize: 20,
      globalGroup: false,
      macros: {},
    });
  } catch {
    return null;
  }
}
