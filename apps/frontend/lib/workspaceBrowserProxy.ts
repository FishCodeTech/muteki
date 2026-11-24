
export const WORKSPACE_BROWSER_PROXY_PREFIX = "/api/workspace-browser-proxy";

const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]", "::1"]);

export function isLoopbackBrowserTarget(url: string): boolean {
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return false;
    return LOOPBACK.has(parsed.hostname.toLowerCase());
  } catch {
    return false;
  }
}

export function buildWorkspaceBrowserProxyPath(targetUrl: string, ticket?: string): string | null {
  if (!isLoopbackBrowserTarget(targetUrl)) return null;
  let parsed: URL;
  try {
    parsed = new URL(targetUrl);
  } catch {
    return null;
  }
  const scheme = parsed.protocol.replace(/:$/, "");
  const host = parsed.host;
  let path = parsed.pathname.startsWith("/") ? parsed.pathname : `/${parsed.pathname}`;
  const search = parsed.search || "";
  const hash = parsed.hash || "";
  if (path === "/") path = "";
  return `${WORKSPACE_BROWSER_PROXY_PREFIX}/${ticket ? `${ticket}/` : ""}${scheme}/${host}${path}${search}${hash}`;
}

export type ParsedProxyTarget =
  | { kind: "ok"; url: URL }
  | { kind: "error"; status: number; detail: string };

export function parseWorkspaceBrowserProxyPath(segments: string[]): ParsedProxyTarget {
  if (segments.length < 2) {
    return { kind: "error", status: 400, detail: "expected /{http|https}/{host}/..." };
  }
  const scheme = segments[0]?.toLowerCase();
  if (scheme !== "http" && scheme !== "https") {
    return { kind: "error", status: 400, detail: "scheme must be http or https" };
  }
  const host = segments[1];
  if (!host) {
    return { kind: "error", status: 400, detail: "missing host" };
  }
  const rawRest = segments.slice(2).join("/");
  const path = rawRest ? `/${rawRest}` : "/";
  try {
    const url = new URL(`${scheme}://${host}${path}`);
    if (!isLoopbackBrowserTarget(url.toString())) {
      return { kind: "error", status: 403, detail: "only loopback targets are proxied" };
    }
    return { kind: "ok", url };
  } catch {
    return { kind: "error", status: 400, detail: "invalid target URL" };
  }
}

export function unwrapWorkspaceBrowserProxyHref(href: string): string {
  try {
    const parsed = new URL(href, "http://127.0.0.1");
    const prefix = WORKSPACE_BROWSER_PROXY_PREFIX + "/";
    if (!parsed.pathname.startsWith(prefix)) return href;
    const rest = parsed.pathname.slice(prefix.length);
    const parts = rest.split("/").filter(Boolean);
    if (parts[0]?.startsWith("v1.")) parts.shift();
    const result = parseWorkspaceBrowserProxyPath(parts);
    if (result.kind !== "ok") return href;
    result.url.search = parsed.search;
    result.url.hash = parsed.hash;
    return result.url.toString();
  } catch {
    return href;
  }
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function rewriteProxiedHtml(html: string, target: URL, ticket: string): string {
  const scheme = target.protocol.replace(/:$/, "");
  const hostRoot = `${WORKSPACE_BROWSER_PROXY_PREFIX}/${ticket}/${scheme}/${target.host}`;
  const dirPath = target.pathname.endsWith("/")
    ? target.pathname
    : target.pathname.replace(/[^/]*$/, "");
  const baseHref = `${hostRoot}${dirPath || "/"}`;

  let out = html;
  out = out.replace(
    /(\s(?:href|src|action)\s*=\s*["'])\/(?!\/)/gi,
    `$1${hostRoot}/`,
  );
  out = out.replace(
    new RegExp(
      `(\\s(?:href|src|action)\\s*=\\s*["'])${escapeRegExp(target.origin)}\\/`,
      "gi",
    ),
    `$1${hostRoot}/`,
  );

  if (/<base\s/i.test(out)) {
    out = out.replace(/<base\s[^>]*>/i, `<base href="${baseHref}">`);
  } else if (/<head[^>]*>/i.test(out)) {
    out = out.replace(/<head([^>]*)>/i, `<head$1><base href="${baseHref}">`);
  } else {
    out = `<head><base href="${baseHref}"></head>${out}`;
  }
  return out;
}

export function rewriteProxiedRedirectLocation(locationHeader: string, requestUrl: URL, ticket: string): string | null {
  try {
    const absolute = new URL(locationHeader, requestUrl);
    if (absolute.origin !== requestUrl.origin) return null;
    return buildWorkspaceBrowserProxyPath(absolute.toString(), ticket);
  } catch {
    return null;
  }
}
