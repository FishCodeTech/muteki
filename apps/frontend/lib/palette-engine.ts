import { generateColorScale, colorHue, colorChroma, SHADE_STEPS } from "./color-scale";
export { SHADE_STEPS } from "./color-scale";
import { readUiPreference, hasLoadedUiPreferences } from "./uiPreferences";
import { readAppearanceOverride } from "./desktopEnvironment";
import { runWithoutThemeTransitions } from "./themeTransition";

import { updateBrandHead } from "./brand";

export type ThemeMode = "light" | "dark";

export interface SchemeDef {
  id: string;
  hue: number;
  labelKey: string;
  lightL?: number;
  darkL?: number;
}

export const SCHEMES: SchemeDef[] = [
  { id: "azure", hue: 268, labelKey: "scheme.azure" },
  { id: "violet", hue: 296, labelKey: "scheme.violet", darkL: 0.76 },
  { id: "teal", hue: 183, labelKey: "scheme.teal" },
  { id: "ember", hue: 53, labelKey: "scheme.ember" },
];

const DEFAULT_SCHEME = "azure";
const SCHEME_STORAGE_KEY = "muteki.scheme";


function oklchToLinearSrgb(l: number, c: number, h: number): [number, number, number] {
  const hr = (h * Math.PI) / 180;
  const a = c * Math.cos(hr);
  const b = c * Math.sin(hr);
  const l_ = l + 0.3963377774 * a + 0.2158037573 * b;
  const m_ = l - 0.1055613458 * a - 0.0638541728 * b;
  const s_ = l - 0.0894841775 * a - 1.291485548 * b;
  const L = l_ ** 3;
  const M = m_ ** 3;
  const S = s_ ** 3;
  return [
    +4.0767416621 * L - 3.3077115913 * M + 0.2309699292 * S,
    -1.2684380046 * L + 2.6097574011 * M - 0.3413193965 * S,
    -0.0041960863 * L - 0.7034186147 * M + 1.707614701 * S,
  ];
}

function linearToSrgb(x: number): number {
  return x <= 0.0031308 ? 12.92 * x : 1.055 * Math.pow(x, 1 / 2.4) - 0.055;
}

function inGamut(rgb: [number, number, number]): boolean {
  return rgb.every((v) => v >= -1e-4 && v <= 1 + 1e-4);
}

function clampToGamut(l: number, c: number, h: number): [number, number, number] {
  let rgb = oklchToLinearSrgb(l, c, h);
  if (inGamut(rgb)) return rgb.map((v) => Math.min(1, Math.max(0, v))) as [number, number, number];
  let lo = 0;
  let hi = c;
  for (let i = 0; i < 20; i++) {
    const mid = (lo + hi) / 2;
    if (inGamut(oklchToLinearSrgb(l, mid, h))) lo = mid;
    else hi = mid;
  }
  rgb = oklchToLinearSrgb(l, lo, h);
  return rgb.map((v) => Math.min(1, Math.max(0, v))) as [number, number, number];
}

function rgbToHex(rgb: [number, number, number]): string {
  const c = (v: number) => Math.round(linearToSrgb(v) * 255).toString(16).padStart(2, "0");
  return `#${c(rgb[0])}${c(rgb[1])}${c(rgb[2])}`;
}

function addSurfaceTokens(out: Record<string, string>): void {
  out["--surface"] = out["--panel"];
  out["--accent-foreground"] = out["--on-accent"];
  out["--focus"] = out["--focus-ring"];
}

function oklch(l: number, c: number, h: number): string {
  return rgbToHex(clampToGamut(l, c, h));
}


function hexToLinear(hex: string): [number, number, number] {
  const v = parseInt(hex.slice(1), 16);
  const f = (x: number) => {
    const s = x / 255;
    return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return [f((v >> 16) & 255), f((v >> 8) & 255), f(v & 255)];
}

function luminance(hex: string): number {
  const [r, g, b] = hexToLinear(hex);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrastRatio(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}


const norm360 = (h: number) => ((h % 360) + 360) % 360;
const hueDist = (a: number, b: number) => Math.abs(((a - b + 540) % 360) - 180);

const MIN_GAP_ACCENT = 26;
const MIN_GAP_ROLE = 22;

function resolveHue(target: number, taken: number[], minGap: number): number {
  if (taken.every((h) => hueDist(target, h) >= minGap)) return norm360(target);
  let best = norm360(target);
  let bestGap = -1;
  for (let step = 6; step <= 60; step += 6) {
    for (const sign of [1, -1] as const) {
      const cand = norm360(target + sign * step);
      const gap = Math.min(...taken.map((h) => hueDist(cand, h)));
      if (gap >= minGap) return cand;
      if (gap > bestGap) {
        bestGap = gap;
        best = cand;
      }
    }
  }
  return best;
}

interface RoleSpec {
  token: string;
  hue: number | null;
  lightL: number;
  lightC: number;
  darkL: number;
  darkC: number;
  semantic?: boolean;
}

const ROLES: RoleSpec[] = [
  { token: "green", hue: 148, lightL: 0.52, lightC: 0.17, darkL: 0.78, darkC: 0.2, semantic: true },
  { token: "amber", hue: 75, lightL: 0.52, lightC: 0.14, darkL: 0.78, darkC: 0.14, semantic: true },
  { token: "red", hue: 27, lightL: 0.55, lightC: 0.2, darkL: 0.7, darkC: 0.18, semantic: true },
  { token: "gold", hue: 88, lightL: 0.55, lightC: 0.12, darkL: 0.76, darkC: 0.13, semantic: true },
  { token: "cyan", hue: 220, lightL: 0.5, lightC: 0.12, darkL: 0.74, darkC: 0.12 },
  { token: "violet", hue: 300, lightL: 0.5, lightC: 0.22, darkL: 0.76, darkC: 0.16 },
  { token: "magenta", hue: 328, lightL: 0.52, lightC: 0.22, darkL: 0.76, darkC: 0.18 },
  { token: "pink", hue: 350, lightL: 0.53, lightC: 0.2, darkL: 0.74, darkC: 0.17 },
];

const PANEL_BG: Record<ThemeMode, string> = { light: "#ffffff", dark: "#1b1f24" };
const ACCENT_MIN_CONTRAST = 4.5;

const TEXT_NEUTRALS: { token: string; lightL: number; darkL: number }[] = [
  { token: "text", lightL: 0.27, darkL: 0.90 },
  { token: "bright", lightL: 0.22, darkL: 0.95 },
  { token: "muted", lightL: 0.51, darkL: 0.70 },
  { token: "dim", lightL: 0.64, darkL: 0.52 },
];

const NEUTRALS: { token: string; lightL: number; darkL: number; line?: boolean }[] = [
  { token: "bg", lightL: 0.995, darkL: 0.2076 },
  { token: "rail", lightL: 0.984, darkL: 0.185 },
  { token: "bg2", lightL: 0.9662, darkL: 0.1943 },
  { token: "panel", lightL: 0.998, darkL: 0.2373 },
  { token: "panel2", lightL: 0.9752, darkL: 0.224 },
  { token: "panel3", lightL: 0.9537, darkL: 0.2703 },
  { token: "term-bg", lightL: 0.9614, darkL: 0.1853 },
  { token: "line", lightL: 0.9298, darkL: 0.2987, line: true },
  { token: "border", lightL: 0.9298, darkL: 0.2987, line: true },
  { token: "line2", lightL: 0.8924, darkL: 0.3498, line: true },
  { token: "border2", lightL: 0.8924, darkL: 0.3498, line: true },
];
const SURFACE_C: Record<ThemeMode, number> = { light: 0.0045, dark: 0.01 };
const LINE_C: Record<ThemeMode, number> = { light: 0.007, dark: 0.014 };

const CHROME_L: Record<ThemeMode, number> = { light: 0.962, dark: 0.158 };
const SIDE_C: Record<ThemeMode, number> = { light: 0.003, dark: 0.012 };
const SIDE_STEPS: Record<ThemeMode, { hover: number; active: number; selected: number; line: number }> = {
  light: { hover: 0.02, active: 0.034, selected: 0.045, line: 0.04 },
  dark: { hover: 0.036, active: 0.062, selected: 0.082, line: 0.085 },
};

const TEXT_LEVEL_GAP = 0.085;
const TEXT_C: Record<ThemeMode, number> = { light: 0.01, dark: 0.008 };

export const ENGINE_HUE: Record<string, number> = {
  claude: 45,
  codex: 165,
  cursor: 290,
  pi: 200,
  omp: 80,
  kimi: 240,
  grok: 320,
  opencode: 180,
  devin: 215,
  droid: 28,
  agy: 255,
  dsh: 350,
  reason: 265,
  deepseek: 265,
  verifier: 120,
};

export function hueColor(hue: number, mode: ThemeMode, chromaScale = 1): string {
  return mode === "light"
    ? oklch(0.52, 0.16 * chromaScale, hue)
    : oklch(0.74, 0.14 * chromaScale, hue);
}

export function currentMode(): ThemeMode {
  if (typeof document === "undefined") return "dark";
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

function schemeById(id: string): SchemeDef {
  return SCHEMES.find((s) => s.id === id) ?? SCHEMES[0];
}

export function buildColorScale(hue: number): Record<number, string> {
  return generateColorScale(oklch(0.63, 0.2, norm360(hue)));
}

export function buildPaletteFromHue(
  hue: number,
  mode: ThemeMode,
  opts: { lightL?: number; darkL?: number; hex?: string } = {},
): Record<string, string> {
  const out: Record<string, string> = {};
  hue = norm360(Number.isFinite(hue) ? hue : SCHEMES[0].hue);
  if (opts.hex) hue = colorHue(opts.hex);
  const scale = opts.hex ? generateColorScale(opts.hex) : buildColorScale(hue);
  for (const step of SHADE_STEPS) out[`--accent-${step}`] = scale[step];

  let accentL = mode === "light" ? opts.lightL ?? 0.53 : opts.darkL ?? 0.74;
  const accentC = colorChroma(scale[mode === "light" ? 600 : 300]);
  let accent = scale[mode === "light" ? 600 : 300];
  for (let i = 0; i < 40 && contrastRatio(accent, PANEL_BG[mode]) < ACCENT_MIN_CONTRAST; i++) {
    accentL += mode === "light" ? -0.01 : 0.01;
    if (accentL < 0.25 || accentL > 0.92) break;
    accent = oklch(accentL, accentC, hue);
  }
  out["--blue"] = accent;
  out["--accent"] = accent;
  out["--on-accent"] = mode === "light" ? "#ffffff" : "#0b0e12";

  const taken: number[] = [hue];
  for (const role of ROLES) {
    const target = role.hue ?? hue;
    const gap = role.semantic ? MIN_GAP_ACCENT : MIN_GAP_ROLE;
    const resolved = role.semantic ? target : resolveHue(target, taken, gap);
    taken.push(resolved);
    out[`--${role.token}`] = oklch(
      mode === "light" ? role.lightL : role.darkL,
      mode === "light" ? role.lightC : role.darkC,
      resolved,
    );
  }
  out["--yellow"] = out["--amber"];

  for (const n of NEUTRALS) {
    out[`--${n.token}`] = oklch(
      mode === "light" ? n.lightL : n.darkL,
      n.line ? LINE_C[mode] : n.token === "rail" ? SIDE_C[mode] : SURFACE_C[mode],
      hue,
    );
  }
  for (const n of TEXT_NEUTRALS) {
    out[`--${n.token}`] = oklch(mode === "light" ? n.lightL : n.darkL, TEXT_C[mode], hue);
  }

  const railL = mode === "light" ? NEUTRALS.find((n) => n.token === "rail")!.lightL : NEUTRALS.find((n) => n.token === "rail")!.darkL;
  const sign = mode === "light" ? -1 : 1;
  const sideAt = (delta: number, extraC = 0) => oklch(railL + sign * delta, SIDE_C[mode] + extraC, hue);
  out["--chrome"] = oklch(CHROME_L[mode], SIDE_C[mode] + 0.001, hue);
  out["--side-bg"] = out["--rail"];
  out["--side-hover"] = sideAt(SIDE_STEPS[mode].hover, 0.002);
  out["--side-active"] = sideAt(SIDE_STEPS[mode].active, 0.003);
  out["--side-selected"] = sideAt(SIDE_STEPS[mode].selected, 0.004);
  out["--side-line"] = sideAt(SIDE_STEPS[mode].line, 0.003);

  for (const [engine, engineHue] of Object.entries(ENGINE_HUE)) {
    out[`--eng-${engine}`] = hueColor(engineHue, mode);
  }
  out["--eng-default"] = hueColor(250, mode, 0.18);

  if (mode === "light") {
    out["--human-bg"] = oklch(0.95, 0.02, hue);
    out["--human-border"] = oklch(0.87, 0.03, hue);
  } else {
    out["--human-bg"] = oklch(0.26, 0.03, hue);
    out["--human-border"] = oklch(0.38, 0.05, hue);
  }
  const textSurfaces = ["bg", "panel", "panel2", "panel3", "rail", "chrome", "side-hover", "side-active", "side-selected"].map((s) => out[`--${s}`]);
  const clears = (color: string) => textSurfaces.every((surface) => contrastRatio(color, surface) >= 4.5);
  const textAt = (l: number) => oklch(l, TEXT_C[mode], hue);
  const stronger = mode === "light" ? -1 : 1;
  let dimL = mode === "light" ? 0.66 : 0.5;
  for (let i = 0; i < 120 && !clears(textAt(dimL)); i++) dimL += stronger * 0.005;
  const mutedLimit = TEXT_NEUTRALS.find((n) => n.token === "text")![mode === "light" ? "lightL" : "darkL"];
  let mutedL = dimL + stronger * TEXT_LEVEL_GAP;
  mutedL = mode === "light" ? Math.max(mutedL, mutedLimit + 0.1) : Math.min(mutedL, mutedLimit - 0.1);
  out["--dim"] = textAt(dimL);
  out["--muted"] = textAt(mutedL);
  for (const token of ["text", "bright"]) {
    let l = TEXT_NEUTRALS.find((n) => n.token === token)![mode === "light" ? "lightL" : "darkL"];
    for (let i = 0; i < 40 && !clears(textAt(l)); i++) l += stronger * 0.01;
    out[`--${token}`] = textAt(l);
  }
  for (let l = mode === "light" ? 0.5 : 0.72, i = 0; i < 60 && textSurfaces.some((s) => contrastRatio(out["--accent"], s) < 4.5); i++) {
    out["--accent"] = oklch(l, accentC, hue);
    l += stronger * 0.01;
  }
  out["--blue"] = out["--accent"];
  out["--accent-soft"] = scale[mode === "light" ? 50 : 950];
  out["--accent-line"] = scale[mode === "light" ? 200 : 800];
  out["--focus-ring"] = out["--muted"];
  addSurfaceTokens(out);
  return out;
}

export function buildPalette(schemeId: string, mode: ThemeMode): Record<string, string> {
  const scheme = schemeById(schemeId);
  return buildPaletteFromHue(scheme.hue, mode, { lightL: scheme.lightL, darkL: scheme.darkL });
}


export type SchemeSelection =
  | { kind: "preset"; id: string }
  | { kind: "custom"; hue: number; hex?: string };

const CUSTOM_SCHEME_ID = "custom";
const HUE_STORAGE_KEY = "muteki.schemeHue";

export function readSavedSelection(): SchemeSelection {
  const override = readAppearanceOverride(); if (override) return override.selection;
  const shared = readUiPreference("accent", {kind: "preset", id: DEFAULT_SCHEME});
  if (hasLoadedUiPreferences()) return shared;
  try {
    const saved = window.localStorage.getItem(SCHEME_STORAGE_KEY);
    if (saved === CUSTOM_SCHEME_ID) {
      const hue = Number(window.localStorage.getItem(HUE_STORAGE_KEY));
      if (Number.isFinite(hue)) return { kind: "custom", hue: norm360(hue) };
      return { kind: "preset", id: DEFAULT_SCHEME };
    }
    if (saved && SCHEMES.some((s) => s.id === saved)) return { kind: "preset", id: saved };
  } catch {
  }
  return { kind: "preset", id: DEFAULT_SCHEME };
}

export function readSavedTheme(): ThemeMode {
  const override = readAppearanceOverride(); if (override) return override.resolvedTheme;
  let legacy: "light" | "dark" | "system" = "system";
  try { const value = localStorage.getItem("muteki.themePreference") || localStorage.getItem("muteki.theme"); if (value === "light" || value === "dark" || value === "system") legacy = value; } catch {  }
  const preference = readUiPreference("theme", hasLoadedUiPreferences() ? "system" : legacy);
  return preference === "system" ? (typeof window !== "undefined" && window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark") : preference;
}

function paletteForSelection(sel: SchemeSelection, mode: ThemeMode): Record<string, string> {
  return sel.kind === "custom" ? buildPaletteFromHue(sel.hue, mode, { hex: sel.hex }) : buildPalette(sel.id, mode);
}

let lastPalette: { key: string; tokens: Record<string, string> } | undefined;

export function applySelection(sel: SchemeSelection, mode: ThemeMode): void {
  if (typeof document === "undefined") return;
  const scheme = sel.kind === "custom" ? CUSTOM_SCHEME_ID : schemeById(sel.id).id;
  const key = JSON.stringify(sel.kind === "custom" ? [mode, scheme, sel.hue, sel.hex] : [mode, scheme]);
  if (lastPalette?.key !== key) lastPalette = { key, tokens: paletteForSelection(sel, mode) };
  const palette = lastPalette.tokens;
  const root = document.documentElement;
  const themeChanged =
    root.dataset.theme !== mode
    || root.classList.contains("dark") !== (mode === "dark")
    || root.classList.contains("light") !== (mode === "light");
  const commit = () => {
    for (const [token, value] of Object.entries(palette)) {
      if (root.style.getPropertyValue(token) !== value) root.style.setProperty(token, value);
    }
    if (root.style.colorScheme !== mode) root.style.colorScheme = mode;
    const brandLightMix = mode === "light" ? "100%" : "0%";
    if (root.style.getPropertyValue("--eng-brand-light-mix") !== brandLightMix) {
      root.style.setProperty("--eng-brand-light-mix", brandLightMix);
    }
    if (root.dataset.theme !== mode) root.dataset.theme = mode;
    root.classList.toggle("dark", mode === "dark");
    root.classList.toggle("light", mode === "light");
    if (root.dataset.scheme !== scheme) root.dataset.scheme = scheme;
  };
  if (themeChanged) runWithoutThemeTransitions(commit, root);
  else commit();
  updateBrandHead(palette);

}
