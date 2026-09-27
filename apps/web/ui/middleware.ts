import { NextRequest, NextResponse } from "next/server";
import { backendApiUrl } from "./lib/backendApiUrl";

// App Route handlers proxy HTTP bodies but cannot accept WebSocket upgrades.
// An external middleware rewrite lets Next's upgrade handler proxy this stream
// using the same runtime backend configuration as the HTTP API proxy.
export function middleware(request: NextRequest) {
  return NextResponse.rewrite(backendApiUrl(request.nextUrl.pathname, request.nextUrl.search));
}

export const config = {
  matcher: "/api/threads/:threadId/workspace/terminal/sessions/:sessionId/stream",
};
