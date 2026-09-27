/** Shared geometry for the UI, favicon, install icons and exported artwork. */
export const BRAND_VERSION = "wu-1";
export const SEAL_PATH = "M20 8h216l12 12v216l-12 12H20L8 236V20Z";
export const WU_PATH = "M64 28h16l4 4v12h132l4 4v20l-4 4h-12v24h8l8 8v52l-4 4H40l-4-4v-52l8-8h8V72H40l-4-4V56ZM80 72v24h16V72Zm40 0v24h16V72Zm40 0v24h16V72ZM62 116v24h28v-24Zm52 0v24h28v-24Zm52 0v24h28v-24ZM40 172h176l4 4v44l-4 4h-24v-32h-24v32h-28v-32h-24v32H88v-32H64v32H40l-4-4v-44Z";
export const WORDMARK_PATH = "M0 56V0h10l14 22L38 0h10v56H37V20L24 40 11 20v36ZM62 0h11v35c0 8 4 12 11 12s11-4 11-12V0h11v35c0 14-8 23-22 23S62 49 62 35ZM120 0h46v10h-17v46h-12V10h-17ZM180 0h38v10h-26v12h23v10h-23v14h26v10h-38ZM232 0h12v23l21-23h15l-25 27 26 29h-15l-22-25v25h-12ZM296 0h12v56h-12Z";
export const WORDMARK_TRANSFORM = "translate(300 70) scale(2)";
// All glyph corners stay inside the central 80% circle required by maskable icons.
export const APP_GLYPH_TRANSFORM = "translate(32 32) scale(.75)";

export type BrandColors = { accent: string; ink: string; surface: string };
export const DEFAULT_BRAND_COLORS: BrandColors = {
  accent: "#87a7ff",
  ink: "#0b0e12",
  surface: "#0f1011",
};

function hexColor(value: string | null | undefined, fallback: string): string {
  const hex = value?.replace(/^#/, "");
  return hex && /^[\da-f]{6}$/i.test(hex) ? `#${hex.toLowerCase()}` : fallback;
}

export function brandColorsFromParams(params: URLSearchParams): BrandColors {
  return {
    accent: hexColor(params.get("accent"), DEFAULT_BRAND_COLORS.accent),
    ink: hexColor(params.get("ink"), DEFAULT_BRAND_COLORS.ink),
    surface: hexColor(params.get("surface"), DEFAULT_BRAND_COLORS.surface),
  };
}

export function brandAssetParams(colors: BrandColors): URLSearchParams {
  return new URLSearchParams({
    v: BRAND_VERSION,
    accent: hexColor(colors.accent, DEFAULT_BRAND_COLORS.accent).slice(1),
    ink: hexColor(colors.ink, DEFAULT_BRAND_COLORS.ink).slice(1),
    surface: hexColor(colors.surface, DEFAULT_BRAND_COLORS.surface).slice(1),
  });
}

export function brandIconUrl(colors: BrandColors, size: 180 | 192 | 512): string {
  const params = brandAssetParams(colors);
  params.set("size", String(size));
  return `/brand/icon.png?${params}`;
}

export function brandSvg({
  accent = DEFAULT_BRAND_COLORS.accent,
  ink = DEFAULT_BRAND_COLORS.ink,
  wordmark = false,
  text = "#1c1d21",
  appIcon = false,
}: Partial<BrandColors> & { wordmark?: boolean; text?: string; appIcon?: boolean } = {}): string {
  const fill = hexColor(accent, DEFAULT_BRAND_COLORS.accent);
  const foreground = hexColor(ink, DEFAULT_BRAND_COLORS.ink);
  const wide = wordmark && !appIcon;
  const width = wide ? 928 : 256;
  const seal = appIcon
    ? `<rect width="256" height="256" fill="${fill}"/>`
    : `<path fill="${fill}" d="${SEAL_PATH}"/>`;
  const glyph = `<path fill="${foreground}" fill-rule="evenodd" d="${WU_PATH}"${appIcon ? ` transform="${APP_GLYPH_TRANSFORM}"` : ""}/>`;
  const lettering = wide
    ? `<path fill="${hexColor(text, "#1c1d21")}" d="${WORDMARK_PATH}" transform="${WORDMARK_TRANSFORM}"/>`
    : "";
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="256" viewBox="0 0 ${width} 256" role="img" aria-label="Muteki"><title>Muteki · 無印</title>${seal}${glyph}${lettering}</svg>`;
}

/** Called by the palette engine for presets, free hues, boot and light/dark changes. */
export function updateBrandHead(palette: Record<string, string>): void {
  if (typeof document === "undefined") return;
  const colors: BrandColors = {
    accent: palette["--accent"],
    ink: palette["--on-accent"],
    surface: palette["--bg"],
  };
  const favicon = document.getElementById("muteki-favicon");
  const apple = document.getElementById("muteki-apple-icon");
  const manifest = document.getElementById("muteki-manifest");
  favicon?.setAttribute("href", `data:image/svg+xml,${encodeURIComponent(brandSvg(colors))}`);
  apple?.setAttribute("href", brandIconUrl(colors, 180));
  manifest?.setAttribute("href", `/brand/manifest.webmanifest?${brandAssetParams(colors)}`);
}
