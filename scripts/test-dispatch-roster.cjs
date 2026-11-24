const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');
const assert = require('node:assert/strict');
const ts = require('../apps/frontend/node_modules/typescript');
function load(file) {
  const box = { exports: {}, require: name => load(path.resolve(path.dirname(file), name + '.ts')) };
  vm.runInNewContext(ts.transpileModule(fs.readFileSync(file, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, box);
  return box.exports;
}
const { dispatchRoster } = load(path.resolve('apps/frontend/lib/dispatchRoster.ts'));
const deck = { runId: 'draft-test', category: '', lanes: {}, chat: [], degradedEngines: {} };
const settings = { engines: ['pi-a'], worker_profiles: [
  { id: 'pi-a', engine: 'pi', label: 'Pi A', enabled: true, model: 'a' },
  { id: 'pi-b', engine: 'pi', label: 'Pi B', enabled: true, model: 'b' },
  { id: 'kimi', engine: 'kimi', enabled: false },
] };
const keys = rows => Array.from(rows, row => row.key);
assert.deepEqual(keys(dispatchRoster(deck, settings)), ['pi-a']);
assert.deepEqual(keys(dispatchRoster(deck, { ...settings, engines: ['pi'] })), ['pi-a', 'pi-b']);
assert.equal(dispatchRoster(deck, { ...settings, engines: ['kimi'] }).length, 0);
assert.equal(dispatchRoster(deck, { ...settings, worker_profiles: settings.worker_profiles.map(p => ({ ...p, enabled: false })) }).length, 0);
assert.equal(dispatchRoster(deck, { ...settings, engines: [] }).length, 0);
assert.deepEqual(keys(dispatchRoster({ ...deck, category: 'web' }, { ...settings, overrides: { web: { engines: ['pi-b'] } } })), ['pi-b']);
assert.equal(dispatchRoster(deck, { ...settings, seats: [{ id: 'pi-a', engine: 'pi', label: 'Seat', enabled: false }] }).length, 0);
const run = { ...deck, runId: 'run-old', lanes: {
  reason: { solverId: 'reason', online: true },
  'cli-pi': { solverId: 'cli-pi', engine: 'pi', profileId: 'pi-a', profileLabel: 'Previous Pi', online: true, model: 'a' },
  'cli-pi-g2': { solverId: 'cli-pi-g2', engine: 'pi', profileId: 'pi-b', online: true, model: 'b' },
}, executionGeneration: 2 };
assert.deepEqual(keys(dispatchRoster(run, settings)), ['cli-pi-g2']);
assert.equal(dispatchRoster(run, settings)[0].state, 'online');
assert.equal(dispatchRoster({ ...run, finished: true }, settings)[0].state, 'finished');
assert.equal(dispatchRoster({ ...run, lanes: {}, chat: [] }, settings).length, 0);
const degraded = dispatchRoster({ ...run, degradedEngines: { 'pi-b': 'failed', codex: 'auth failed' } }, settings);
assert.equal(degraded.length, 2);
assert.equal(degraded[0].state, 'degraded');
assert.deepEqual(keys(dispatchRoster(run, { ...settings, engines: ['kimi'], worker_profiles: [] })), ['cli-pi-g2']);
console.log('Dispatch roster: selected/enabled seats, same-engine profiles, category overrides, empty rosters, run isolation, generation filtering and degradation passed.');
if (process.argv.includes('--live')) (async () => {
  const { config } = await (await fetch('http://127.0.0.1:8100/api/settings/workers')).json();
  const rows = dispatchRoster(deck, config);
  console.log(JSON.stringify({ liveDraftRoster: rows }));
})().catch(error => { console.error(error); process.exitCode = 1; });
