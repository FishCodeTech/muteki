import { NextRequest, NextResponse } from "next/server";
import { backendApiUrl } from "./lib/backendApiUrl";

export function middleware(request: NextRequest) {
  const bindHost = process.env.MUTEKI_UI_HOST || "127.0.0.1";
  const local = ["127.0.0.1", "::1", "localhost"].includes(bindHost);
  if (local && !/^(?:127\.0\.0\.1|\[::1\]|localhost)(?::[0-9]+)?$/i.test(request.headers.get("host") ?? "")) {
    return NextResponse.json({ error: { code: "auth.host_invalid", message: "请使用本机工作台地址。" } }, { status: 403 });
  }
  const { pathname, search } = request.nextUrl;
  if (/^\/api\/threads\/[^/]+\/workspace\/terminal\/sessions\/[^/]+\/stream$/.test(pathname)) {
    return NextResponse.rewrite(backendApiUrl(pathname, search));
  }
  const response = NextResponse.next();
  if (pathname === "/api/health") response.headers.set("X-Muteki-Workbench", "1");
  return response;
}

export const config = {
  matcher: "/api/:path*",
};
