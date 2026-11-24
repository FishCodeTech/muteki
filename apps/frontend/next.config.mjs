import path from "node:path";
import { fileURLToPath } from "node:url";
import buildInfo from "./build-info.cjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));


/** @type {import('next').NextConfig} */
const nextConfig = {
  env: { NEXT_PUBLIC_MUTEKI_UI_BUILD: buildInfo.uiBuildId() },
  transpilePackages: ["@muteki/ui"],
  experimental: {
    optimizePackageImports: ["@muteki/ui", "@muteki/ui/agent", "lucide-react"],
  },
  webpack(config) {
    config.resolve.modules = [
      ...(config.resolve.modules || ["node_modules"]),
      path.resolve(__dirname, "node_modules"),
    ];
    return config;
  },
  async redirects() {
    return [
      { source: "/task", destination: "/ctf", permanent: true },
      { source: "/task/workers", destination: "/ctf/workers", permanent: true },
      { source: "/solve", destination: "/ctf", permanent: true },
    ];
  },
  distDir: process.env.MUTEKI_NEXT_DIST_DIR || ".next",
  output: "standalone",
  compress: false,
};

export default nextConfig;
