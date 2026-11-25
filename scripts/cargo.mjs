#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { mkdirSync } from 'node:fs';
import { availableParallelism, totalmem } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
mkdirSync(path.join(root, '.cache'), { recursive: true });
const argv = process.argv.slice(2);
const env = { ...process.env };
if (!env.CARGO_BUILD_JOBS && !argv.some(arg => arg === '-j' || arg.startsWith('-j') || arg.startsWith('--jobs'))) {
  env.CARGO_BUILD_JOBS = String(Math.max(1, Math.min(availableParallelism(), Math.floor(totalmem() / (3 * 1024 ** 3)))));
}

if (env.RUSTC_WRAPPER === undefined) {
  const uds = path.join(root, '.cache/sccache.sock');
  if (Buffer.byteLength(uds) < 95) {
    const sccacheEnv = {
      ...env,
      SCCACHE_DIR: env.SCCACHE_DIR || path.join(root, '.cache/sccache'),
      SCCACHE_SERVER_UDS: env.SCCACHE_SERVER_UDS || uds,
      SCCACHE_CACHE_SIZE: env.SCCACHE_CACHE_SIZE || '8G',
    };
    const started = spawnSync('sccache', ['--start-server'], { cwd: root, env: sccacheEnv, encoding: 'utf8' });
    const output = (started.stdout ?? '') + (started.stderr ?? '');
    if (!started.error && (started.status === 0 || output.includes('already running'))) {
      Object.assign(env, sccacheEnv, { RUSTC_WRAPPER: 'sccache' });
    }
  }
}

function pruneIncremental() {
  const cleanup = spawnSync('python3', [path.join(root, 'scripts/prune-build-cache.py'), '--apply'], { cwd: root, stdio: 'inherit' });
  if (cleanup.error || cleanup.status !== 0) {
    console.error(`Build cache cleanup failed: ${cleanup.error?.message ?? cleanup.status}`);
  }
}

const builds = ['build', 'b', 'check', 'c', 'run', 'r', 'rustc', 'test', 't'].includes(argv[0]);
if (builds) pruneIncremental();
const result = spawnSync('cargo', argv, { cwd: root, env, stdio: 'inherit' });
if (result.error) console.error(result.error.message);
if (builds) {
  try {
    pruneIncremental();
  } catch (error) {
    console.error(`Incremental cache cleanup skipped: ${error.message}`);
  }
}
process.exit(result.status ?? 1);
