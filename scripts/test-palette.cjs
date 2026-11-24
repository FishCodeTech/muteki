const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const ts = require('../apps/frontend/node_modules/typescript');
const path = require('node:path');
function load(file) {
 const source = fs.readFileSync(file, 'utf8');
 const box = { exports: {}, require: name => name === 'chroma-js' ? require('../apps/frontend/node_modules/chroma-js') : name.endsWith('.json') ? JSON.parse(fs.readFileSync(path.resolve(path.dirname(file), name))) : name === './color-scale' ? load(path.resolve(path.dirname(file), 'color-scale.ts')).exports : {} };
 vm.runInNewContext(ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, esModuleInterop: true } }).outputText, box);
 return box;
}
const box = load('apps/frontend/lib/palette-engine.ts');
const chroma = require('../apps/frontend/node_modules/chroma-js').default;
const { buildPaletteFromHue, buildColorScale, SHADE_STEPS } = box.exports;
const lum = hex => {
 const rgb = hex.slice(1).match(/../g).map(x => parseInt(x, 16) / 255).map(x => x <= .04045 ? x / 12.92 : ((x + .055) / 1.055) ** 2.4);
 return rgb[0] * .2126 + rgb[1] * .7152 + rgb[2] * .0722;
};
const contrast = (a,b) => (Math.max(lum(a),lum(b))+.05)/(Math.min(lum(a),lum(b))+.05);
for (const mode of ['light','dark']) {
 const baseline = buildPaletteFromHue(268, mode);
 for (let hue=0; hue<360; hue++) {
  const p = buildPaletteFromHue(hue,mode);
  const scale = buildColorScale(hue);
  for (let i=1;i<SHADE_STEPS.length;i++) assert(chroma(scale[SHADE_STEPS[i-1]]).get("hsl.l")>=chroma(scale[SHADE_STEPS[i]]).get("hsl.l"), `non-monotonic scale ${hue}`);
  for (const state of ['green','amber','red','gold']) assert.equal(p[`--${state}`],baseline[`--${state}`]);
  for (const token of ['text','bright','muted','dim','accent']) for (const bg of ['bg','panel','panel2','panel3','rail']) assert(contrast(p[`--${token}`],p[`--${bg}`])>=4.5, `${mode}/${hue}/${token}/${bg}`);
  assert(contrast(p['--accent'],p['--on-accent'])>=4.5);
 }
}
console.log('720 hue/theme palettes: ordered HSL scales, fixed semantic colors and AA contrast passed.');
const { generateColorScale } = load('apps/frontend/lib/color-scale.ts').exports;
assert.deepEqual(Array.from(Object.values(generateColorScale('#f49d0c'))), ['#fffbeb','#fef3c7','#fde68a','#fbd24e','#fabe25','#f49d0c','#d87607','#b3530a','#91400f','#78350f','#451a03']);
for (const hex of ['#000000','#ffffff','#808080','#14b8a6','#ff00ff','#ffff00']) {
 assert(Object.values(generateColorScale(hex)).includes(hex), `lost input ${hex}`);
 for (const mode of ['light','dark']) {
  const p=buildPaletteFromHue(0,mode,{hex});
  for(const bg of ['bg','panel','panel2','panel3','rail']) assert(contrast(p['--accent'],p[`--${bg}`])>=4.5);
  assert(contrast(p['--accent'],p['--on-accent'])>=4.5);
 }
}
console.log('Public frontend reference fixture, exact input retention and custom-color contrast passed.');
for (const [hex, expected] of Object.entries(require('./fixtures/uicolors-default-scales.json'))) {
 assert.equal(JSON.stringify(generateColorScale(hex)), JSON.stringify(expected), `reference mismatch: ${hex}`);
}
console.log('44 UI Colors reference inputs / 484 exact shade values passed.');
