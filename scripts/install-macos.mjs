#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { access, rename, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('../', import.meta.url));
const argv = process.argv.slice(2);
if (argv.includes('--help')) {
  console.log(`Usage: node scripts/install-macos.mjs [--build [--profile=local|release]] [--open] [PATH/Muteki.app]
Verifies the package, prepares persistent storage, then replaces /Applications/Muteki.app without retaining the old App. Quit the installed app first. No publication; persistent data and storage migration backups are preserved.
--build    first runs scripts/package-macos.mjs (default --profile=local: incremental, no LTO) into dist/Muteki.app.
--open     launches the installed app afterwards.`);
  process.exit(0);
}
const build = argv.includes('--build');
const open = argv.includes('--open');
const profileArg = argv.find(arg => arg.startsWith('--profile='));
if (profileArg && !build) throw new Error('--profile applies to --build only.');
const args = argv.filter(arg => !['--build', '--open'].includes(arg) && arg !== profileArg);
if (process.platform !== 'darwin' || args.length > 1 || args.some(arg => arg.startsWith('--'))) throw new Error('Requires macOS and at most one bundle path. Use --help.');
if (build && args.length) throw new Error('--build packages dist/Muteki.app; do not also pass a bundle path.');
const source = path.resolve(args[0] || path.join(root, 'dist/Muteki.app'));
const destination = '/Applications/Muteki.app';
if (source === destination || path.basename(source) !== 'Muteki.app') throw new Error('Select a separate built Muteki.app.');
function run(command, argv, capture = false) {
  const result = spawnSync(command, argv, { stdio: capture ? 'pipe' : 'inherit', encoding: 'utf8' });
  if (result.error || result.status !== 0) throw new Error(`${command} failed: ${result.error?.message || result.status}`);
  return result.stdout || '';
}
const processes = () => run('/bin/ps', ['-axo', 'comm='], true).split('\n');
if (processes().some(command => command.trim().startsWith(`${destination}/Contents/`))) throw new Error('Quit the installed Muteki and its services before replacement.');
if (build) run(process.execPath, [path.join(root, 'scripts/package-macos.mjs'), profileArg || '--profile=local']);
if (run('/usr/libexec/PlistBuddy', ['-c', 'Print :CFBundleIdentifier', path.join(source, 'Contents/Info.plist')], true).trim() !== 'tech.fishcode.muteki.desktop') throw new Error('Unexpected Muteki bundle identity.');
await access(path.join(source, 'Contents/MacOS/muteki-server'));
await access(path.join(source, 'Contents/Resources/frontend-build-id'));
run('/usr/bin/codesign', ['--verify', '--deep', '--strict', source]);
if (processes().some(command => command.trim().startsWith(`${destination}/Contents/`))) throw new Error('Quit the installed Muteki and its services before replacement.');
let hadPrevious = false;
try { await access(destination); hadPrevious = true; } catch (error) { if (error.code !== 'ENOENT') throw error; }
run(path.join(source, 'Contents/MacOS/Muteki'), ['--muteki-prepare-storage']);
const staging = path.join('/Applications', `.Muteki-${process.pid}.app`);
run('/usr/bin/ditto', [source, staging]);
run('/usr/bin/codesign', ['--verify', '--deep', '--strict', staging]);
const previous = path.join('/Applications', `.Muteki-previous-${process.pid}.app`);
if (processes().some(command => command.trim().startsWith(`${destination}/Contents/`))) {
  await rm(staging, { recursive: true, force: true });
  throw new Error('Muteki started while preparing the replacement. Quit it and run the installer again.');
}
if (hadPrevious) await rename(destination, previous);
let replaced = false;
try {
  await rename(staging, destination);
  replaced = true;
  if (processes().some(command => [destination, previous].some(bundle => command.trim().startsWith(`${bundle}/Contents/`)))) {
    throw new Error('Muteki started during replacement. The running application was preserved; quit it before installing again.');
  }
} catch (error) {
  if (replaced) await rename(destination, staging);
  if (hadPrevious) await rename(previous, destination);
  throw error;
}
if (hadPrevious) await rm(previous, { recursive: true, force: true });
console.log(`Installed ${destination}\nPrevious application removed; no application backup retained.\nPersistent storage is outside the application bundle.`);
if (open) run('/usr/bin/open', [destination]);
