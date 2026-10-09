const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const { randomUUID, createHash } = require('node:crypto');
const { spawn, execFileSync } = require('node:child_process');
const {once} = require('node:events');
const { bundleHash, bundleIdentity, copyBundle, atomicJson } = require('./update-files.cjs');
const OWNER = 'FishCodeTech/muteki';
const API = `https://api.github.com/repos/${OWNER}/releases/latest`;
const MAX_ASSET = 1024 * 1024 * 1024;
const DOWNLOAD_CHUNK_SIZE = 1024 * 1024;
function compareVersions(left, right) {
  const a = left.split('.').map(Number);
  const b = right.split('.').map(Number);
  for (let index = 0; index < Math.max(a.length, b.length); index++) {
    const difference = (a[index] || 0) - (b[index] || 0);
    if (difference) return difference;
  }
  return 0;
}
function pathEntryExists(file) {
  try { fs.lstatSync(file); return true; }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}

class DesktopUpdates {
  constructor(app, environment, onStatus = () => {}) {
    this.app = app;
    this.environment = environment;
    this.onStatus = onStatus;
    this.prepared = null;
    this.operation = null;
    this.current = path.resolve(app.getPath('exe'), '../../..');
    this.downloads = app.getPath('downloads');
    this.status = this.readLastInstall() || { state: 'idle', currentVersion: app.getVersion() };
  }
  publish(patch) {
    this.status = { ...this.status, ...patch };
    this.onStatus(this.status);
    return this.status;
  }
  getStatus() { return { ...this.status }; }
  readLastInstall() {
    const directory = path.join(this.environment.root, 'updates');
    try {
      const stat = fs.lstatSync(directory);
      if (!stat.isDirectory() || stat.isSymbolicLink()) return null;
    } catch (error) {
      if (error.code === 'ENOENT') return null;
      throw error;
    }
    const outcomes = [];
    for (const id of fs.readdirSync(directory)) {
      const transactionDirectory = path.join(directory, id);
      const transactionFile = path.join(transactionDirectory, 'transaction.json');
      try {
        const directoryStat = fs.lstatSync(transactionDirectory);
        if (!directoryStat.isDirectory() || directoryStat.isSymbolicLink()) continue;
        const transactionStat = fs.lstatSync(transactionFile);
        if (!transactionStat.isFile() || transactionStat.isSymbolicLink()) continue;
        const journal = JSON.parse(fs.readFileSync(transactionFile, 'utf8'));
        if (journal.version !== 1 || journal.id !== id || journal.environmentRoot !== this.environment.root) continue;
        if (journal.candidateIdentity?.id !== 'tech.fishcode.muteki.desktop' || !/^\d+\.\d+\.\d+$/.test(journal.candidateIdentity.version || '')) continue;
        if (!['complete', 'rolled-back', 'failed', 'repair-required', 'recovery-required'].includes(journal.phase)) continue;
        outcomes.push({ journal, at: transactionStat.mtime, file: transactionFile });
      } catch (error) {
        if (error.code === 'ENOENT') continue;
        throw Object.assign(new Error(`无法读取更新记录 ${transactionFile}：${error.message}`), { code: 'update.history_read_failed' });
      }
    }
    outcomes.sort((a, b) => b.at - a.at);
    const latest = outcomes[0];
    if (!latest) return null;
    const { journal, at } = latest;
    const failed = journal.phase !== 'complete';
    const code = ['repair-required', 'recovery-required'].includes(journal.phase) ? 'update.recovery_required' : 'update.last_install_failed';
    const detail = typeof journal.error === 'string' ? journal.error.slice(0, 1000) : '';
    const version = journal.candidateIdentity.version;
    const message = failed
      ? `上次更新到 ${version} 未完成。${detail ? ` ${detail}` : ''}`
      : `上次更新已完成，安装版本为 ${version}。`;
    return {
      state: failed ? 'error' : 'idle',
      currentVersion: this.app.getVersion(),
      message,
      ...(failed ? { error: { code, message } } : {}),
      lastInstall: { state: failed ? 'failed' : 'complete', version, at: at.toISOString(), message, ...(failed ? { code } : {}) },
    };
  }
  discardPreparedLocal() {
    if (this.prepared?.mode === 'local') this.prepared = null;
  }
  async copyVerifiedFile(source, destination, asset, onProgress = () => {}) {
    const before = fs.lstatSync(source);
    if (!before.isFile() || before.isSymbolicLink() || before.size !== asset.size) {
      throw Object.assign(new Error('Existing release package is not a regular file with the published size.'), { code: 'update.cached_asset_invalid' });
    }
    const noFollow = fs.constants.O_NOFOLLOW || 0;
    const input = fs.openSync(source, fs.constants.O_RDONLY | noFollow);
    const output = fs.openSync(destination, 'wx', 0o600);
    const hash = createHash('sha256');
    let total = 0;
    try {
      if (!fs.fstatSync(input).isFile()) throw new Error('Existing release package is not a regular file.');
      const buffer = Buffer.alloc(DOWNLOAD_CHUNK_SIZE);
      for (;;) {
        const length = fs.readSync(input, buffer, 0, buffer.length, null);
        if (!length) break;
        const chunk = buffer.subarray(0, length);
        total += length;
        if (total > asset.size) throw Object.assign(new Error('Existing release package exceeds the published size.'), { code: 'update.cached_asset_invalid' });
        hash.update(chunk);
        let written = 0;
        while (written < length) written += fs.writeSync(output, chunk, written, length - written);
        onProgress(total);
      }
      fs.fsyncSync(output);
    } finally {
      fs.closeSync(input);
      fs.closeSync(output);
    }
    if (total !== asset.size || hash.digest('hex') !== asset.digest.slice(7).toLowerCase()) {
      throw Object.assign(new Error('Existing release package does not match GitHub size and SHA-256 metadata.'), { code: 'update.cached_asset_invalid' });
    }
  }
  async downloadAsset(asset, destination) {
    const response = await fetch(asset.browser_download_url, {
      headers: { 'User-Agent': 'Muteki-Desktop-Updater' },
      signal: AbortSignal.timeout(180000),
    });
    const finalUrl = new URL(response.url);
    const trustedHosts = ['github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com'];
    if (!response.ok || finalUrl.protocol !== 'https:' || !trustedHosts.includes(finalUrl.hostname)) {
      throw Object.assign(new Error(`Release download failed (HTTP ${response.status}).`), { code: 'update.download_failed' });
    }
    const contentLength = response.headers.get('content-length');
    if ((contentLength && Number(contentLength) !== asset.size) || !response.body) {
      throw Object.assign(new Error('Downloaded release size does not match GitHub metadata.'), { code: 'update.asset_size_mismatch' });
    }
    const hash = createHash('sha256');
    let total = 0;
    const fd = fs.openSync(destination, 'wx', 0o600);
    try {
      for await (const chunk of response.body) {
        const part = Buffer.from(chunk);
        total += part.length;
        if (total > asset.size || total > MAX_ASSET) {
          throw Object.assign(new Error('Downloaded release exceeds its published size.'), { code: 'update.asset_size_mismatch' });
        }
        hash.update(part);
        let written = 0;
        while (written < part.length) written += fs.writeSync(fd, part, written, part.length - written);
        this.publish({ progress: Math.floor(total * 100 / asset.size) });
      }
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    if (total !== asset.size) {
      throw Object.assign(new Error('Downloaded release size does not match GitHub metadata.'), { code: 'update.asset_size_mismatch' });
    }
    if (hash.digest('hex') !== asset.digest.slice(7).toLowerCase()) {
      throw Object.assign(new Error('Downloaded release SHA-256 does not match GitHub metadata.'), { code: 'update.digest_mismatch' });
    }
  }
  assertInstallable() {
    if (!this.app.isPackaged || process.platform !== 'darwin' || this.environment.channel !== 'stable') {
      throw Object.assign(new Error('安装桌面更新仅支持已安装的 macOS 日常版。'), { code: 'update.unsupported' });
    }
  }
  async check() {
    if (this.operation) return this.operation;
    this.operation = (async () => {
      this.publish({ state: 'checking', error: undefined, progress: undefined, message: undefined, lastInstall: undefined });
      try {
        const response = await fetch(API, {
          headers: {
            Accept: 'application/vnd.github+json',
            'User-Agent': 'Muteki-Desktop-Updater',
            'X-GitHub-Api-Version': '2022-11-28',
          },
          signal: AbortSignal.timeout(30000),
        });
        if (!response.ok) {
          throw Object.assign(new Error(`GitHub release check failed (HTTP ${response.status}).`), { code: 'update.release_check_failed' });
        }
        const release = await response.json();
        if (release.draft || release.prerelease || !/^v?\d+\.\d+\.\d+$/.test(release.tag_name || '')) {
          throw Object.assign(new Error('Latest GitHub release is not a stable semantic version.'), { code: 'update.release_invalid' });
        }
        const version = release.tag_name.replace(/^v/, '');
        const expectedReleaseUrl = `https://github.com/${OWNER}/releases/tag/${encodeURIComponent(release.tag_name)}`;
        if (release.html_url !== expectedReleaseUrl) {
          throw Object.assign(new Error('GitHub release URL does not match the trusted repository and tag.'), { code: 'update.release_invalid' });
        }
        const arch = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : null;
        const name = `Muteki-${version}-${arch}.zip`;
        const asset = release.assets?.find(item => item.name === name && item.state === 'uploaded');
        const expectedAssetUrl = `https://github.com/${OWNER}/releases/download/${encodeURIComponent(release.tag_name)}/${encodeURIComponent(name)}`;
        if (!arch || !asset || !/^sha256:[a-f0-9]{64}$/i.test(asset.digest || '') || asset.browser_download_url !== expectedAssetUrl) {
          throw Object.assign(new Error(`Trusted ${arch || process.arch} release asset or its SHA-256 digest is unavailable.`), { code: 'update.asset_unavailable' });
        }
        const canInstall = this.app.isPackaged && process.platform === 'darwin' && this.environment.channel === 'stable';
        const currentVersion = this.app.getVersion();
        const current = compareVersions(version, currentVersion) <= 0;
        return this.publish({
          state: current ? 'current' : 'available',
          currentVersion,
          latestVersion: version,
          assetName: name,
          digest: asset.digest,
          releaseUrl: release.html_url,
          checkedAt: new Date().toISOString(),
          canInstall,
          installReason: canInstall ? undefined : '安装更新仅支持已安装的 macOS 日常版。',
          message: current ? '已是最新版本。' : undefined,
          progress: 0,
        });
      } catch (error) {
        return this.publish({
          state: 'error',
          error: { code: error.code || 'update.release_check_failed', message: error.message },
          message: error.message,
        });
      } finally {
        this.operation = null;
      }
    })();
    return this.operation;
  }
  async download() {
    if (this.prepared?.mode === 'official' && this.status.state === 'ready') return this.getStatus();
    if (this.prepared?.mode === 'local') {
      throw Object.assign(new Error('A local candidate is prepared; use the local candidate confirmation to continue.'), { code: 'update.mode_conflict' });
    }
    if (this.operation) return this.operation;
    this.operation = (async () => {
      let temporary;
      try {
        this.assertInstallable();
        if (this.status.state !== 'available') {
          throw Object.assign(new Error('Check for an available update before installing.'), { code: 'update.not_available' });
        }
        this.publish({ state: 'downloading', progress: 0, error: undefined });
        const releaseResponse = await fetch(API, {
          headers: {
            Accept: 'application/vnd.github+json',
            'User-Agent': 'Muteki-Desktop-Updater',
            'X-GitHub-Api-Version': '2022-11-28',
          },
          signal: AbortSignal.timeout(30000),
        });
        if (!releaseResponse.ok) {
          throw Object.assign(new Error(`GitHub release check failed (HTTP ${releaseResponse.status}).`), { code: 'update.release_check_failed' });
        }
        const release = await releaseResponse.json();
        const version = release.tag_name?.replace(/^v/, '');
        const arch = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : null;
        const name = `Muteki-${version}-${arch}.zip`;
        const asset = release.assets?.find(item => item.name === name && item.state === 'uploaded');
        const expectedAssetUrl = `https://github.com/${OWNER}/releases/download/${encodeURIComponent(release.tag_name)}/${encodeURIComponent(name)}`;
        if (release.draft || release.prerelease || release.html_url !== this.status.releaseUrl || version !== this.status.latestVersion || !asset || asset.state !== 'uploaded' || asset.name !== this.status.assetName || asset.digest !== this.status.digest || asset.browser_download_url !== expectedAssetUrl || !Number.isSafeInteger(asset.size) || asset.size < 1 || asset.size > MAX_ASSET || !/^sha256:[a-f0-9]{64}$/i.test(asset.digest || '')) {
          throw Object.assign(new Error('Release metadata changed since the update check. Check again.'), { code: 'update.release_changed' });
        }
        temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-update-'));
        const zip = path.join(temporary, name);
        const extracted = path.join(temporary, 'extracted');
        fs.mkdirSync(extracted, { mode: 0o700 });
        const updateDirectory = path.join(this.environment.root, 'updates');
        fs.mkdirSync(updateDirectory, { recursive: true, mode: 0o700 });
        if (!fs.lstatSync(updateDirectory).isDirectory() || fs.lstatSync(updateDirectory).isSymbolicLink()) {
          throw Object.assign(new Error('Update storage directory is not private and safe.'), { code: 'update.cache_unavailable' });
        }
        fs.chmodSync(updateDirectory, 0o700);
        const downloadsCache = path.join(updateDirectory, 'downloads');
        fs.mkdirSync(downloadsCache, { recursive: true, mode: 0o700 });
        if (!fs.lstatSync(downloadsCache).isDirectory() || fs.lstatSync(downloadsCache).isSymbolicLink()) {
          throw Object.assign(new Error('Update download cache is not private and safe.'), { code: 'update.cache_unavailable' });
        }
        fs.chmodSync(downloadsCache, 0o700);
        const cachedFile = path.join(downloadsCache, `${asset.digest.slice(7)}.zip`);
        const userDownload = path.join(this.downloads, name);
        const existing = pathEntryExists(cachedFile) ? cachedFile : pathEntryExists(userDownload) ? userDownload : null;
        if (existing) {
          this.publish({ message: '正在验证已有的官方安装包…', progress: 0 });
          try {
            await this.copyVerifiedFile(existing, zip, asset, total => this.publish({ progress: Math.floor(total * 100 / asset.size) }));
          } catch (error) {
            if (error.code === 'update.cached_asset_invalid') throw error;
            throw Object.assign(new Error(`Existing release package could not be verified: ${error.message}`), { code: 'update.cached_asset_invalid' });
          }
        } else {
          this.publish({ message: '正在从 GitHub 下载官方安装包…', progress: 0 });
          await this.downloadAsset(asset, zip);
          fs.copyFileSync(zip, cachedFile, fs.constants.COPYFILE_EXCL);
          fs.chmodSync(cachedFile, 0o600);
        }
        const digest = asset.digest.slice(7).toLowerCase();
        this.publish({ state: 'validating', progress: 100, downloadDigest: digest });
        this.assertZipSafe(fs.readFileSync(zip));
        execFileSync('/usr/bin/ditto', ['-x', '-k', zip, extracted], { stdio: ['ignore', 'pipe', 'pipe'] });
        const extractedEntries = fs.readdirSync(extracted);
        const apps = extractedEntries.filter(entry => entry.endsWith('.app'));
        if (apps.length !== 1 || extractedEntries.length !== 1) {
          throw Object.assign(new Error('Release archive must contain exactly one application bundle.'), { code: 'update.archive_invalid' });
        }
        const source = path.join(extracted, apps[0]);
        const identity = bundleIdentity(source, { allowAdhoc: true, expectedArch: arch });
        if (identity.id !== 'tech.fishcode.muteki.desktop' || identity.version !== version) {
          throw Object.assign(new Error('Downloaded application identity or version does not match the checked release.'), { code: 'update.identity_mismatch' });
        }
        const currentIdentity = bundleIdentity(this.current, { allowAdhoc: true, expectedArch: arch });
        if (identity.id !== currentIdentity.id || Boolean(identity.team) !== Boolean(currentIdentity.team) || (identity.team && identity.team !== currentIdentity.team)) {
          throw Object.assign(new Error('Release signing identity does not match the installed application.'), { code: 'update.signing_mismatch' });
        }
        this.prepared = await this.prepare(source, {
          mode: 'official',
          expectedSha256: digest,
          version,
          assetDigest: asset.digest,
          assetName: name,
          releaseUrl: release.html_url,
        });
        return this.publish({
          state: 'ready',
          progress: 100,
          latestVersion: version,
          preparedVersion: version,
          message: '更新已通过完整性与启动检查。',
        });
      } catch (error) {
        return this.publish({
          state: 'error',
          error: { code: error.code || 'update.install_prepare_failed', message: error.message },
          message: error.message,
        });
      } finally {
        if (temporary) fs.rmSync(temporary, { recursive: true, force: true });
        this.operation = null;
      }
    })();
    return this.operation;
  }
  assertZipSafe(bytes) {
    let eocd = -1;
    for (let index = Math.max(0, bytes.length - 65557); index <= bytes.length - 22; index++) {
      if (bytes.readUInt32LE(index) === 0x06054b50) eocd = index;
    }
    if (eocd < 0 || bytes.readUInt16LE(eocd + 4) !== 0 || bytes.readUInt16LE(eocd + 6) !== 0 || eocd + 22 + bytes.readUInt16LE(eocd + 20) !== bytes.length) {
      throw Object.assign(new Error('Release archive uses an unsupported ZIP layout.'), { code: 'update.archive_invalid' });
    }

    const count = bytes.readUInt16LE(eocd + 10);
    const size = bytes.readUInt32LE(eocd + 12);
    const centralStart = bytes.readUInt32LE(eocd + 16);
    const centralEnd = centralStart + size;
    if (bytes.readUInt16LE(eocd + 8) !== count) {
      throw Object.assign(new Error('Release archive entry counts do not match.'), { code: 'update.archive_invalid' });
    }
    if (centralEnd !== eocd) {
      throw Object.assign(new Error('Release archive central directory bounds are invalid.'), { code: 'update.archive_invalid' });
    }

    let offset = centralStart;
    let expandedSize = 0;
    const entries = [];
    const seenPaths = new Set();
    for (let index = 0; index < count; index++) {
      if (bytes.readUInt32LE(offset) !== 0x02014b50) {
        throw Object.assign(new Error('Release archive directory is invalid.'), { code: 'update.archive_invalid' });
      }
      const flags = bytes.readUInt16LE(offset + 8);
      const mode = bytes.readUInt32LE(offset + 38) >>> 16;
      const method = bytes.readUInt16LE(offset + 10);
      const compressedSize = bytes.readUInt32LE(offset + 20);
      const uncompressedSize = bytes.readUInt32LE(offset + 24);
      const nameLength = bytes.readUInt16LE(offset + 28);
      const extraLength = bytes.readUInt16LE(offset + 30);
      const commentLength = bytes.readUInt16LE(offset + 32);
      const localOffset = bytes.readUInt32LE(offset + 42);
      const name = bytes.subarray(offset + 46, offset + 46 + nameLength).toString('utf8');
      const kind = mode & 0o170000;
      const parts = name.split('/');
      if (!name || name.includes('\\') || name.includes('\0') || name.startsWith('/') || /^[A-Za-z]:/.test(name) || parts.some(part => part === '..' || part === '.') || (kind && ![0o100000, 0o040000, 0o120000].includes(kind)) || ![0, 8].includes(method) || (flags & 1) || uncompressedSize > MAX_ASSET) {
        throw Object.assign(new Error('Release archive contains an unsafe or unsupported path.'), { code: 'update.archive_unsafe' });
      }
      const canonical = name.endsWith('/') ? name.slice(0, -1) : name;
      const canonicalParts = canonical.split('/');
      if (canonicalParts[0] !== 'Muteki.app' || canonicalParts.some(part => !part)) {
        throw Object.assign(new Error('Release archive contains an unexpected top-level path.'), { code: 'update.archive_unsafe' });
      }
      if (localOffset >= centralStart || bytes.readUInt32LE(localOffset) !== 0x04034b50) {
        throw Object.assign(new Error('Release archive local entry is invalid.'), { code: 'update.archive_invalid' });
      }
      const localNameLength = bytes.readUInt16LE(localOffset + 26);
      const localExtraLength = bytes.readUInt16LE(localOffset + 28);
      const localName = bytes.subarray(localOffset + 30, localOffset + 30 + localNameLength).toString('utf8');
      const dataOffset = localOffset + 30 + localNameLength + localExtraLength;
      const localFlags = bytes.readUInt16LE(localOffset + 6);
      const localMethod = bytes.readUInt16LE(localOffset + 8);
      const localCrc = bytes.readUInt32LE(localOffset + 14);
      const centralCrc = bytes.readUInt32LE(offset + 16);
      const localCompressedSize = bytes.readUInt32LE(localOffset + 18);
      const localUncompressedSize = bytes.readUInt32LE(localOffset + 22);
      if (localName !== name || localFlags !== flags || localMethod !== method || dataOffset + compressedSize > centralStart || (!(flags & 8) && (localCrc !== centralCrc || localCompressedSize !== compressedSize || localUncompressedSize !== uncompressedSize))) {
        throw Object.assign(new Error('Release archive entry path or bounds are invalid.'), { code: 'update.archive_invalid' });
      }
      expandedSize += uncompressedSize;
      if (expandedSize > 2 * 1024 * 1024 * 1024) {
        throw Object.assign(new Error('Release archive expands beyond the allowed size.'), { code: 'update.archive_unsafe' });
      }
      let symlinkTarget = Buffer.alloc(0);
      if (kind === 0o120000) {
        if (uncompressedSize > 4096) {
          throw Object.assign(new Error('Release archive symlink target is too long.'), { code: 'update.archive_unsafe' });
        }
        const compressed = bytes.subarray(dataOffset, dataOffset + compressedSize);
        symlinkTarget = method === 0 ? compressed : require('node:zlib').inflateRawSync(compressed, { maxOutputLength: 4096 });
        if (symlinkTarget.length !== uncompressedSize) {
          throw Object.assign(new Error('Release archive symlink target is invalid.'), { code: 'update.archive_invalid' });
        }
      }
      if (seenPaths.has(canonical)) {
        throw Object.assign(new Error('Release archive contains duplicate paths.'), { code: 'update.archive_unsafe' });
      }
      seenPaths.add(canonical);
      entries.push({ name: canonical, symlinkTarget, mode, isDirectory: name.endsWith('/') });
      offset += 46 + nameLength + extraLength + commentLength;
    }
    if (offset !== centralEnd) {
      throw Object.assign(new Error('Release archive directory size is invalid.'), { code: 'update.archive_invalid' });
    }

    const symlinks = new Set(entries.filter(entry => (entry.mode & 0o170000) === 0o120000).map(entry => entry.name));
    const kinds = new Map(entries.map(entry => [entry.name, (entry.mode & 0o170000) || (entry.isDirectory ? 0o040000 : 0o100000)]));
    for (const entry of entries) {
      const segments = entry.name.split('/');
      for (let index = 1; index < segments.length; index++) {
        const ancestor = segments.slice(0, index).join('/');
        if (symlinks.has(ancestor)) {
          throw Object.assign(new Error('Release archive writes through a symlink path.'), { code: 'update.archive_unsafe' });
        }
        if (kinds.has(ancestor) && kinds.get(ancestor) !== 0o040000) {
          throw Object.assign(new Error('Release archive contains a file used as a directory.'), { code: 'update.archive_unsafe' });
        }
      }
      if (symlinks.has(entry.name)) {
        const target = entry.symlinkTarget.toString('utf8');
        const resolved = path.posix.normalize(path.posix.join(path.posix.dirname(entry.name), target));
        if (!target || path.posix.isAbsolute(target) || resolved === '..' || resolved.startsWith('../') || !resolved.startsWith('Muteki.app/')) {
          throw Object.assign(new Error('Release archive symlink escapes its application bundle.'), { code: 'update.archive_unsafe' });
        }
      }
    }
  }
  async prepare(source,{mode='local',expectedSha256,version,assetDigest,assetName,releaseUrl}={}) {
    if (mode === 'local') this.prepared = null;
    this.assertInstallable();
    source = fs.realpathSync(source);
    if (source === this.current) throw new Error('请选择新构建的候选应用。');
    const identityOptions = { allowAdhoc: mode === 'official', expectedArch: process.arch };
    const currentIdentity = bundleIdentity(this.current, identityOptions);
    const candidateIdentity = bundleIdentity(source, identityOptions);
    const sameTeam = currentIdentity.team === candidateIdentity.team;
    if (currentIdentity.id !== candidateIdentity.id || (mode === 'local' && !sameTeam) || (mode === 'official' && !sameTeam)) {
      throw new Error('候选版本的应用身份或签名团队与日常版不同。');
    }
    if (mode === 'official' && candidateIdentity.version !== version) {
      throw Object.assign(new Error('Release version does not match its checked metadata.'), { code: 'update.version_mismatch' });
    }
    const currentVersion = this.app.getVersion();
    if (compareVersions(candidateIdentity.version, currentVersion) <= 0) {
      throw Object.assign(new Error('更新版本必须高于当前版本。'), { code: 'update.not_newer' });
    }
    const id = randomUUID();
    const transaction = path.join(this.environment.root, 'updates', id);
    fs.mkdirSync(transaction, { recursive: true, mode: 0o700 });
    const candidate = path.join(transaction, 'candidate.app');
    copyBundle(source, candidate);
    bundleIdentity(candidate, identityOptions);
    const sha256 = bundleHash(candidate);
    if (sha256 !== bundleHash(source) || (expectedSha256 && expectedSha256 !== this.status.downloadDigest)) {
      throw new Error('候选应用在准备过程中发生变化，请重新下载。');
    }

    const profile = path.join(transaction, 'acceptance');
    const acceptanceLog = path.join(transaction, 'acceptance.log');
    const log = fs.openSync(acceptanceLog, 'a', 0o600);
    const env = {
      ...process.env,
      MUTEKI_DESKTOP_USER_DATA: path.join(profile, 'desktop'),
      MUTEKI_DESKTOP_REGISTER_PROTOCOL: '0',
    };
    for (const key of ['MUTEKI_DESKTOP_URL', 'MUTEKI_ENVIRONMENT_ROOT', 'MUTEKI_DESKTOP_WORKSPACE', 'NODE_OPTIONS', 'NODE_PATH', 'ELECTRON_RUN_AS_NODE', 'PYTHONPATH', 'PYTHONHOME', 'VIRTUAL_ENV']) {
      delete env[key];
    }
    const child = spawn(path.join(candidate, 'Contents/MacOS', candidateIdentity.executable), [
      '--muteki-channel=candidate',
      `--muteki-environment-root=${profile}`,
      '--muteki-self-check',
    ], { env, stdio: ['ignore', log, log] });
    fs.closeSync(log);
    const timer = setTimeout(() => child.kill('SIGTERM'), 150000);
    let code;
    try {
      [code] = await once(child, 'exit');
    } finally {
      clearTimeout(timer);
    }
    const resultFile = path.join(profile, 'candidate-result.json');
    const passed = code === 0 && fs.existsSync(resultFile) && JSON.parse(fs.readFileSync(resultFile, 'utf8')).status === 'passed';
    if (!passed) throw new Error(`候选版本验收失败，请查看 ${acceptanceLog}`);
    if (bundleHash(candidate) !== sha256) throw new Error('候选版本运行时修改了应用包，不能安装。');

    const runtimeManifest = path.join(process.resourcesPath, 'runtime', 'manifest.json');
    const runtime = JSON.parse(fs.readFileSync(runtimeManifest, 'utf8'));
    const transactionNode = path.join(transaction, 'node');
    fs.copyFileSync(path.join(process.resourcesPath, 'runtime', runtime.node), transactionNode);
    fs.chmodSync(transactionNode, 0o700);
    for (const name of ['update-runner.cjs', 'update-files.cjs']) {
      fs.copyFileSync(path.join(__dirname, name), path.join(transaction, name));
    }
    const journal = {
      version: 1,
      id,
      phase: 'prepared',
      mode,
      current: this.current,
      candidate,
      identity: currentIdentity,
      candidateIdentity,
      sha256,
      previousSha256: bundleHash(this.current),
      environmentRoot: this.environment.root,
      ...(mode === 'official' ? {
        assetDigest,
        integrity: { kind: 'github-release-asset', repository: OWNER, releaseUrl, assetName, sha256: assetDigest },
      } : {}),
    };
    const transactionFile = path.join(transaction, 'transaction.json');
    atomicJson(transactionFile, journal);
    this.prepared = { file: transactionFile, transaction, version: candidateIdentity.version, mode };
    return this.prepared;
  }
  async launch(recover=false) {
    if (!this.prepared) throw new Error('No accepted candidate is prepared');
    const { file, transaction } = this.prepared;
    const log = fs.openSync(path.join(transaction, 'install.log'), 'a', 0o600);
    const env = { ...process.env };
    delete env.NODE_OPTIONS;
    delete env.NODE_PATH;
    const args = [path.join(transaction, 'update-runner.cjs'), file];
    if (recover) args.push('--recover');
    const child = spawn(path.join(transaction, 'node'), args, {
      env,
      detached: true,
      stdio: ['pipe', log, log],
    });
    fs.closeSync(log);
    await once(child, 'spawn');
    child.unref();
    const journal = JSON.parse(fs.readFileSync(file, 'utf8'));
    atomicJson(file, {
      ...journal,
      helperPid: child.pid,
      ...(!recover ? { phase: 'waiting-for-owner' } : {}),
    });
    this.helper = child;
  }
}
module.exports = { DesktopUpdates };
