#!/usr/bin/env node
import { createHash, randomUUID } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import { chmod, copyFile, cp, mkdir, readFile, rename, rm, writeFile, access } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { buildFrontend, cargo } from './build-steps.mjs';
const root = fileURLToPath(new URL('../', import.meta.url));
const args = new Set();
const values = {};
for (const arg of process.argv.slice(2)) {
  const [key, value] = arg.split(/=(.*)/s, 2);
  if (['--version', '--output', '--identity', '--profile'].includes(key) && value) values[key] = value;
  else if (['--skip-build', '--debug', '--zip', '--adhoc', '--require-release-sign', '--help'].includes(arg)) args.add(arg);
  else throw new Error(`Unknown argument ${arg}. Use --help.`);
}
function listCodeSignIdentities() {
  const result = spawnSync('security', ['find-identity', '-v', '-p', 'codesigning'], { encoding: 'utf8' });
  if (result.status !== 0) return [];
  return [...(result.stdout || '').matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}
function resolveSignIdentity() {
  if (args.has('--adhoc')) return '-';
  const requested = values['--identity'] || process.env.MUTEKI_CODESIGN_IDENTITY;
  if (requested === '-' || requested === 'adhoc') return '-';
  if (requested) return requested;
  const identities = listCodeSignIdentities();
  const developerId = identities.find((name) => name.startsWith('Developer ID Application:'));
  if (developerId) return developerId;
  const appleDev = identities.find((name) => name.startsWith('Apple Development:'));
  if (appleDev) return appleDev;
  return '-';
}
if (args.has('--help')) {
  console.log(`Usage: node scripts/package-macos.mjs [--skip-build] [--profile=release|local|debug] [--debug] [--version=X.Y.Z] [--output=PATH/Muteki.app] [--zip] [--identity=NAME] [--adhoc] [--require-release-sign]
Writes dist/Muteki.app with the NSH host, Rust backend, original Next frontend and Node runtime.
Default signing uses a local Developer ID / Apple Development identity when available, falling back to ad-hoc.
Pass --adhoc or MUTEKI_CODESIGN_IDENTITY=- for ad-hoc. Pass --require-release-sign for official CI releases.
Does not notarize, publish, or upload.
--skip-build           uses existing workspace binaries and frontend build.
--profile              Cargo profile to build and package (default release, used for published zips).
                       local = incremental, no LTO: faster rebuilds for local installs.
--debug                same as --profile=debug.
Rust and the Next build run in parallel; an unchanged frontend reuses its last build
(MUTEKI_FRONTEND_REBUILD=1 forces it).
--version              sets CFBundleShortVersionString (default: crates/desktop version), e.g. for an update candidate.
--output               writes the bundle elsewhere; the name must stay Muteki.app.
--zip                  writes Muteki-<version>-<arch>.zip next to the bundle.
--identity             codesign identity name (overrides MUTEKI_CODESIGN_IDENTITY and auto-detect).
--adhoc                force ad-hoc signature (resets TCC on every rebuild).
--require-release-sign require the official release Developer ID certificate and designated requirement.`);
  process.exit(0);
}
if (process.platform !== 'darwin') throw new Error('macOS packaging requires macOS (sips and iconutil).');
function run(command, arguments_, env = process.env) {
  const result = spawnSync(command, arguments_, { cwd: root, stdio: 'inherit', env });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} failed (${result.status ?? result.signal})`);
}
const frontend = path.join(root, 'apps/frontend');
const frontendDist = process.env.MUTEKI_NEXT_DIST_DIR || '.next';
if (!/^\.[a-zA-Z0-9_-]+$/.test(frontendDist)) throw new Error('MUTEKI_NEXT_DIST_DIR must be a local dot-prefixed directory name.');
const nodeVersion = '22.23.1';
const nodeArchive = `node-v${nodeVersion}-darwin-${process.arch}.tar.gz`;
async function nodeRuntime() {
  if (!['arm64', 'x64'].includes(process.arch)) throw new Error(`Unsupported architecture: ${process.arch}`);
  const cache = path.join(root, '.cache/node-runtime');
  await mkdir(cache, { recursive: true });
  const base = `https://nodejs.org/dist/v${nodeVersion}`;
  console.log(`Preparing official Node ${nodeVersion} (${process.arch})...`);
  const archive = path.join(cache, nodeArchive);
  if (!(await access(archive).then(() => true).catch(() => false))) {
    const response = await fetch(`${base}/${nodeArchive}`, { signal: AbortSignal.timeout(180000) });
    if (!response.ok) throw new Error(`Cannot download official Node: HTTP ${response.status}`);
    await writeFile(archive, Buffer.from(await response.arrayBuffer()));
  }
  const extracted = path.join(cache, `node-v${nodeVersion}-darwin-${process.arch}`);
  const complete = path.join(extracted, '.muteki-extracted');
  if (await access(complete).then(() => true, () => false)) return extracted;
  await rm(extracted, { recursive: true, force: true });
  run('tar', ['-xzf', archive, '-C', cache]);
  await writeFile(complete, `${nodeArchive}\n`);
  return extracted;
}
const profile = values['--profile'] || (args.has('--debug') ? 'debug' : 'release');
if (!['release', 'local', 'debug'].includes(profile)) throw new Error(`Unknown --profile ${profile}; use release, local or debug.`);
const cargoProfile = profile === 'debug' ? [] : ['--profile', profile];
const signIdentity = resolveSignIdentity();
const officialPackage = args.has('--require-release-sign') || process.env.MUTEKI_REQUIRE_RELEASE_SIGN === '1';
if (officialPackage && (!signIdentity.startsWith('Developer ID Application:') || !signIdentity.endsWith('(H53Q22J4T3)'))) {
  throw new Error('Official release packages require Developer ID Application for Team H53Q22J4T3.');
}

const binaryDir = path.join(root, '.cache/target', profile);
const destination = path.resolve(values['--output'] || path.join(root, 'dist/Muteki.app'));
if (path.basename(destination) !== 'Muteki.app') throw new Error('--output must end in Muteki.app.');
const dist = path.dirname(destination);
const staging = path.join(dist, '.Muteki.staging.app');
const bundleId = 'tech.fishcode.muteki.desktop';
const arch = process.arch === 'arm64' ? 'arm64' : 'x64';
async function exists(file) {
  return access(file).then(() => true, () => false);
}

async function copyStandalone(standaloneDir, destDir) {
  if (await exists(path.join(standaloneDir, 'server.js'))) {
    await cp(standaloneDir, destDir, { recursive: true, dereference: true });
    return;
  }
  const nested = path.join(standaloneDir, 'apps/frontend');
  if (!(await exists(path.join(nested, 'server.js')))) {
    throw new Error(`Cannot find standalone server.js under ${standaloneDir}`);
  }
  await mkdir(destDir, { recursive: true });
  if (await exists(path.join(standaloneDir, 'node_modules'))) {
    await cp(path.join(standaloneDir, 'node_modules'), path.join(destDir, 'node_modules'), { recursive: true, dereference: true });
  }
  if (await exists(path.join(standaloneDir, 'packages'))) {
    await cp(path.join(standaloneDir, 'packages'), path.join(destDir, 'packages'), { recursive: true, dereference: true });
  }
  await cp(nested, destDir, { recursive: true, dereference: true });
}

try {
  const crateVersion = /^version\s*=\s*"([^"]+)"/m.exec(await readFile(path.join(root, 'crates/desktop/Cargo.toml'), 'utf8'))?.[1];
  const version = values['--version'] || crateVersion;
  if (!/^\d+\.\d+\.\d+$/.test(version ?? '')) throw new Error(`Invalid bundle version ${version}.`);
  await mkdir(dist, { recursive: true });
  if (!args.has('--skip-build')) {
    const builds = await Promise.allSettled([
      cargo(['build', '-p', 'muteki-desktop', '-p', 'muteki-server', ...cargoProfile], { ...process.env, MACOSX_DEPLOYMENT_TARGET: process.env.MACOSX_DEPLOYMENT_TARGET || '14.0' }),
      buildFrontend({ distDir: frontendDist }),
    ]);
    const failed = builds.find(build => build.status === 'rejected');
    if (failed) throw failed.reason;
  }
  const standaloneDir = path.join(frontend, frontendDist, 'standalone');
  const hasFlatStandalone = await exists(path.join(standaloneDir, 'server.js'));
  const hasNestedStandalone = await exists(path.join(standaloneDir, 'apps/frontend/server.js'));
  if (!hasFlatStandalone && !hasNestedStandalone) {
    throw new Error(`Cannot find standalone server.js under ${standaloneDir}`);
  }
  const runtime = await nodeRuntime();
  await rm(staging, { recursive: true, force: true });
  const macos = path.join(staging, 'Contents/MacOS');
  const resources = path.join(staging, 'Contents/Resources');
  await mkdir(macos, { recursive: true });
  await mkdir(resources, { recursive: true });
  await copyFile(path.join(binaryDir, 'muteki-desktop'), path.join(macos, 'Muteki'));
  await copyFile(path.join(binaryDir, 'muteki-server'), path.join(macos, 'muteki-server'));
  await chmod(path.join(macos, 'Muteki'), 0o755);
  await chmod(path.join(macos, 'muteki-server'), 0o755);
  await copyStandalone(standaloneDir, path.join(resources, 'frontend'));
  const serverFile = path.join(resources, 'frontend/server.js');
  const configMarker = 'process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(nextConfig)';
  const serverSource = (await readFile(serverFile, 'utf8')).replaceAll(JSON.stringify(frontend), '__dirname').replaceAll(JSON.stringify(root), '__dirname');
  if (!serverSource.includes(configMarker)) throw new Error('Cannot configure the standalone frontend cache: Next config marker is missing.');
  await writeFile(serverFile, serverSource.replace(configMarker, `nextConfig.experimental.isrFlushToDisk = false;\n${configMarker}`));
  const requiredFile = path.join(resources, 'frontend', frontendDist, 'required-server-files.json');
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
  await rm(path.join(resources, 'frontend', frontendDist, 'server/route-cache'), { recursive: true, force: true });
  await rm(path.join(resources, 'frontend', frontendDist, 'cache'), { recursive: true, force: true });
  await cp(path.join(frontend, frontendDist, 'static'), path.join(resources, 'frontend', frontendDist, 'static'), { recursive: true });
  await cp(path.join(frontend, 'public'), path.join(resources, 'frontend/public'), { recursive: true });
  await mkdir(path.join(resources, 'node/bin'), { recursive: true });
  await copyFile(path.join(runtime, 'bin/node'), path.join(resources, 'node/bin/node'));
  await chmod(path.join(resources, 'node/bin/node'), 0o755);
  for (const file of ['LICENSE', 'README.md', 'CHANGELOG.md']) await copyFile(path.join(runtime, file), path.join(resources, 'node', file));
  await writeFile(path.join(resources, 'frontend-build-id'), randomUUID());
  const licenses = path.join(resources, 'licenses');
  await mkdir(licenses, { recursive: true });
  await copyFile(path.join(root, 'LICENSE'), path.join(licenses, 'Muteki-LICENSE'));
  await copyFile(path.join(frontend, 'THIRD_PARTY_NOTICES.md'), path.join(licenses, 'Frontend-THIRD_PARTY_NOTICES.md'));
  for (const name of ['LICENSE-APACHE']) await copyFile(path.join(root, 'vendor/upstream-nsh', name), path.join(licenses, `NSH-${name}`));
  const icon = path.join(root, 'assets/icon-512.png');
  const cachedIcns = path.join(root, '.cache/icons', `${createHash('sha256').update(await readFile(icon)).digest('hex')}.icns`);
  if (!(await access(cachedIcns).then(() => true, () => false))) {
    const iconset = path.join(dist, '.muteki.iconset');
    await mkdir(iconset, { recursive: true });
    for (const size of [16, 32, 128, 256, 512]) {
      for (const density of [1, 2]) {
        run('sips', ['-z', String(size * density), String(size * density), icon, '--out', path.join(iconset, `icon_${size}x${size}${density === 2 ? '@2x' : ''}.png`)]);
      }
    }
    await mkdir(path.dirname(cachedIcns), { recursive: true });
    const partial = cachedIcns.replace(/\.icns$/, `.${process.pid}.partial.icns`);
    run('iconutil', ['-c', 'icns', iconset, '-o', partial]);
    await rename(partial, cachedIcns);
    await rm(iconset, { recursive: true, force: true });
  }
  await copyFile(cachedIcns, path.join(resources, 'Muteki.icns'));
  await writeFile(path.join(staging, 'Contents/Info.plist'), `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Muteki</string>
<key>CFBundleDisplayName</key><string>Muteki</string>
<key>CFBundleIdentifier</key><string>${bundleId}</string>
<key>CFBundleExecutable</key><string>Muteki</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleDevelopmentRegion</key><string>zh-Hans</string>
<key>CFBundleLocalizations</key><array><string>zh-Hans</string><string>en</string></array>
<key>CFBundleShortVersionString</key><string>${version}</string>
<key>CFBundleVersion</key><string>${version}</string>
<key>CFBundleIconFile</key><string>Muteki.icns</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
<key>NSPrincipalClass</key><string>NSApplication</string>
<key>CFBundleURLTypes</key><array><dict>
<key>CFBundleURLName</key><string>${bundleId}</string>
<key>CFBundleURLSchemes</key><array><string>muteki</string></array>
</dict></array>
<key>NSMicrophoneUsageDescription</key><string>Muteki 需要访问麦克风以进行语音输入。</string>
<key>NSSpeechRecognitionUsageDescription</key><string>Muteki 需要语音识别权限以将您的语音转换为文字。</string>
</dict></plist>
`);
  run('plutil', ['-lint', path.join(staging, 'Contents/Info.plist')]);
  await mkdir(path.join(resources, 'runtime'), { recursive: true });
  await writeFile(path.join(resources, 'runtime/manifest.json'), `${JSON.stringify({ platform: 'darwin', arch, version, node: '../node/bin/node' }, null, 2)}\n`);
  const signArgs = ['--force', '--sign', signIdentity, '--timestamp=none'];
  run('codesign', ['--verify', '--strict', path.join(resources, 'node/bin/node')]);
  run('codesign', [...signArgs, '--identifier', `${bundleId}.server`, path.join(macos, 'muteki-server')]);
  run('codesign', [...signArgs, '--identifier', bundleId, staging]);
  run('codesign', ['--verify', '--deep', '--strict', staging]);
  if (officialPackage) {
    run('codesign', ['--verify', '--strict', '-R=' + (await readFile(path.join(root, 'assets/macos-release.requirement'), 'utf8')), staging]);
  }

  await rm(destination, { recursive: true, force: true });
  await rename(staging, destination);
  const signLabel = signIdentity === '-' ? 'ad-hoc' : signIdentity;
  console.log(`Packaged macOS app ${version} signed as ${signLabel}: ${destination}`);
  if (args.has('--zip')) {
    const zip = path.join(dist, `Muteki-${version}-${arch}.zip`);
    await rm(zip, { force: true });
    await rm(`${zip}.sha512`, { force: true });
    run('ditto', ['-c', '-k', '--norsrc', '--noextattr', '--noacl', '--keepParent', destination, zip]);
    console.log(`Release asset: ${zip}`);
  }
  console.log(`Launch: open ${destination.startsWith(root) ? path.relative(root, destination) : destination}`);
} catch (error) {
  console.error(`macOS package failed: ${error.message}`);
  process.exitCode = 1;
}
