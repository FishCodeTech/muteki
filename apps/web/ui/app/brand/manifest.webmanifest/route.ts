import manifest from "@/public/manifest.json";
import { brandColorsFromParams, brandIconUrl } from "@/lib/brand";

export function GET(request: Request) {
  const colors = brandColorsFromParams(new URL(request.url).searchParams);
  const icons = ([192, 512] as const).map((size) => ({
    src: brandIconUrl(colors, size), sizes: `${size}x${size}`, type: "image/png", purpose: "any maskable",
  }));
  return Response.json({
    ...manifest,
    id: "/chat",
    scope: "/",
    background_color: colors.surface,
    theme_color: colors.surface,
    icons,
    shortcuts: manifest.shortcuts.map((shortcut) => ({ ...shortcut, icons: [icons[0]] })),
  }, { headers: { "Content-Type": "application/manifest+json", "Cache-Control": "public, max-age=3600" } });
}
