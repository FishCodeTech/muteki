// Real Git + Cargo over LayerFS, with Commit/remount and fork isolation.
import assert from 'node:assert/strict';
import { readFileSync, writeFileSync } from 'node:fs';
import { performance } from 'node:perf_hooks';
import { LayerFS } from '/layerfs-sdk/src/index.js';

const settings = JSON.parse(readFileSync('/layerfs-state/settings.json', 'utf8'));
const client = new LayerFS(settings.project, {
  storePath: '/layerfs-store/global/store.sqlite',
  trace: '/layerfs-state/trace.jsonl',
});
// v1.0.0's constructor takes these deployment fields from the environment.
// Its pinned client stores them in options; set them explicitly for this trial.
client.options.home = '/layerfs-state/home';
client.options.deploy = {
  docker: '/var/run/docker.sock', image: settings.image,
  volume: settings.volume, daemon: '/layerfs-daemon',
};
const result = { ...settings, steps: [] };
async function step(name, action) {
  const start = performance.now();
  const value = await action();
  result.steps.push({ name, ms: Math.round(performance.now() - start), value });
  writeFileSync('/layerfs-state/result.json', JSON.stringify(result, null, 2));
  console.log(`${name}: ${result.steps.at(-1).ms} ms`);
  return value;
}
async function exec(ws, command) {
  const value = await client.workspace.exec(ws, command, undefined, {
    stdout: process.stdout, stderr: process.stderr,
  });
  assert.equal(value.exitCode, 0, JSON.stringify(value));
  return { exitCode: value.exitCode };
}
let workspace;
try {
  await step('init', () => client.project.init('/layerfs-input'));
  await step('fork', () => client.branch.fork('build'));
  workspace = await step('mount', () => client.workspace.mount('build'));
  await step('cold-test', () => exec(workspace,
    "set -eu; test -z \"$(git --no-optional-locks status --porcelain)\"; cargo +1.98.1 test --locked -p nsh-package; mkdir -p .nsh; printf 'snapshot-ok\\n' > .nsh/layerfs-probe.txt"));
  await step('commit', () => client.workspace.commit(workspace, 'Cache NSH package tests and probe'));
  await step('unmount', () => client.workspace.unmount(workspace));
  workspace = undefined;
  await step('fork-warm', () => client.branch.fork('warm', 'build'));
  workspace = await step('remount', () => client.workspace.mount('warm'));
  await step('warm-test', () => exec(workspace,
    "set -eu; test -d target/debug/deps; test \"$(cat .nsh/layerfs-probe.txt)\" = snapshot-ok; cargo +1.98.1 test --locked --offline -p nsh-package"));
  await step('unmount-warm', () => client.workspace.unmount(workspace));
  workspace = undefined;
  await step('history', async () => {
    const history = await client.branch.list_commits('build');
    assert.ok(history.items.length > 0);
    return history;
  });
  await step('fork-untouched', () => client.branch.fork('untouched'));
  workspace = await step('mount-main', () => client.workspace.mount('untouched'));
  await step('isolation', () => exec(workspace,
    'set -eu; test ! -e .nsh/layerfs-probe.txt; test ! -d target; test -z "$(git --no-optional-locks status --porcelain)"'));
  await step('unmount-main', () => client.workspace.unmount(workspace));
  workspace = undefined;
  result.ok = true;
} catch (error) {
  // Preserve the failed Workspace and Store for diagnosis; never replay work.
  result.ok = false;
  result.error = { message: error.message, code: error.code, detail: error.detail, workspace };
  throw error;
} finally {
  await client.close();
  writeFileSync('/layerfs-state/result.json', JSON.stringify(result, null, 2));
}
