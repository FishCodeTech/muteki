export const APP_NAME = "Muteki";
export const BRAND_VERSION = "daylight-1";
export const SEAL_PATH = "M20 8h216l12 12v216l-12 12H20L8 236V20Z";
export const WU_PATH = "M64 28h16l4 4v12h132l4 4v20l-4 4h-12v24h8l8 8v52l-4 4H40l-4-4v-52l8-8h8V72H40l-4-4V56ZM80 72v24h16V72Zm40 0v24h16V72Zm40 0v24h16V72ZM62 116v24h28v-24Zm52 0v24h28v-24Zm52 0v24h28v-24ZM40 172h176l4 4v44l-4 4h-24v-32h-24v32h-28v-32h-24v32H88v-32H64v32H40l-4-4v-44Z";
export const WORDMARK_PATH = "M0 56V0h10l14 22L38 0h10v56H37V20L24 40 11 20v36ZM62 0h11v35c0 8 4 12 11 12s11-4 11-12V0h11v35c0 14-8 23-22 23S62 49 62 35ZM120 0h46v10h-17v46h-12V10h-17ZM180 0h38v10h-26v12h23v10h-23v14h26v10h-38ZM232 0h12v23l21-23h15l-25 27 26 29h-15l-22-25v25h-12ZM296 0h12v56h-12Z";
export const WORDMARK_TRANSFORM = "translate(300 70) scale(2)";

export const APP_ICON_PATHS = {
  180: "/apple-touch-icon.png",
  192: "/icon-192.png",
  512: "/icon-512.png",
} as const;

export function brandSurface(surface: string | null | undefined): string {
  const hex = surface?.replace(/^#/, "");
  return hex && /^[\da-f]{6}$/i.test(hex) ? `#${hex.toLowerCase()}` : "#0f1011";
}

export function updateBrandHead(palette: Record<string, string>): void {
  if (typeof document === "undefined") return;
  const manifest = document.getElementById("muteki-manifest");
  const params = new URLSearchParams({
    v: BRAND_VERSION,
    surface: brandSurface(palette["--bg"]).slice(1),
  });
  manifest?.setAttribute("href", `/brand/manifest.webmanifest?${params}`);
}
