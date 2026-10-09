// Electron patches fs APIs to expose virtual files inside ASAR archives. Bundle
// hashes must describe the physical .app tree so the standalone update runner
// computes the same digest after Electron has prepared the candidate.
const fs = process.versions.electron ? require('original-fs') : require('node:fs');
const path = require('node:path');
const { createHash } = require('node:crypto');
const { execFileSync } = require('node:child_process');

function bundleHash(root) {
  const hash = createHash('sha256');
  const walk = directory => {
    for (const name of fs.readdirSync(directory).sort()) {
      const file = path.join(directory, name), relative = path.relative(root, file), stat = fs.lstatSync(file);
      hash.update(relative + '\0' + (stat.mode & 0o777).toString(8) + '\0');
      if (stat.isSymbolicLink()) {
        const target = fs.readlinkSync(file);
        if (!fs.realpathSync(file).startsWith(fs.realpathSync(root) + path.sep)) throw new Error(`Bundle symlink escapes its root: ${relative}`);
        hash.update('link\0' + target + '\0');
      } else if (stat.isDirectory()) walk(file);
      else if (stat.isFile()) {
        const content = createHash('sha256'), fd = fs.openSync(file, 'r'), buffer = Buffer.alloc(1024 * 1024);
        try { for (;;) { const length = fs.readSync(fd, buffer); if (!length) break; content.update(buffer.subarray(0, length)); } }
        finally { fs.closeSync(fd); }
        hash.update(content.digest());
      } else throw new Error(`Unsupported bundle entry: ${relative}`);
    }
  };
  walk(root); return hash.digest('hex');
}
function bundleIdentity(root, {allowAdhoc = false, expectedArch} = {}) {
  if (process.platform !== 'darwin' || !root.endsWith('.app')) throw new Error('Local candidate installation currently requires a macOS .app bundle');
  execFileSync('/usr/bin/codesign', ['--verify', '--deep', '--strict', root], {stdio: ['ignore', 'pipe', 'pipe']});
  const plist = key => execFileSync('/usr/libexec/PlistBuddy', ['-c', `Print :${key}`, path.join(root, 'Contents/Info.plist')], {encoding: 'utf8'}).trim();
  const { spawnSync } = require('node:child_process');
  const signed = spawnSync('/usr/bin/codesign', ['-dv', '--verbose=4', root], {encoding: 'utf8'});
  if (signed.status !== 0) throw new Error(signed.stderr);
  const team = signed.stderr.match(/^TeamIdentifier=(.+)$/m)?.[1];
  if ((!team || team === 'not set') && !allowAdhoc) throw new Error('Candidate must be signed with a stable signing identity');
  const executable = plist('CFBundleExecutable');
  if (path.basename(executable) !== executable) throw new Error('Invalid bundle executable');
  const id = plist('CFBundleIdentifier'), version = plist('CFBundleShortVersionString');
  if (expectedArch) {
    const archs = execFileSync('/usr/bin/lipo', ['-archs', path.join(root, 'Contents/MacOS', executable)], {encoding: 'utf8'}).trim().split(/\s+/);
    const binaryArch = expectedArch === 'x64' ? 'x86_64' : expectedArch;
    if (!archs.includes(binaryArch)) throw new Error(`Application executable does not contain ${binaryArch}`);
    const runtime = JSON.parse(fs.readFileSync(path.join(root, 'Contents/Resources/runtime/manifest.json'), 'utf8'));
    if (runtime.platform !== 'darwin' || runtime.arch !== expectedArch) throw new Error(`Application runtime does not match darwin/${expectedArch}`);
  }
  return { id, version, executable, team: team && team !== 'not set' ? team : null };
}
function copyBundle(source, destination) {
  execFileSync('/usr/bin/ditto', ['--rsrc', '--extattr', source, destination], {stdio: ['ignore', 'pipe', 'pipe']});
}
function atomicJson(file, value) {
  const temporary = file + '.tmp';
  fs.writeFileSync(temporary, JSON.stringify(value, null, 2) + '\n', {mode: 0o600});
  const fd = fs.openSync(temporary, 'r'); try { fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
  fs.renameSync(temporary, file);
  const directory = fs.openSync(path.dirname(file), 'r');
  try { fs.fsyncSync(directory); } finally { fs.closeSync(directory); }
}
function helperRunning(journal) {
  if (!Number.isInteger(journal.helperPid)) return false;
  try { process.kill(journal.helperPid, 0); return true; }
  catch (error) { if (error.code === 'ESRCH') return false; throw error; }
}
function unfinishedUpdate(root) {
  const directory = path.join(root, 'updates');
  if (!fs.existsSync(directory)) return null;
  const pending = fs.readdirSync(directory).flatMap(name => {
    const file = path.join(directory, name, 'transaction.json');
    if (!fs.existsSync(file)) return [];
    const journal = JSON.parse(fs.readFileSync(file, 'utf8'));
    if (['prepared', 'complete', 'rolled-back'].includes(journal.phase)) return [];
    if (journal.environmentRoot !== root || journal.id !== name) throw new Error('Invalid pending update identity');
    return [{file, journal}];
  });
  if (pending.length > 1) throw new Error('Multiple unfinished updates require recovery before this workspace can start');
  return pending[0] || null;
}
module.exports = { bundleHash, bundleIdentity, copyBundle, atomicJson, helperRunning, unfinishedUpdate };
