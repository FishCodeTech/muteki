#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
for (const name of ['pre-commit', 'pre-push']) {
  const hook = path.join(root, '.githooks', name);
  fs.chmodSync(hook, 0o755);
}

const res = spawnSync('git', ['config', 'core.hooksPath', '.githooks'], {
  cwd: root,
  encoding: 'utf8',
});
if (res.status !== 0) {
  throw new Error(`Failed to set core.hooksPath: ${(res.stderr || res.stdout || '').trim()}`);
}

const verified = spawnSync('git', ['config', '--get', 'core.hooksPath'], {
  cwd: root,
  encoding: 'utf8',
}).stdout.trim();

console.log(`Git hooks configured: core.hooksPath=${verified}`);
console.log('  - pre-commit: enforces Lint + typecheck + contracts + Rust/Frontend property tests on staged code changes');
console.log('  - pre-push:   runs scripts/verify/pr_gate.mjs --local-push (VERIFY_GATE=observe by default; set VERIFY_GATE=enforce to block)');
