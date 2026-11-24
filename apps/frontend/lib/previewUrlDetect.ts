
const LOCAL_URL_RE = /\bhttps?:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\]|\[::\])(?::\d{2,5})?(?:\/[^\s"'`<>)\]}]*)?/gi;
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007]*\u0007/g;

function tidy(raw: string): string {
  let url = raw.replace(/[.,;:!?]+$/, "");
  url = url.replace(/^(https?:\/\/)(?:0\.0\.0\.0|\[::\])/i, "$1localhost");
  return url;
}

export function isLocalPreviewUrl(url: string): boolean {
  LOCAL_URL_RE.lastIndex = 0;
  const match = LOCAL_URL_RE.exec(url);
  LOCAL_URL_RE.lastIndex = 0;
  return Boolean(match && match.index === 0);
}

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
