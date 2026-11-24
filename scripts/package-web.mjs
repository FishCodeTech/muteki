#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { access, chmod, copyFile, cp, mkdtemp, mkdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const args = new Set();
const options = {};
for (const arg of process.argv.slice(2)) {
  const [key, value] = arg.split(/=(.*)/s, 2);
  if (['--version', '--output', '--server-bin', '--node-bin'].includes(key) && value) options[key] = value;
  else if (['--skip-build', '--debug', '--help'].includes(arg)) args.add(arg);
  else throw new Error(`Unknown argument: ${arg}`);
}
if (args.has('--help')) {
  console.log('Usage: node scripts/package-web.mjs [--skip-build] [--debug] [--version=X.Y.Z] [--output=DIR] [--server-bin=PATH] [--node-bin=PATH]\nCreates Muteki-Web-<version>-<darwin|linux>-<arm64|amd64>.tar.gz.');
  process.exit(0);
}
if (!['darwin', 'linux'].includes(process.platform) || !['arm64', 'x64'].includes(process.arch)) {
  throw new Error(`Unsupported Web release target: ${process.platform}/${process.arch}`);
}

function run(command, argv, cwd = root, env = process.env) {
  const result = spawnSync(command, argv, { cwd, env, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} failed (${result.status ?? result.signal})`);
}
function gitRef() {
  if (process.env.MUTEKI_RELEASE_GIT_REF) return process.env.MUTEKI_RELEASE_GIT_REF;
  for (const argv of [['describe', '--tags', '--exact-match'], ['symbolic-ref', '--short', 'HEAD']]) {
    const result = spawnSync('git', argv, { cwd: root, encoding: 'utf8' });
    if (result.status === 0 && result.stdout.trim()) return result.stdout.trim();
  }
  return process.env.GITHUB_REF_NAME || 'working-tree';
}

const manifest = await readFile(path.join(root, 'crates/server/Cargo.toml'), 'utf8');
const crateVersion = /^version\s*=\s*"([^"]+)"/m.exec(manifest)?.[1];
const version = options['--version'] || crateVersion;
if (!/^\d+\.\d+\.\d+$/.test(version || '')) throw new Error(`Invalid Web release version: ${version}`);
const platform = process.platform;
const arch = process.arch === 'x64' ? 'amd64' : 'arm64';
const profile = args.has('--debug') ? 'debug' : 'release';
const target = path.resolve(process.env.CARGO_TARGET_DIR || path.join(root, '.cache/target'));
const serverBin = path.resolve(options['--server-bin'] || path.join(target, profile, 'muteki-server'));
const nodeBin = path.resolve(options['--node-bin'] || process.execPath);
const frontend = path.join(root, 'apps/frontend');
const standalone = path.join(frontend, '.next/standalone');
const output = path.resolve(options['--output'] || path.join(root, 'dist/web-releases'));
const name = `Muteki-Web-${version}-${platform}-${arch}.tar.gz`;
const wrapper = `muteki-web-${version}`;

if (!args.has('--skip-build')) {
  run('cargo', ['build', '-p', 'muteki-server', ...(profile === 'release' ? ['--release'] : [])], root,
    { ...process.env, CARGO_TARGET_DIR: target });
  run('npm', ['--prefix', 'apps/frontend', 'run', 'build']);
}
async function exists(file) {
  return access(file).then(() => true, () => false);
}

async function resolveStandaloneRoot(standaloneDir) {
  if (await exists(path.join(standaloneDir, 'server.js'))) return { mode: 'flat', appDir: standaloneDir };
  const nested = path.join(standaloneDir, 'apps/frontend');
  if (await exists(path.join(nested, 'server.js'))) return { mode: 'nested', appDir: nested };
  throw new Error(`Missing standalone server.js under ${standaloneDir}`);
}

const layout = await resolveStandaloneRoot(standalone);
for (const file of [serverBin, nodeBin, path.join(layout.appDir, 'server.js'), path.join(frontend, '.next/static'), path.join(frontend, 'public')]) {
  await access(file);
}

const temporary = await mkdtemp(path.join(os.tmpdir(), 'muteki-web-package-'));
try {
  const bundle = path.join(temporary, wrapper);
  const bin = path.join(bundle, 'bin');
  const web = path.join(bundle, 'frontend');
  await mkdir(bin, { recursive: true });
  if (layout.mode === 'flat') {
    await cp(standalone, web, { recursive: true, dereference: true });
  } else {
    await mkdir(web, { recursive: true });
    if (await exists(path.join(standalone, 'node_modules'))) {
      await cp(path.join(standalone, 'node_modules'), path.join(web, 'node_modules'), { recursive: true, dereference: true });
    }
    if (await exists(path.join(standalone, 'packages'))) {
      await cp(path.join(standalone, 'packages'), path.join(web, 'packages'), { recursive: true, dereference: true });
    }
    await cp(layout.appDir, web, { recursive: true, dereference: true });
  }
  await cp(path.join(frontend, '.next/static'), path.join(web, '.next/static'), { recursive: true, dereference: true });
  await cp(path.join(frontend, 'public'), path.join(web, 'public'), { recursive: true, dereference: true });
  const serverFile = path.join(web, 'server.js');
  const marker = 'process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(nextConfig)';
  const source = (await readFile(serverFile, 'utf8')).replaceAll(JSON.stringify(frontend), '__dirname').replaceAll(JSON.stringify(root), '__dirname');
  if (!source.includes(marker)) throw new Error('Next standalone server config marker is missing');
  await writeFile(serverFile, source.replace(marker, `nextConfig.experimental.isrFlushToDisk = false;\n${marker}`));
  const requiredFile = path.join(web, '.next/required-server-files.json');
  const required = JSON.parse(await readFile(requiredFile, 'utf8'));
  required.appDir = '.';
  required.relativeAppDir = '.';
  if (required.config) {
    required.config.outputFileTracingRoot = '.';
    required.config.experimental ??= {};
    required.config.experimental.isrFlushToDisk = false;
    if (required.config.turbopack) required.config.turbopack.root = '.';
  }
  await writeFile(requiredFile, JSON.stringify(required));
  await rm(path.join(web, '.next/server/route-cache'), { recursive: true, force: true });
  await rm(path.join(web, '.next/cache'), { recursive: true, force: true });
  await copyFile(serverBin, path.join(bin, 'muteki-server'));
  await copyFile(nodeBin, path.join(bin, 'node'));
  await chmod(path.join(bin, 'muteki-server'), 0o755);
  await chmod(path.join(bin, 'node'), 0o755);
  await copyFile(path.join(root, 'scripts/web-launcher.mjs'), path.join(bundle, 'launcher.mjs'));
  await copyFile(path.join(root, 'scripts/web-gateway.mjs'), path.join(bundle, 'web-gateway.mjs'));
  await copyFile(path.join(root,'docker-compose.release.yml'),path.join(bundle,'docker-compose.release.yml'));
  await writeFile(path.join(bundle, 'release.json'), JSON.stringify({
    schema_version: 1, version, platform, arch, git_ref: gitRef(), built_at: new Date().toISOString(),
  }, null, 2) + '\n');
  await mkdir(output, { recursive: true });
  const asset = path.join(output, name);
  await rm(asset, { force: true });
  run('tar', ['-czf', asset, '-C', temporary, wrapper], root, { ...process.env, COPYFILE_DISABLE: '1' });
  console.log(JSON.stringify({ asset, bytes: (await stat(asset)).size }));
} finally {
  await rm(temporary, { recursive: true, force: true });
}
