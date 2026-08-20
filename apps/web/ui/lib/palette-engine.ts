/**
 * palette-engine.ts — scheme-driven color engine for the command deck.
 *
 * A color scheme is defined by a single accent HUE (OKLCH). From it the engine
 * derives the full accent family (--blue/--accent, --green, --amber, --cyan,
 * --pink, --violet, --magenta, --red, --gold) for both light and dark mode:
 *
 *   · Semantic hues (green/amber/red/gold) are FIXED — they carry meaning
 *     (success / warning / error / flag) and must survive every scheme.
 *   · Decorative hues (cyan/violet/magenta/pink) are absolute targets, but any
 *     role that lands within MIN_GAP of the accent (or an already-assigned
 *     role) is rotated to the nearest free hue, so e.g. the violet scheme
 *     auto-relocates the evidence-lane violet instead of clashing with it.
 *   · Lightness/chroma follow per-mode curves; out-of-gamut colors are
 *     chroma-clamped into sRGB, and the accent is nudged until it clears
 *     WCAG 4.5:1 against the panel background (which also guarantees the
 *     --on-accent text pair).
 *
 * Output is applied as inline custom properties on <html>, overriding the
 * static fallback palettes in globals.css. The selection persists in
 * localStorage ("muteki.scheme") and is mirrored to <html data-scheme>.
 */

export type ThemeMode = "light" | "dark";

export interface SchemeDef {
  id: string;
  /** OKLCH hue of the primary accent, degrees 0–360. */
  hue: number;
  /** i18n key for the display name (see lib/i18n.tsx). */
  labelKey: string;
  /** Optional per-mode accent lightness overrides. */
  lightL?: number;
  darkL?: number;
}

export const SCHEMES: SchemeDef[] = [
  { id: "azure", hue: 268, labelKey: "scheme.azure" },
  { id: "violet", hue: 296, labelKey: "scheme.violet", darkL: 0.76 },
  { id: "teal", hue: 183, labelKey: "scheme.teal" },
  { id: "ember", hue: 53, labelKey: "scheme.ember" },
];

export const DEFAULT_SCHEME = "azure";
export const SCHEME_STORAGE_KEY = "muteki.scheme";

/* ── OKLCH → sRGB ─────────────────────────────────────────────────────────── */

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

/** Chroma-clamp an OKLCH color into the sRGB gamut (hue & lightness preserved). */
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

export function oklch(l: number, c: number, h: number): string {
  return rgbToHex(clampToGamut(l, c, h));
}

/* ── WCAG contrast ────────────────────────────────────────────────────────── */

function hexToLinear(hex: string): [number, number, number] {
  const v = parseInt(hex.slice(1), 16);
  const f = (x: number) => {
    const s = x / 255;
    return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return [f((v >> 16) & 255), f((v >> 8) & 255), f(v & 255)];
}

export function luminance(hex: string): number {
  const [r, g, b] = hexToLinear(hex);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function contrastRatio(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

/* ── hue roles & collision resolution ─────────────────────────────────────── */

const norm360 = (h: number) => ((h % 360) + 360) % 360;
const hueDist = (a: number, b: number) => Math.abs(((a - b + 540) % 360) - 180);

/** Minimum circular separation between the accent and any role, degrees. */
const MIN_GAP_ACCENT = 26;
/** Minimum separation between two assigned roles. */
const MIN_GAP_ROLE = 22;

/**
 * Rotate `target` to the nearest hue that clears `minGap` from every hue in
 * `taken`; if nothing within ±60° qualifies, take the candidate with the
 * largest minimum gap (deterministic, so palettes are stable across loads).
 */
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
  /** Token name without the leading `--`. */
  token: string;
  /** Absolute OKLCH hue target; null = follow the scheme accent. */
  hue: number | null;
  lightL: number;
  lightC: number;
  darkL: number;
  darkC: number;
  /** When true the hue may be relocated if it collides (decorative roles). */
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

/** Panel backgrounds from globals.css — the surfaces accent text sits on. */
const PANEL_BG: Record<ThemeMode, string> = { light: "#ffffff", dark: "#1b1f24" };
const ACCENT_MIN_CONTRAST = 4.5;

/**
 * Neutral ramps, as OKLCH lightness measured from the static globals.css
 * palettes. The engine re-emits every neutral with a whisper of the accent
 * hue (SURFACE_C / LINE_C), so the "white" of each scheme feels designed
 * rather than dead — lightness never moves, only chroma.
 */
const TEXT_NEUTRALS: { token: string; lightL: number; darkL: number }[] = [
  { token: "text", lightL: 0.27, darkL: 0.90 },
  { token: "bright", lightL: 0.22, darkL: 0.95 },
  { token: "muted", lightL: 0.51, darkL: 0.70 },
  { token: "dim", lightL: 0.64, darkL: 0.52 },
];

const NEUTRALS: { token: string; lightL: number; darkL: number; line?: boolean }[] = [
  { token: "bg", lightL: 0.9789, darkL: 0.2076 },
  { token: "rail", lightL: 0.9662, darkL: 0.1943 },
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

/**
 * Engine-identity hues (OKLCH). Fixed per engine so a worker reads as the
 * same colour in every scheme; saturation/lightness follow the mode curves
 * via hueColor(). Emitted as --eng-* tokens AND used by lib/workers.ts, so
 * the two can never drift apart.
 */
export const ENGINE_HUE: Record<string, number> = {
  claude: 45,
  codex: 165,
  cursor: 290,
  pi: 200,
  omp: 80,
  kimi: 240,
  grok: 320,
  opencode: 180,
  dsh: 350,
  reason: 265,
  deepseek: 265,
  verifier: 120,
};

/**
 * A saturated, mode-correct colour for an arbitrary hue — used for engine
 * identity dots/marks. `chromaScale` < 1 yields a quieter (slate) variant.
 */
export function hueColor(hue: number, mode: ThemeMode, chromaScale = 1): string {
  return mode === "light"
    ? oklch(0.52, 0.16 * chromaScale, hue)
    : oklch(0.74, 0.14 * chromaScale, hue);
}

/** Current mode from <html data-theme>; dark matches the app default. */
export function currentMode(): ThemeMode {
  if (typeof document === "undefined") return "dark";
  return document.documentElement.dataset.theme === "light" ? "light" : "dark";
}

function schemeById(id: string): SchemeDef {
  return SCHEMES.find((s) => s.id === id) ?? SCHEMES[0];
}

/**
 * Core palette builder: derive the full token map from an accent hue (OKLCH,
 * degrees) for one mode. This is the "universal" entry point — presets are
 * just named hues, and the settings page can feed ANY hue from the slider.
 */
export function buildPaletteFromHue(
  hue: number,
  mode: ThemeMode,
  opts: { lightL?: number; darkL?: number } = {},
): Record<string, string> {
  const out: Record<string, string> = {};

  // accent — nudge lightness until it clears WCAG AA against the panel.
  let accentL = mode === "light" ? opts.lightL ?? 0.53 : opts.darkL ?? 0.74;
  const accentC = mode === "light" ? 0.2 : 0.15;
  let accent = oklch(accentL, accentC, hue);
  for (let i = 0; i < 40 && contrastRatio(accent, PANEL_BG[mode]) < ACCENT_MIN_CONTRAST; i++) {
    accentL += mode === "light" ? -0.01 : 0.01;
    if (accentL < 0.25 || accentL > 0.92) break;
    accent = oklch(accentL, accentC, hue);
  }
  out["--blue"] = accent;
  out["--accent"] = accent;
  out["--on-accent"] = mode === "light" ? "#ffffff" : "#0b0e12";

  // roles — semantic hues fixed, decorative hues relocate on collision.
  const taken: number[] = [hue];
  for (const role of ROLES) {
    const target = role.hue ?? hue;
    const gap = role.semantic ? MIN_GAP_ACCENT : MIN_GAP_ROLE;
    const resolved = resolveHue(target, taken, role.semantic ? MIN_GAP_ACCENT : gap);
    taken.push(resolved);
    out[`--${role.token}`] = oklch(
      mode === "light" ? role.lightL : role.darkL,
      mode === "light" ? role.lightC : role.darkC,
      resolved,
    );
  }
  out["--yellow"] = out["--amber"];

  // neutrals — same lightness as the static palettes, tinted with a whisper
  // of the accent hue so surfaces feel cohesive instead of dead white/gray.
  for (const n of NEUTRALS) {
    out[`--${n.token}`] = oklch(
      mode === "light" ? n.lightL : n.darkL,
      n.line ? LINE_C[mode] : SURFACE_C[mode],
      hue,
    );
  }
  for (const n of TEXT_NEUTRALS) {
    out[`--${n.token}`] = oklch(mode === "light" ? n.lightL : n.darkL, SURFACE_C[mode], hue);
  }

  // engine identity colours — fixed hues, mode-correct saturation.
  for (const [engine, engineHue] of Object.entries(ENGINE_HUE)) {
    out[`--eng-${engine}`] = hueColor(engineHue, mode);
  }

  // human (operator) bubble — a whisper of the accent hue, not a gray.
  if (mode === "light") {
    out["--human-bg"] = oklch(0.95, 0.02, hue);
    out["--human-border"] = oklch(0.87, 0.03, hue);
  } else {
    out["--human-bg"] = oklch(0.26, 0.03, hue);
    out["--human-border"] = oklch(0.38, 0.05, hue);
  }
  return out;
}

/**
 * Build the full token map for one preset scheme + mode. Keys are CSS custom
 * property names (with leading `--`); values are hex colors.
 */
export function buildPalette(schemeId: string, mode: ThemeMode): Record<string, string> {
  const scheme = schemeById(schemeId);
  return buildPaletteFromHue(scheme.hue, mode, { lightL: scheme.lightL, darkL: scheme.darkL });
}

/* ── application & persistence ────────────────────────────────────────────── */

/** A saved selection is either a preset id or a free hue from the slider. */
export type SchemeSelection =
  | { kind: "preset"; id: string }
  | { kind: "custom"; hue: number };

export const CUSTOM_SCHEME_ID = "custom";
const HUE_STORAGE_KEY = "muteki.schemeHue";

export function readSavedSelection(): SchemeSelection {
  try {
    const saved = window.localStorage.getItem(SCHEME_STORAGE_KEY);
    if (saved === CUSTOM_SCHEME_ID) {
      const hue = Number(window.localStorage.getItem(HUE_STORAGE_KEY));
      if (Number.isFinite(hue)) return { kind: "custom", hue: norm360(hue) };
      return { kind: "preset", id: DEFAULT_SCHEME };
    }
    if (saved && SCHEMES.some((s) => s.id === saved)) return { kind: "preset", id: saved };
  } catch {
    /* storage unavailable — fall through to default */
  }
  return { kind: "preset", id: DEFAULT_SCHEME };
}

export function readSavedScheme(): string {
  const sel = readSavedSelection();
  return sel.kind === "preset" ? sel.id : CUSTOM_SCHEME_ID;
}

export function readSavedTheme(): ThemeMode {
  try {
    if (window.localStorage.getItem("muteki.theme") === "light") return "light";
  } catch {
    /* keep dark */
  }
  return "dark";
}

/** Token map for a selection (preset or free hue) in one mode. */
export function paletteForSelection(sel: SchemeSelection, mode: ThemeMode): Record<string, string> {
  return sel.kind === "custom" ? buildPaletteFromHue(sel.hue, mode) : buildPalette(sel.id, mode);
}

/**
 * Generate the palette for a selection + mode and apply it as inline custom
 * properties on <html>, overriding the static fallbacks in globals.css.
 * Persists the selection and mirrors it to <html data-scheme>.
 */
export function applySelection(sel: SchemeSelection, mode: ThemeMode): void {
  if (typeof document === "undefined") return;
  const palette = paletteForSelection(sel, mode);
  const root = document.documentElement;
  for (const [token, value] of Object.entries(palette)) root.style.setProperty(token, value);
  root.dataset.theme = mode;
  root.dataset.scheme = sel.kind === "custom" ? CUSTOM_SCHEME_ID : schemeById(sel.id).id;
  try {
    window.localStorage.setItem(SCHEME_STORAGE_KEY, root.dataset.scheme);
    if (sel.kind === "custom") window.localStorage.setItem(HUE_STORAGE_KEY, String(Math.round(sel.hue)));
  } catch {
    /* theming still works for this session */
  }
}

/** Back-compat shorthand: apply a preset scheme id. */
export function applyScheme(schemeId: string, mode: ThemeMode): void {
  applySelection({ kind: "preset", id: schemeId }, mode);
}
