"use client";

import { useEffect, useState } from "react";
import type { HighlighterCore, ThemedToken } from "shiki/core";

/**
 * Shared Shiki highlighter for the chat workbench (code blocks, diffs, file
 * previews). Fine-grained: the core, the Oniguruma WASM engine and each
 * grammar load lazily on first use. The JS regex engine is avoided on
 * purpose — some grammars can hang it on pathological input.
 */

export interface CodeToken {
  content: string;
  light?: string;
  dark?: string;
  fontStyle?: number;
}

export type CodeLines = CodeToken[][];

const LANG_LOADERS: Record<string, () => Promise<unknown>> = {
  typescript: () => import("shiki/langs/typescript.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsonc: () => import("shiki/langs/jsonc.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  bash: () => import("shiki/langs/bash.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  kotlin: () => import("shiki/langs/kotlin.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  csharp: () => import("shiki/langs/csharp.mjs"),
  php: () => import("shiki/langs/php.mjs"),
  ruby: () => import("shiki/langs/ruby.mjs"),
  lua: () => import("shiki/langs/lua.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  vue: () => import("shiki/langs/vue.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  ini: () => import("shiki/langs/ini.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  docker: () => import("shiki/langs/docker.mjs"),
  makefile: () => import("shiki/langs/makefile.mjs"),
  nginx: () => import("shiki/langs/nginx.mjs"),
  powershell: () => import("shiki/langs/powershell.mjs"),
  graphql: () => import("shiki/langs/graphql.mjs"),
  solidity: () => import("shiki/langs/solidity.mjs"),
  asm: () => import("shiki/langs/asm.mjs"),
};

const ALIASES: Record<string, string> = {
  ts: "typescript", mts: "typescript", cts: "typescript",
  js: "javascript", mjs: "javascript", cjs: "javascript", node: "javascript",
  py: "python", python3: "python", py3: "python",
  sh: "bash", shell: "bash", zsh: "bash", console: "bash", shellscript: "bash", terminal: "bash",
  rs: "rust", golang: "go", kt: "kotlin", kts: "kotlin",
  "c++": "cpp", cc: "cpp", cxx: "cpp", hpp: "cpp", h: "c", cs: "csharp", "c#": "csharp",
  rb: "ruby", yml: "yaml", md: "markdown", mdx: "markdown", htm: "html", svg: "xml",
  dockerfile: "docker", make: "makefile", mk: "makefile", ps1: "powershell", ps: "powershell",
  gql: "graphql", sol: "solidity", patch: "diff", conf: "ini", cfg: "ini", env: "ini",
  jsonl: "json", json5: "jsonc", s: "asm", nasm: "asm",
};

export function normalizeLanguage(lang: string | null | undefined): string | null {
  if (!lang) return null;
  const key = lang.trim().toLowerCase().replace(/^language-/, "");
  const resolved = ALIASES[key] ?? key;
  return LANG_LOADERS[resolved] ? resolved : null;
}

export function languageFromPath(path: string | null | undefined): string | null {
  if (!path) return null;
  const base = path.split("/").pop() || path;
  const lower = base.toLowerCase();
  if (lower === "dockerfile" || lower.startsWith("dockerfile.")) return "docker";
  if (lower === "makefile") return "makefile";
  if (lower.endsWith(".d.ts")) return "typescript";
  const ext = lower.includes(".") ? lower.split(".").pop() || "" : "";
  return normalizeLanguage(ext);
}

let highlighterPromise: Promise<HighlighterCore> | null = null;
const loadedLangs = new Set<string>();
const pendingLangs = new Map<string, Promise<void>>();

async function getHighlighter(): Promise<HighlighterCore> {
  if (!highlighterPromise) {
    highlighterPromise = (async () => {
      const [{ createHighlighterCore }, { createOnigurumaEngine }] = await Promise.all([
        import("shiki/core"),
        import("shiki/engine/oniguruma"),
      ]);
      return createHighlighterCore({
        themes: [import("shiki/themes/github-light-default.mjs"), import("shiki/themes/github-dark-default.mjs")],
        langs: [],
        engine: createOnigurumaEngine(import("shiki/wasm")),
      });
    })();
  }
  return highlighterPromise;
}

async function ensureLanguage(highlighter: HighlighterCore, lang: string): Promise<void> {
  if (loadedLangs.has(lang)) return;
  let pending = pendingLangs.get(lang);
  if (!pending) {
    pending = (async () => {
      const mod = (await LANG_LOADERS[lang]()) as { default: Parameters<HighlighterCore["loadLanguage"]>[0] };
      await highlighter.loadLanguage(mod.default);
      loadedLangs.add(lang);
    })();
    pendingLangs.set(lang, pending);
  }
  await pending;
}

const MAX_HIGHLIGHT_CHARS = 200_000;
const cache = new Map<string, CodeLines>();

function toLines(tokens: ThemedToken[][]): CodeLines {
  return tokens.map((line) => line.map((token) => {
    const style = (token.htmlStyle || {}) as Record<string, string>;
    return {
      content: token.content,
      light: style.color || token.color,
      dark: style["--shiki-dark"],
      fontStyle: token.fontStyle,
    };
  }));
}

/** Tokenizes `code`; resolves null when the language is unknown or input is too large. */
export async function highlightCode(code: string, language: string | null | undefined): Promise<CodeLines | null> {
  const lang = normalizeLanguage(language);
  if (!lang || code.length > MAX_HIGHLIGHT_CHARS) return null;
  const key = `${lang}\u0000${code}`;
  const hit = cache.get(key);
  if (hit) return hit;
  try {
    const highlighter = await getHighlighter();
    await ensureLanguage(highlighter, lang);
    const result = highlighter.codeToTokens(code, {
      lang,
      themes: { light: "github-light-default", dark: "github-dark-default" },
      defaultColor: "light",
    });
    const lines = toLines(result.tokens);
    if (cache.size > 200) cache.delete(cache.keys().next().value as string);
    cache.set(key, lines);
    return lines;
  } catch {
    return null;
  }
}

/** React hook: plain lines render immediately, tokens swap in once ready. */
export function useHighlightedCode(code: string, language: string | null | undefined, enabled = true): CodeLines | null {
  const [lines, setLines] = useState<CodeLines | null>(null);
  useEffect(() => {
    if (!enabled) { setLines(null); return; }
    let cancelled = false;
    const handle = window.setTimeout(() => {
      void highlightCode(code, language).then((next) => { if (!cancelled) setLines(next); });
    }, 0);
    return () => { cancelled = true; window.clearTimeout(handle); };
  }, [code, language, enabled]);
  return lines;
}

export const FONT_STYLE_ITALIC = 1;
export const FONT_STYLE_BOLD = 2;
export const FONT_STYLE_UNDERLINE = 4;
