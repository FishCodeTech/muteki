/**
 * Localhost-only workspace browser preview proxy (#140).
 *
 * Chat UI and a user fixture on another port are cross-origin, so
 * contentWindow.location is unreadable. For loopback targets we mount the
 * iframe at a same-origin proxy path so in-page navigations stay readable
 * without touching cross-origin DOM on direct embeds.
 *
 * Path shape: /api/workspace-browser-proxy/{http|https}/{host}[/{path...}]
 * Example:    /api/workspace-browser-proxy/http/127.0.0.1:54040/page-b.html
 */

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

/** Build iframe mount path for a loopback URL; null → load URL directly. */
export function buildWorkspaceBrowserProxyPath(targetUrl: string): string | null {
  if (!isLoopbackBrowserTarget(targetUrl)) return null;
  let parsed: URL;
  try {
    parsed = new URL(targetUrl);
  } catch {
    return null;
  }
  const scheme = parsed.protocol.replace(/:$/, "");
  const host = parsed.host; // includes port
  let path = parsed.pathname.startsWith("/") ? parsed.pathname : `/${parsed.pathname}`;
  const search = parsed.search || "";
  const hash = parsed.hash || "";
  // Next.js 308-strips a trailing slash on this route; mount "/" as no extra path
  // segment. <base href> in rewritten HTML still uses hostRoot+"/".
  if (path === "/") path = "";
  return `${WORKSPACE_BROWSER_PROXY_PREFIX}/${scheme}/${host}${path}${search}${hash}`;
}

export type ParsedProxyTarget =
  | { kind: "ok"; url: URL }
  | { kind: "error"; status: number; detail: string };

/** Parse catch-all route segments into a loopback target URL. */
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
  // segments are already decoded by Next; re-join raw path
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

/**
 * If href is a proxied location on this origin, return the real target URL.
 * Otherwise return href unchanged.
 */
export function unwrapWorkspaceBrowserProxyHref(href: string): string {
  try {
    const parsed = new URL(href, "http://127.0.0.1");
    const prefix = WORKSPACE_BROWSER_PROXY_PREFIX + "/";
    if (!parsed.pathname.startsWith(prefix)) return href;
    const rest = parsed.pathname.slice(prefix.length); // http/127.0.0.1:54040/page-b.html
    const parts = rest.split("/").filter(Boolean);
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

/**
 * Rewrite HTML so navigations stay on the proxy (same-origin with Chat UI).
 * Handles path-absolute href/src/action and same-origin absolute URLs.
 * Injects <base> for document-relative links.
 */
export function rewriteProxiedHtml(html: string, target: URL): string {
  const scheme = target.protocol.replace(/:$/, "");
  const hostRoot = `${WORKSPACE_BROWSER_PROXY_PREFIX}/${scheme}/${target.host}`;
  const dirPath = target.pathname.endsWith("/")
    ? target.pathname
    : target.pathname.replace(/[^/]*$/, "");
  const baseHref = `${hostRoot}${dirPath || "/"}`;

  let out = html;
  // path-absolute: href="/page-b.html" → proxy host root + path
  out = out.replace(
    /(\s(?:href|src|action)\s*=\s*["'])\/(?!\/)/gi,
    `$1${hostRoot}/`,
  );
  // absolute same-origin: href="http://127.0.0.1:54040/foo"
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

export function rewriteProxiedRedirectLocation(locationHeader: string, requestUrl: URL): string | null {
  try {
    const absolute = new URL(locationHeader, requestUrl);
    if (!isLoopbackBrowserTarget(absolute.toString())) return null;
    return buildWorkspaceBrowserProxyPath(absolute.toString());
  } catch {
    return null;
  }
}
