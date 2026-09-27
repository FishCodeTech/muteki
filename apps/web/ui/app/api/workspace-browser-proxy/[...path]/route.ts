export const runtime = "nodejs";
export const dynamic = "force-dynamic";

import {
  parseWorkspaceBrowserProxyPath,
  rewriteProxiedHtml,
  rewriteProxiedRedirectLocation,
} from "@/lib/workspaceBrowserProxy";

type RouteContext = {
  params: Promise<{ path?: string[] }>;
};

const HOP = [
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  "content-encoding",
  "content-length",
];

function filterRequestHeaders(req: Request): Headers {
  const headers = new Headers();
  // Minimal headers — avoid leaking cookies from the Chat origin to loopback targets.
  const accept = req.headers.get("accept");
  if (accept) headers.set("accept", accept);
  const acceptLang = req.headers.get("accept-language");
  if (acceptLang) headers.set("accept-language", acceptLang);
  headers.set("user-agent", req.headers.get("user-agent") || "muteki-workspace-browser-proxy");
  return headers;
}

function buildResponseHeaders(upstream: Response, target: URL, isHtml: boolean): Headers {
  const headers = new Headers();
  const contentType = upstream.headers.get("content-type");
  if (contentType) headers.set("content-type", contentType);
  // Allow embedding in Chat UI iframe.
  headers.delete("x-frame-options");
  headers.delete("content-security-policy");
  headers.delete("content-security-policy-report-only");
  headers.set("cache-control", "no-store");
  const location = upstream.headers.get("location");
  if (location) {
    const rewritten = rewriteProxiedRedirectLocation(location, target);
    if (rewritten) headers.set("location", rewritten);
    else headers.set("location", location);
  }
  if (isHtml && !headers.get("content-type")) {
    headers.set("content-type", "text/html; charset=utf-8");
  }
  for (const name of HOP) headers.delete(name);
  return headers;
}

export async function GET(req: Request, ctx: RouteContext) {
  const params = await ctx.params;
  const parsed = parseWorkspaceBrowserProxyPath(params.path || []);
  if (parsed.kind === "error") {
    return Response.json({ ok: false, detail: parsed.detail }, { status: parsed.status });
  }
  const target = parsed.url;
  // Carry query from the proxy request onto the target (path builder may already include search).
  const incoming = new URL(req.url);
  if (incoming.search && !target.search) target.search = incoming.search;

  let upstream: Response;
  try {
    upstream = await fetch(target.toString(), {
      method: "GET",
      headers: filterRequestHeaders(req),
      redirect: "manual",
      cache: "no-store",
      signal: req.signal,
    });
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    return Response.json({ ok: false, detail: `proxy fetch failed: ${detail}` }, { status: 502 });
  }

  const contentType = upstream.headers.get("content-type") || "";
  const isHtml = /text\/html/i.test(contentType) || upstream.status === 0;
  const headers = buildResponseHeaders(upstream, target, isHtml);

  if (upstream.status >= 300 && upstream.status < 400) {
    return new Response(null, { status: upstream.status, headers });
  }

  if (isHtml) {
    const html = await upstream.text();
    const rewritten = rewriteProxiedHtml(html, target);
    return new Response(rewritten, { status: upstream.status, headers });
  }

  return new Response(upstream.body, { status: upstream.status, headers });
}
