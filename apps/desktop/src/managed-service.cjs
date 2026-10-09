const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { EventEmitter } = require('node:events');
const { randomUUID } = require('node:crypto');
const { atomicJson, serviceEnvironment } = require('./environment.cjs');

class ManagedService extends EventEmitter {
  constructor(context, runtime) {
    super(); this.context = context; this.runtime = runtime; this.children = new Set(); this.ready = null; this.pending = null; this.stopping = false;
    this.status = { state: 'stopped', environmentId: context.id, channel: context.channel, logs: context.paths.logs }; this.stopPending = null;
  }
  changed(patch) { Object.assign(this.status, patch); this.emit('state', { ...this.status }); }
  async child(name, executable, args, bootstrap) {
    if (!fs.existsSync(executable)) throw new Error(`Missing ${name} runtime: ${executable}`);
    const log = fs.openSync(path.join(this.context.paths.logs, `${name}.log`), 'a', 0o600);
    const env = serviceEnvironment(this.context);
    env.PATH = [path.dirname(this.runtime.node), path.dirname(this.runtime.python), env.PATH].join(path.delimiter);
    let child;
    try { child = spawn(executable, args, { cwd: this.context.root, env, detached: process.platform !== 'win32', windowsHide: true, stdio: ['pipe', log, log, 'pipe'] }); }
    finally { fs.closeSync(log); }
    this.children.add(child);
    if (name === 'backend') this.backendChild = child;
    child.once('error', () => { if (!child.pid) this.children.delete(child); });
    child.once('exit', (code, signal) => {
      this.children.delete(child);
      if (!this.stopping) {
        this.ready = null;
        this.changed({ state: 'failed', error: `${name} exited (${signal || code}). See ${name}.log`, origin: undefined });
        void this.stop(false).catch(error => this.changed({ error: error.message }));
      }
    });
    const ready = await new Promise((resolve, reject) => {
      let buffer = '';
      const timer = setTimeout(() => finish(new Error(`${name} did not become ready; see ${name}.log`)), 120000);
      const failed = error => finish(error);
      const exited = (code, signal) => finish(new Error(`${name} stopped before readiness (${signal || code}); see ${name}.log`));
      const data = chunk => {
        buffer += chunk.toString('utf8');
        if (buffer.length > 65536) return finish(new Error(`${name} sent an invalid readiness message`));
        if (!buffer.includes('\n')) return;
        try { finish(null, JSON.parse(buffer.slice(0, buffer.indexOf('\n')))); }
        catch (error) { finish(error); }
      };
      const finish = (error, value) => {
        clearTimeout(timer); child.off('error', failed); child.off('exit', exited); child.stdio[3].off('data', data);
        if (error) reject(error); else resolve(value);
      };
      child.once('error', failed); child.once('exit', exited); child.stdio[3].on('data', data);
      child.stdin.on('error', failed);
      child.stdin.write(JSON.stringify(bootstrap) + '\n');
    });
    if (ready.environment_id !== this.context.id || ready.generation !== this.context.generation) throw new Error(`${name} readiness identity mismatch`);
    return ready;
  }
  start() {
    if (this.ready) return Promise.resolve(this.ready);
    if (this.pending) return this.pending;
    this.pending = this.launch().finally(() => { this.pending = null; });
    return this.pending;
  }
  async refreshSession(command = 'session') {
    if (this.refreshPending) return this.refreshPending;
    if (!this.ready || !this.backendChild || this.backendChild.exitCode !== null) throw new Error('Local workspace is not running');
    this.refreshPending = new Promise((resolve, reject) => {
      const child = this.backendChild, request_id = randomUUID();
      let buffer = '';
      const finish = (error, value) => {
        clearTimeout(timer); child.stdio[3].off('data', data); child.off('exit', exited);
        if (error) reject(error); else resolve(value);
      };
      const exited = () => finish(new Error('Local workspace stopped during session handshake'));
      const data = chunk => {
        buffer += chunk.toString();
        if (buffer.length > 65536) return finish(new Error('Invalid private session response size'));
        if (!buffer.includes('\n')) return;
        try {
          const value = JSON.parse(buffer.slice(0, buffer.indexOf('\n')));
          if (value.request_id !== request_id || typeof value.token !== 'string' || !Number.isFinite(value.expires_at)) throw new Error('Invalid private session response');
          if (!this.ready) throw new Error('Workspace changed during session handshake');
          Object.assign(this.ready, {token: value.token, expires_at: value.expires_at});
          if (command === 'activate') { this.ready.maintenance = false; this.changed({state: 'ready'}); }
          finish(null, this.ready);
        } catch (error) { finish(error); }
      };
      const timer = setTimeout(() => finish(new Error('Private session handshake timed out')), command === 'activate' ? 120000 : 10000);
      child.stdio[3].on('data', data); child.once('exit', exited);
      child.stdin.write(JSON.stringify({command, request_id}) + '\n');
    }).finally(() => { this.refreshPending = null; });
    return this.refreshPending;
  }
  async launch() {
    if (this.stopPending) await this.stopPending;
    this.context.generation = randomUUID();
    this.stopping = false;
    this.changed({ state: 'starting', error: undefined });
    try {
      const bootstrap = { environment_id: this.context.id, generation: this.context.generation, release: this.runtime.release };
      const backend = await this.child('backend', this.runtime.python, ['-I', '-B', path.join(this.runtime.code, 'apps/web/desktop_runtime.py')], bootstrap);
      const preferences = path.join(this.context.root, 'runtime-preferences.json');
      const previous = fs.existsSync(preferences) ? JSON.parse(fs.readFileSync(preferences, 'utf8')) : {};
      if (previous.uiPort !== undefined && (!Number.isInteger(previous.uiPort) || previous.uiPort < 1024 || previous.uiPort > 65535)) throw new Error('Invalid saved workspace port');
      if (!fs.existsSync(this.runtime.next)) throw new Error('Production Web UI is missing. Build the Web UI before starting the development App.');
      // Standalone Node cannot resolve Electron's virtual app.asar filesystem.
      const ui = await this.child('ui', this.runtime.node, [path.join(this.runtime.code, 'apps/desktop/src/next-runtime.cjs')], {
        ...bootstrap, next: this.runtime.next, backend: `http://127.0.0.1:${backend.api_port}`, port: previous.uiPort || 0,
      });
      const origin = `http://127.0.0.1:${ui.port}`;
      const health = await fetch(`${origin}/api/health`, { signal: AbortSignal.timeout(15000), redirect: 'error' }).then(response => response.json());
      if ((!health.ready && !(this.context.maintenance && health.desktop_runtime?.maintenance)) || health.desktop_runtime?.generation !== this.context.generation || health.desktop_runtime?.environment_id !== this.context.id) throw new Error('Workspace health identity mismatch');
      atomicJson(preferences, { uiPort: ui.port });
      this.ready = { ...backend, origin };
      const descriptor = { ...bootstrap, origin, pid: process.pid, backendPid: backend.pid, serviceId: backend.service_id };
      atomicJson(path.join(this.context.root, 'runtime.json'), descriptor);
      this.changed({ state: this.context.maintenance ? 'maintenance' : 'ready', origin, portChanged: Boolean(previous.uiPort && previous.uiPort !== ui.port), error: undefined });
      return this.ready;
    } catch (error) {
      await this.stop(false);
      this.changed({ state: 'failed', error: error.message });
      throw error;
    }
  }
  stop(report = true) {
    if (this.stopPending) return this.stopPending;
    this.stopPending = this.shutdown(report).finally(() => { this.stopPending = null; });
    return this.stopPending;
  }
  async activity() {
    if (!this.ready) return { active: 0 };
    await this.refreshSession();
    const response = await fetch(`${this.ready.origin}/api/desktop/activity`, { headers: { Authorization: `Bearer ${this.ready.token}` }, signal: AbortSignal.timeout(10000) });
    if (!response.ok) throw new Error(`Cannot verify running tasks (HTTP ${response.status}); the workspace is still running`);
    return response.json();
  }
  async drain(cancel = false) {
    await this.refreshSession();
    const response = await fetch(`${this.ready.origin}/api/desktop/drain`, { method: 'POST', headers: { Authorization: `Bearer ${this.ready.token}`, 'Content-Type': 'application/json' }, body: JSON.stringify({cancel}), signal: AbortSignal.timeout(10000) });
    const result = await response.json();
    if (!response.ok) throw new Error(result.detail || result.error?.message || 'Workspace could not enter maintenance');
    return result;
  }
  async shutdown(report) {
    this.stopping = true; this.ready = null;
    let graceful = true;
    const children = [...this.children].filter(child => child.pid && child.exitCode === null && child.signalCode === null);
    await Promise.all(children.map(async child => {
      const exited = once(child, 'exit').catch(() => {});
      child.stdin.end();
      const timer = setTimeout(() => {
        try {
          if (process.platform === 'win32') child.kill();
          else process.kill(-child.pid, 'SIGTERM');
        } catch (error) { if (error.code !== 'ESRCH') this.changed({ error: error.message }); }
      }, 5000);
      const force = setTimeout(() => {
        graceful = false;
        try { if (process.platform === 'win32') child.kill('SIGKILL'); else process.kill(-child.pid, 'SIGKILL'); }
        catch (error) { if (error.code !== 'ESRCH') this.changed({ error: error.message }); }
      }, 25000);
      await exited; clearTimeout(timer); clearTimeout(force);
    }));
    const descriptor = path.join(this.context.root, 'runtime.json');
    if (fs.existsSync(descriptor) && JSON.parse(fs.readFileSync(descriptor, 'utf8')).generation === this.context.generation) fs.unlinkSync(descriptor);
    if (report) this.changed({ state: 'stopped', origin: undefined });
    return {graceful};
  }
}
module.exports = { ManagedService };
