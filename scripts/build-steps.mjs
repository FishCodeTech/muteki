import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { access, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
export const frontend = path.join(root, 'apps/frontend');
const mutekiUi = path.join(root, 'packages/muteki-ui');
const stampName = 'muteki-build-inputs.sha256';
const generated = new Set(['next-env.d.ts']);

async function gitFiles(cwd) {
  const listed = spawnSync('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', '.'], { cwd, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  if (listed.error || listed.status !== 0) return { files: null, reason: `git ls-files failed: ${listed.error?.message || listed.stderr}` };
  return { files: listed.stdout.split('\0').filter(Boolean) };
}

async function inputsHash(distDir, env) {
  const fe = await gitFiles(frontend);
  if (!fe.files) return { hash: null, reason: fe.reason };
  const ui = await gitFiles(mutekiUi);
  if (!ui.files) return { hash: null, reason: ui.reason };
  const files = fe.files.filter(file => !generated.has(file)).map(file => ['apps/frontend', frontend, file]);
  for (const file of ui.files) files.push(['packages/muteki-ui', mutekiUi, file]);
  for (const entry of await readdir(frontend)) {
    if (entry.startsWith('.env') && !fe.files.includes(entry)) files.push(['apps/frontend', frontend, entry]);
  }
  files.push(['apps/frontend', frontend, 'node_modules/.package-lock.json']);
  files.sort((a, b) => `${a[0]}/${a[2]}`.localeCompare(`${b[0]}/${b[2]}`));
  const hash = createHash('sha256');
  const { uiBuildId } = createRequire(import.meta.url)(path.join(frontend, 'build-info.cjs'));
  const relevantEnv = Object.keys(env).filter(key => key.startsWith('NEXT_PUBLIC_') || ['MUTEKI_RELEASE_GIT_REF', 'NODE_ENV'].includes(key)).sort();
  hash.update(JSON.stringify({ distDir, node: process.version, ui: uiBuildId(), env: relevantEnv.map(key => [key, env[key]]) }));
  for (const [scope, base, file] of files) {
    hash.update(`\0${scope}/${file}\0`);
    try { hash.update(await readFile(path.join(base, file))); }
    catch (error) { if (error.code !== 'ENOENT') throw error; hash.update('\0missing'); }
  }
  return { hash: hash.digest('hex') };
}

async function exists(file) {
  return access(file).then(() => true, () => false);
}

export async function buildFrontend({ distDir = '.next', env = process.env } = {}) {
  const output = path.join(frontend, distDir);
  const stamp = path.join(output, stampName);
  const { hash, reason } = await inputsHash(distDir, env);
  const force = env.MUTEKI_FRONTEND_REBUILD === '1';
  const hasStandalone = await exists(path.join(output, 'standalone/server.js'))
    || await exists(path.join(output, 'standalone/apps/frontend/server.js'));
  if (hash && !force && await exists(path.join(output, 'BUILD_ID')) && hasStandalone) {
    const previous = await readFile(stamp, 'utf8').catch(() => '');
    if (previous.trim() === hash) {
      console.log(`Frontend unchanged; reusing apps/frontend/${distDir} (MUTEKI_FRONTEND_REBUILD=1 forces a rebuild).`);
      return;
    }
  }
  if (!hash) console.log(`Frontend build reuse unavailable (${reason}); rebuilding.`);
  await rm(stamp, { force: true });
  await new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [path.join(frontend, 'node_modules/next/dist/bin/next'), 'build'], { cwd: frontend, env: { ...env, MUTEKI_NEXT_DIST_DIR: distDir, NEXT_TELEMETRY_DISABLED: '1' }, stdio: 'inherit' });
    child.on('error', reject);
    child.on('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`Next frontend build failed (${code ?? signal}).`)));
  });
  if (hash) await writeFile(stamp, `${hash}\n`);
}

export function cargo(arguments_, env = process.env) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [path.join(root, 'scripts/cargo.mjs'), ...arguments_], { cwd: root, env, stdio: 'inherit' });
    child.on('error', reject);
    child.on('exit', (code, signal) => code === 0 ? resolve() : reject(new Error(`Rust build failed (${code ?? signal}).`)));
  });
}
