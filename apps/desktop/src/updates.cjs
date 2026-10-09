const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { bundleHash, bundleIdentity, copyBundle, atomicJson } = require('./update-files.cjs');

class DesktopUpdates {
  constructor(app, environment) {
    this.app = app; this.environment = environment; this.prepared = null;
    this.current = path.resolve(app.getPath('exe'), '../../..');
  }
  async prepare(source) {
    if (!this.app.isPackaged || process.platform !== 'darwin' || this.environment.channel !== 'stable') throw new Error('请在已安装的 macOS 日常版中应用候选更新。');
    source = fs.realpathSync(source);
    if (source === this.current) throw new Error('请选择新构建的候选应用。');
    const identity = bundleIdentity(this.current), candidateIdentity = bundleIdentity(source);
    if (identity.id !== candidateIdentity.id || identity.team !== candidateIdentity.team) throw new Error('候选版本的应用身份或签名团队与日常版不同。');
    const id = randomUUID(), transaction = path.join(this.environment.root, 'updates', id);
    fs.mkdirSync(transaction, {recursive: true, mode: 0o700});
    const candidate = path.join(transaction, 'candidate.app');
    copyBundle(source, candidate);
    bundleIdentity(candidate);
    const sha256 = bundleHash(candidate);
    if (sha256 !== bundleHash(source)) throw new Error('候选应用在准备过程中发生变化，请重新构建。');
    const profile = path.join(transaction, 'acceptance');
    const log = fs.openSync(path.join(transaction, 'acceptance.log'), 'a', 0o600);
    const env = {...process.env, MUTEKI_DESKTOP_USER_DATA: path.join(profile, 'desktop'), MUTEKI_DESKTOP_REGISTER_PROTOCOL: '0'};
    for (const key of ['MUTEKI_DESKTOP_URL', 'MUTEKI_ENVIRONMENT_ROOT', 'MUTEKI_DESKTOP_WORKSPACE', 'NODE_OPTIONS', 'NODE_PATH', 'ELECTRON_RUN_AS_NODE', 'PYTHONPATH', 'PYTHONHOME', 'VIRTUAL_ENV']) delete env[key];
    const child = spawn(path.join(candidate, 'Contents/MacOS', candidateIdentity.executable), ['--muteki-channel=candidate', `--muteki-environment-root=${profile}`, '--muteki-self-check'], {env, stdio: ['ignore', log, log]});
    fs.closeSync(log);
    const timer = setTimeout(() => child.kill('SIGTERM'), 150000);
    let code;
    try { [code] = await once(child, 'exit'); } finally { clearTimeout(timer); }
    const resultFile = path.join(profile, 'candidate-result.json');
    if (code !== 0 || !fs.existsSync(resultFile) || JSON.parse(fs.readFileSync(resultFile, 'utf8')).status !== 'passed') throw new Error(`候选版本验收失败，请查看 ${path.join(transaction, 'acceptance.log')}`);
    if (bundleHash(candidate) !== sha256) throw new Error('候选版本运行时修改了应用包，不能安装。');
    const file = path.join(transaction, 'transaction.json');
    const runtime = JSON.parse(fs.readFileSync(path.join(process.resourcesPath, 'runtime/manifest.json'), 'utf8'));
    fs.copyFileSync(path.join(process.resourcesPath, 'runtime', runtime.node), path.join(transaction, 'node'));
    fs.chmodSync(path.join(transaction, 'node'), 0o700);
    for (const name of ['update-runner.cjs', 'update-files.cjs']) fs.copyFileSync(path.join(__dirname, name), path.join(transaction, name));
    atomicJson(file, {version: 1, id, phase: 'prepared', current: this.current, candidate, identity, candidateIdentity, sha256, previousSha256: bundleHash(this.current), environmentRoot: this.environment.root});
    this.prepared = {file, transaction, version: candidateIdentity.version};
    return this.prepared;
  }
  async launch(recover = false) {
    if (!this.prepared) throw new Error('No accepted candidate is prepared');
    const {file, transaction} = this.prepared;
    const log = fs.openSync(path.join(transaction, 'install.log'), 'a', 0o600);
    const env = {...process.env}; delete env.NODE_OPTIONS; delete env.NODE_PATH;
    const child = spawn(path.join(transaction, 'node'), [path.join(transaction, 'update-runner.cjs'), file, ...(recover ? ['--recover'] : [])], {env, detached: true, stdio: ['pipe', log, log]});
    fs.closeSync(log); await once(child, 'spawn'); child.unref();
    const journal = JSON.parse(fs.readFileSync(file, 'utf8'));
    atomicJson(file, {...journal, helperPid: child.pid, ...(!recover ? {phase: 'waiting-for-owner'} : {})});
    // The app owns this pipe until it exits. The helper cannot replace a live bundle.
    this.helper = child;
  }
}
module.exports = { DesktopUpdates };
