/**
 * Static export was dropped (FE-routing-workspace): per-run deep links use real
 * dynamic routes (/run/[id]), which `output: "export"` can't prerender (run ids
 * are not known at build time). `run.sh web` serves the UI as a production Next
 * server in the operator's environment, which handles dynamic routes natively;
 * `/api` is proxied to the FastAPI backend (default :8000; override MUTEKI_BACKEND).
 */
const BACKEND = process.env.MUTEKI_BACKEND || "http://127.0.0.1:8000";

/** @type {import('next').NextConfig} */
const nextConfig = {
  // P2-v3: standalone output for the compose `ui` image and bare-host `run.sh web`
  // path (a self-contained server bundle — no full node_modules in the runtime layer).
  output: "standalone",
  // SSE FIX: Next defaults to compress:true, which gzips proxied responses —
  // INCLUDING the /api/runs/<id>/events EventSource stream. gzip buffers the
  // stream so the browser EventSource never gets incremental frames, the deck
  // never folds RUN_STARTED, and a selected run shows the welcome screen instead
  // of its conversation (only reproduces behind the standalone server / docker,
  // not next dev). text/event-stream must never be compressed; disable Next gzip.
  compress: false,
  async rewrites() {
    return [{ source: "/api/:path*", destination: `${BACKEND}/api/:path*` }];
  },
};

export default nextConfig;
