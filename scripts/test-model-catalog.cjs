const fs = require('node:fs');
const vm = require('node:vm');
const assert = require('node:assert/strict');
const ts = require('../apps/frontend/node_modules/typescript');
function load(file) {
  const box = { exports: {}, require: () => ({}), process, console, setTimeout, clearTimeout, setInterval, clearInterval };
  vm.runInNewContext(ts.transpileModule(fs.readFileSync(file, 'utf8'), {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText, box, { filename: file });
  return box.exports;
}
const { normalizeConversationCredential, allCredentialModels } = load('apps/frontend/lib/useConversation.ts');
const { credentialForRuntime } = load('apps/frontend/lib/modelReasoning.ts');
function verify(raw, runtime, expected) {
  const normalized = normalizeConversationCredential(raw);
  assert(normalized);
  for (const scoped of [false, true]) {
    const result = credentialForRuntime(normalized, runtime, scoped);
    const ids = allCredentialModels(result).map(model => model.id);
    assert.equal(JSON.stringify(ids), JSON.stringify(expected), `${raw.engine} scoped=${scoped}`);
    assert(!result.default_model || ids.includes(result.default_model));
  }
}
const inventory = Array.from({ length: 26 }, (_, i) => ({ id: `endpoint-${i}` }));
for (const engine of ['codex', 'claude', 'cursor', 'grok', 'opencode', 'pi', 'kimi', 'omp', 'devin', 'droid']) {
  const runtime = `${engine}:default`;
  verify({ id: `account:${engine}`, engine, connection: 'custom_endpoint',
    default_model: 'muteki/old:cloud', models: ['muteki/old:cloud'],
    candidate_models: ['muteki/old:cloud', ...inventory],
    model_catalogs: { [runtime]: { discovered_models: inventory, verified_models: ['muteki/old:cloud', 'endpoint-0'] } },
  }, runtime, inventory.map(model => model.id));
}
const manual = normalizeConversationCredential({ id: 'account:pi', engine: 'pi', connection: 'custom_endpoint',
  candidate_models: ['manual-model'], default_model: 'manual-model' });
assert.equal(allCredentialModels(credentialForRuntime(manual, 'pi:default', false))[0].id, 'manual-model');
console.log('All 10 agents: 26 discovered models stay 26 despite stale declared/default/verified model; manual fallback preserved.');
if (process.argv.includes('--live')) (async () => {
  const { credentials } = await (await fetch('http://127.0.0.1:8100/api/settings/credentials')).json();
  const expectedCounts = { 'pi-ollama': 26, 'omp-ollama': 26, 'opencode-ollama': 26, 'chat-qa-claude-vision': 26, deepseek: 2, 'cursor-main': 47 };
  for (const raw of credentials) {
    const count = expectedCounts[raw.account_id];
    if (!count) continue;
    for (const [runtime, catalog] of Object.entries(raw.model_catalogs || {})) {
      const expected = catalog.discovered_models.map(model => model.id);
      if (raw.connection === 'custom_endpoint') verify(raw, runtime, expected);
      assert.equal(expected.length, count, `${raw.account_id} ${runtime}`);
    }
  }
  console.log('Live product credentials: settings/chat model projection matches every fetched endpoint catalog.');
})().catch(error => { console.error(error); process.exitCode = 1; });
