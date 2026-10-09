export const runtime = "nodejs";
export const dynamic = "force-dynamic";
export const maxDuration = 900;

import { backendApiUrl } from "@/lib/backendApiUrl";

const hopByHopHeaders = [
  "host",
  "connection",
  "content-length",
  "transfer-encoding",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "te",
  "trailer",
  "upgrade",
];

type RouteContext = {
  params: Promise<{
    path?: string[];
  }>;
};

function apiUrl(req: Request, path: string[] | undefined): string {
  const requestUrl = new URL(req.url);
  const encodedPath = (path || []).map((part) => encodeURIComponent(part)).join("/");
  return backendApiUrl("/api/" + encodedPath, requestUrl.search).toString();
}

function requestHeaders(req: Request): Headers {
  const headers = new Headers(req.headers);
  for (const name of hopByHopHeaders) headers.delete(name);
  // Validate the browser origin here, before mapping it to the backend origin.
  // Caller-supplied forwarding headers never establish that trust.
  const origin = req.headers.get("origin");
  headers.delete("forwarded");
  headers.delete("x-forwarded-for");
  headers.delete("x-real-ip");
  headers.delete("x-forwarded-host");
  headers.delete("x-forwarded-proto");
  if (origin) {
    headers.set("origin", backendApiUrl("/").origin);
    headers.set("x-forwarded-proto", new URL(origin).protocol.slice(0, -1));
  }
  return headers;
}

function responseHeaders(upstream: Response): Headers {
  const headers = new Headers(upstream.headers);
  for (const name of hopByHopHeaders) headers.delete(name);
  return headers;
}

async function proxy(req: Request, ctx: RouteContext) {
  const origin = req.headers.get("origin");
  if (origin) {
    let sameHost = false;
    try { sameHost = new URL(origin).host === req.headers.get("host"); } catch { /* reject malformed origin */ }
    if (!sameHost || (req.headers.get("sec-fetch-site") === "cross-site")) {
      return Response.json({ error: { code: "auth.origin_invalid", message: "请求来源不受信任，请从工作台页面重试。" } }, { status: 403 });
    }
  }
  const params = await ctx.params;
  const init: RequestInit & { duplex?: "half" } = {
    method: req.method,
    headers: requestHeaders(req),
    cache: "no-store",
    redirect: "manual",
    // 页面关闭/导航离开时取消上游请求，释放长连接。
    signal: req.signal,
  };
  if (req.method !== "GET" && req.method !== "HEAD" && req.body !== null) {
    init.body = req.body;
    init.duplex = "half";
  }

  try {
    const upstream = await fetch(apiUrl(req, params.path), init);
    return new Response(upstream.body, {
      status: upstream.status,
      statusText: upstream.statusText,
      headers: responseHeaders(upstream),
    });
  } catch (err) {
    const detail = err instanceof Error ? err.message : String(err);
    return Response.json(
      { ok: false, detail: `api proxy failed: ${detail}` },
      { status: 502 },
    );
  }
}

export const GET = proxy;
export const POST = proxy;
export const PUT = proxy;
export const PATCH = proxy;
export const DELETE = proxy;
export const OPTIONS = proxy;
