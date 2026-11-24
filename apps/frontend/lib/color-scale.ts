import chroma from 'chroma-js';
import references from './color-reference-scales.json';
import extraReferences from './color-reference-extra.json';

// Reference values: Tailwind CSS 3.4.17 (MIT), tailwindlabs/tailwindcss.
// Independently implemented from the observable default UI Colors generator:
// nearest family (CIEDE2000), nearest HSL lightness anchor, hue offset and
// saturation ratio. Additional two reference ramps are observed color values
// from https://uicolors.app/generate (2026-11-05).
export const SHADE_STEPS = [50, 100, 200, 300, 400, 500, 600, 700, 800, 900, 950] as const;
export function generateColorScale(input: string): Record<number, string> {
  const color = chroma(input);
  const [hue, saturation, lightness] = color.hsl();
  const neutral = saturation <= 0.15;
  const families = Object.entries({ ...references, ...extraReferences }).filter(([name]) => ['slate','gray','zinc','stone'].includes(name) === neutral);
  const family = families.reduce((best, candidate) => {
    const distance = (entry: typeof candidate) => Math.min(...Object.values(entry[1]).map(hex => chroma.deltaE(color, hex)));
    return distance(candidate) < distance(best) ? candidate : best;
  })[1];
  const anchor = SHADE_STEPS.reduce((best, step) => Math.abs(chroma(family[step]).get('hsl.l') - lightness) < Math.abs(chroma(family[best]).get('hsl.l') - lightness) ? step : best);
  const base = chroma(family[anchor]);
  const offset = (Number.isFinite(hue) ? hue : 0) - (base.get('hsl.h') || 0);
  const ratio = saturation / base.get('hsl.s');
  return Object.fromEntries(SHADE_STEPS.map(step => {
    if (step === anchor) return [step, color.hex()];
    const reference = chroma(family[step]);
    let shiftedHue = ((reference.get('hsl.h') || 0) + offset) % 360;
    if (shiftedHue < 0) shiftedHue += 360;
    return [step, chroma(reference.set('hsl.s', reference.get('hsl.s') * ratio).hex()).set('hsl.h', shiftedHue).hex()];
  }));
}
export function colorHue(hex: string): number { return chroma(hex).get('oklch.h') || 0; }

export function colorChroma(hex: string): number { return chroma(hex).get("oklch.c"); }
