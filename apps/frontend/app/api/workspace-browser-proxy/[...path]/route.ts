export const runtime = "nodejs";
export const dynamic = "force-dynamic";

import {
  buildWorkspaceBrowserProxyPath,
  parseWorkspaceBrowserProxyPath,
  rewriteProxiedHtml,
  rewriteProxiedRedirectLocation,
} from "@/lib/workspaceBrowserProxy";
import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";
import { allowedBrowserControlOrigin } from "@/lib/browserControlPolicy";
import { backendApiUrl } from "@/lib/backendApiUrl";

const ticketKey = randomBytes(32);
const approvedOrigins = new Set<string>();
function issueTicket(origin: string): string {
  const payload = Buffer.from(JSON.stringify({ origin, expires: Date.now() + 24 * 60 * 60 * 1000 })).toString("base64url");
  return `v1.${payload}.${createHmac("sha256", ticketKey).update(payload).digest("base64url")}`;
}
function validTicket(ticket: string, origin: string): boolean {
  try {
    const [version, payload, signature, extra] = ticket.split(".");
    if (version !== "v1" || !payload || !signature || extra) return false;
    const expected = createHmac("sha256", ticketKey).update(payload).digest();
    const actual = Buffer.from(signature, "base64url");
    if (actual.length !== expected.length || !timingSafeEqual(actual, expected)) return false;
    const data = JSON.parse(Buffer.from(payload, "base64url").toString());
    return data.origin === origin && typeof data.expires === "number" && data.expires > Date.now();
  } catch { return false; }
}

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
  const accept = req.headers.get("accept");
  if (accept) headers.set("accept", accept);
  const acceptLang = req.headers.get("accept-language");
  if (acceptLang) headers.set("accept-language", acceptLang);
  const origin = req.headers.get("origin");
  if (origin === "null") headers.set("origin", origin);
  headers.set("user-agent", req.headers.get("user-agent") || "muteki-workspace-browser-proxy");
  return headers;
}

function buildResponseHeaders(upstream: Response, target: URL, isHtml: boolean, ticket: string, requestOrigin: string | null): Headers {
  const headers = new Headers();
  const contentType = upstream.headers.get("content-type");
  if (contentType) headers.set("content-type", contentType);
  const allowedOrigin = upstream.headers.get("access-control-allow-origin");
  if (requestOrigin === "null" && (allowedOrigin === "*" || allowedOrigin === "null")) {
    headers.set("access-control-allow-origin", "null");
    const exposed = upstream.headers.get("access-control-expose-headers");
    if (exposed) headers.set("access-control-expose-headers", exposed);
  }
  headers.set("vary", "Origin");
  headers.delete("x-frame-options");
  headers.delete("content-security-policy");
  headers.delete("content-security-policy-report-only");
  headers.set("content-security-policy", "sandbox allow-forms allow-modals allow-popups allow-scripts");
  headers.set("cache-control", "no-store");
  headers.set("referrer-policy", "no-referrer");
  const location = upstream.headers.get("location");
  if (location) {
    const rewritten = rewriteProxiedRedirectLocation(location, target, ticket);
    if (rewritten) headers.set("location", rewritten);
  }
  if (isHtml && !headers.get("content-type")) {
    headers.set("content-type", "text/html; charset=utf-8");
  }
  for (const name of HOP) headers.delete(name);
  return headers;
}

async function authorizePreview(req: Request, ctx: RouteContext, confirm: boolean) {
  const protocol = req.headers.get("x-forwarded-proto") || new URL(req.url).protocol.slice(0, -1);
  const workbenchOrigin = `${protocol}://${req.headers.get("host") || new URL(req.url).host}`;
  if (req.headers.get("origin") !== workbenchOrigin) {
    return Response.json({ detail: "Only the workbench can open a preview." }, { status: 403 });
  }
  const authHeaders = new Headers();
  for (const name of ["cookie", "authorization"]) {
    const value = req.headers.get(name);
    if (value) authHeaders.set(name, value);
  }
  try {
    const response = await fetch(backendApiUrl("/api/auth/me"), { headers: authHeaders, cache: "no-store", redirect: "manual", signal: req.signal });
    if (!response.ok || (await response.json()).authenticated !== true) {
      return Response.json({ detail: "Sign in to open a preview." }, { status: 401 });
    }
  } catch {
    return Response.json({ detail: "Cannot verify the workbench session." }, { status: 502 });
  }
  const params = await ctx.params;
  const parsed = parseWorkspaceBrowserProxyPath(params.path || []);
  if (parsed.kind === "error") return Response.json({ detail: parsed.detail }, { status: parsed.status });
  const target = parsed.url;
  const upstreamApi = backendApiUrl("/");
  const apiPorts = new Set(["8100", upstreamApi.port || (upstreamApi.protocol === "https:" ? "443" : "80")]);
  const frontendPorts = new Set(["3101", "3103", new URL(req.url).port, req.headers.get("host")?.match(/:(\d+)$/)?.[1]]);
  const targetPort = target.port || (target.protocol === "https:" ? "443" : "80");
  if (frontendPorts.has(targetPort) && !apiPorts.has(targetPort) && !target.username && !target.password) {
    try {
      const health = await fetch(new URL("/api/health", target), {
        cache: "no-store", redirect: "error", signal: req.signal,
      });
      if (health.ok && health.headers.get("X-Muteki-Workbench") === "1") {
        target.search = new URL(req.url).search;
        return Response.json({ src: target.toString(), direct: true }, { headers: { "cache-control": "no-store" } });
      }
    } catch {
      if (req.signal.aborted) return Response.json({ detail: "预览请求已取消。" }, { status: 408 });
      return Response.json({ detail: "无法连接目标工作台。" }, { status: 502 });
    }
  }
  if ((apiPorts.has(targetPort) || frontendPorts.has(targetPort)) && !allowedBrowserControlOrigin(target)) {
    return Response.json({ detail: "预览不能访问 Muteki 控制端口。" }, { status: 403 });
  }
  if (confirm) {
    if (req.headers.get("x-muteki-preview-confirm") !== "1") return Response.json({ detail: "请确认访问此本机服务。" }, { status: 403 });
    approvedOrigins.add(target.origin);
  }
  if (!approvedOrigins.has(target.origin)) {
    return Response.json({ code: "workspace.browser.approval_required", origin: target.origin }, { status: 428 });
  }
  target.search = new URL(req.url).search;
  return Response.json({ src: buildWorkspaceBrowserProxyPath(target.toString(), issueTicket(target.origin)) }, { headers: { "cache-control": "no-store" } });
}

export async function POST(req: Request, ctx: RouteContext) { return authorizePreview(req, ctx, false); }
export async function PUT(req: Request, ctx: RouteContext) { return authorizePreview(req, ctx, true); }

export async function GET(req: Request, ctx: RouteContext) {
  const params = await ctx.params;
  const segments = [...(params.path || [])];
  const ticket = segments[0]?.startsWith("v1.") ? segments.shift()! : "";
  const parsed = parseWorkspaceBrowserProxyPath(segments);
  if (parsed.kind === "error") {
    return Response.json({ ok: false, detail: parsed.detail }, { status: parsed.status });
  }
  const target = parsed.url;
  const controlPorts = new Set(["8100", "3101", "3103", new URL(req.url).port]);
  const apiOrigin = new URL(process.env.MUTEKI_API_UPSTREAM || "http://127.0.0.1:8100");
  controlPorts.add(apiOrigin.port || (apiOrigin.protocol === "https:" ? "443" : "80"));
  if (controlPorts.has(target.port || (target.protocol === "https:" ? "443" : "80")) && !allowedBrowserControlOrigin(target)) {
    return Response.json({ ok: false, code: "workspace.browser.control_plane_denied", detail: "预览代理不能访问 Muteki 的服务或工作台端口。" }, { status: 403 });
  }
  if (!validTicket(ticket, target.origin)) {
    return Response.json({ detail: "Open this preview from the workbench." }, { status: 403 });
  }
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
  const headers = buildResponseHeaders(upstream, target, isHtml, ticket, req.headers.get("origin"));

  if (upstream.status >= 300 && upstream.status < 400) {
    if (upstream.headers.has("location") && !headers.has("location")) {
      return Response.json({ detail: "Preview redirects must stay on the selected service." }, { status: 403 });
    }
    return new Response(null, { status: upstream.status, headers });
  }

  if (isHtml) {
    const html = await upstream.text();
    const rewritten = rewriteProxiedHtml(html, target, ticket);
    return new Response(rewritten, { status: upstream.status, headers });
  }

  return new Response(upstream.body, { status: upstream.status, headers });
}
