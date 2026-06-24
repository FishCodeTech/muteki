/**
 * Static export was dropped (FE-routing-workspace): per-run deep links use real
 * dynamic routes (/run/[id]), which `output: "export"` can't prerender (run ids
 * are not known at build time). `run.sh web` serves the UI via `next dev`/`next
 * start` (the operator's environment), which handles dynamic routes natively;
 * `/api` is proxied to the FastAPI backend (default :8000; override MUTEKI_BACKEND).
 */
const BACKEND = process.env.MUTEKI_BACKEND || "http://127.0.0.1:8000";

/** @type {import('next').NextConfig} */
const nextConfig = {
  async rewrites() {
    return [{ source: "/api/:path*", destination: `${BACKEND}/api/:path*` }];
  },
};

export default nextConfig;
