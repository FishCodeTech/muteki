import manifest from "@/public/manifest.json";
import { APP_ICON_PATHS, BRAND_VERSION, brandSurface } from "@/lib/brand";

export function GET(request: Request) {
  const surface = brandSurface(new URL(request.url).searchParams.get("surface"));
  const icons = ([192, 512] as const).map((size) => ({
    src: `${APP_ICON_PATHS[size]}?v=${BRAND_VERSION}`, sizes: `${size}x${size}`, type: "image/png", purpose: "any maskable",
  }));
  return Response.json({
    ...manifest,
    id: "/chat",
    scope: "/",
    background_color: surface,
    theme_color: surface,
    icons,
    shortcuts: manifest.shortcuts.map((shortcut) => ({ ...shortcut, icons: [icons[0]] })),
  }, { headers: { "Content-Type": "application/manifest+json", "Cache-Control": "public, max-age=3600" } });
}
