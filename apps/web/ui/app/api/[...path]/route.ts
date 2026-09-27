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
  return headers;
}

function responseHeaders(upstream: Response): Headers {
  const headers = new Headers(upstream.headers);
  for (const name of hopByHopHeaders) headers.delete(name);
  return headers;
}

async function proxy(req: Request, ctx: RouteContext) {
  const params = await ctx.params;
  const init: RequestInit & { duplex?: "half" } = {
    method: req.method,
    headers: requestHeaders(req),
    cache: "no-store",
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
