/**
 * Detects locally served web apps in agent/terminal output so the chat can
 * offer "Open in preview". Only loopback/any-address hosts count: those are
 * the dev servers an agent starts inside the workspace.
 */

const LOCAL_URL_RE = /\bhttps?:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\])(?::\d{2,5})?(?:\/[^\s"'`<>)\]}]*)?/gi;
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007]*\u0007/g;

function tidy(raw: string): string {
  let url = raw.replace(/[.,;:!?]+$/, "");
  // 0.0.0.0 / :: are bind addresses; browsers need a concrete loopback host.
  url = url.replace(/^(https?:\/\/)(?:0\.0\.0\.0|\[::\])/i, "$1localhost");
  return url;
}

export function isLocalPreviewUrl(url: string): boolean {
  LOCAL_URL_RE.lastIndex = 0;
  const match = LOCAL_URL_RE.exec(url);
  LOCAL_URL_RE.lastIndex = 0;
  return Boolean(match && match.index === 0);
}

/** All distinct local URLs in `text`, in order of appearance. */
export function extractLocalUrls(text: string, limit = 6): string[] {
  if (!text) return [];
  const clean = text.replace(ANSI_RE, "");
  const seen = new Set<string>();
  const out: string[] = [];
  for (const match of clean.matchAll(LOCAL_URL_RE)) {
    const url = tidy(match[0]);
    const key = normalizePreviewUrl(url) ?? url;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(url);
    if (out.length >= limit) break;
  }
  return out;
}

export interface LocalUrlMatch {
  /** Openable URL (bind addresses rewritten to localhost, trailing punctuation dropped). */
  url: string;
  /** Offset of the match in `text`. */
  start: number;
  /** Length of the matched characters that belong to the URL. */
  length: number;
}

/**
 * Positional variant of `extractLocalUrls` for already-clean text (a terminal
 * buffer line): every match with its offsets, duplicates included.
 */
export function findLocalUrls(text: string): LocalUrlMatch[] {
  if (!text) return [];
  const out: LocalUrlMatch[] = [];
  for (const match of text.matchAll(LOCAL_URL_RE)) {
    const raw = match[0].replace(/[.,;:!?]+$/, "");
    out.push({ url: tidy(raw), start: match.index ?? 0, length: raw.length });
  }
  return out;
}

/** Address-bar input → absolute URL, or null when it can't be one. */
export function normalizePreviewUrl(input: string): string | null {
  const value = input.trim();
  if (!value) return null;
  if (/^[a-z]+:\/\//i.test(value)) {
    try {
      return new URL(tidy(value)).toString();
    } catch {
      return null;
    }
  }
  if (/^:\d{2,5}(\/.*)?$/.test(value)) return `http://localhost${value}`;
  if (/^\d{2,5}(\/.*)?$/.test(value)) return `http://localhost:${value}`;
  const loopback = /^(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(:\d+)?/i.test(value);
  try {
    return new URL(tidy(`${loopback ? "http" : "https"}://${value}`)).toString();
  } catch {
    return null;
  }
}

/** Host and the rest of the URL, for address bars that emphasize the host. */
export function splitPreviewUrl(url: string): { scheme: string; host: string; rest: string; secure: boolean } {
  try {
    const parsed = new URL(url);
    const path = parsed.pathname === "/" ? "" : parsed.pathname;
    return {
      scheme: `${parsed.protocol}//`,
      host: parsed.host,
      rest: `${path}${parsed.search}${parsed.hash}`,
      secure: parsed.protocol === "https:",
    };
  } catch {
    return { scheme: "", host: url, rest: "", secure: false };
  }
}

export function previewUrlLabel(url: string): string {
  try {
    const parsed = new URL(url);
    const path = parsed.pathname === "/" ? "" : parsed.pathname;
    return `${parsed.host}${path}${parsed.search}`;
  } catch {
    return url;
  }
}
