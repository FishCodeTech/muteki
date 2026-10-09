const fs = require('node:fs');
const path = require('node:path');
const { randomUUID, createHash } = require('node:crypto');

function atomicJson(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
  const temporary = `${file}.${randomUUID()}.tmp`;
  try {
    fs.writeFileSync(temporary, JSON.stringify(value, null, 2) + '\n', { mode: 0o600 });
    fs.renameSync(temporary, file);
  } finally { if (fs.existsSync(temporary)) fs.unlinkSync(temporary); }
}

function desktopEnvironment({ appData, packaged, channel, root, workspace }) {
  channel ||= packaged ? 'stable' : 'dev';
  if (!['stable', 'dev', 'candidate'].includes(channel)) throw new Error('Invalid desktop environment channel');
  if (channel === 'candidate' && !root) throw new Error('Candidate requires an explicit, isolated environment directory');
  workspace = channel === 'dev' ? fs.realpathSync(workspace) : '';
  const suffix = channel === 'dev' ? `dev-${createHash('sha256').update(workspace).digest('hex').slice(0, 16)}` : channel;
  root = path.resolve(root || path.join(appData, 'Muteki', 'environments', suffix));
  if (workspace && (root === workspace || root.startsWith(workspace + path.sep))) throw new Error('Desktop data must be outside the development checkout');
  fs.mkdirSync(root, { recursive: true, mode: 0o700 });
  root = fs.realpathSync(root);
  const file = path.join(root, 'environment.json');
  if (!fs.existsSync(file)) {
    const value = { version: 1, id: randomUUID(), channel, workspace, createdAt: new Date().toISOString() };
    try { fs.writeFileSync(file, JSON.stringify(value, null, 2) + '\n', { flag: 'wx', mode: 0o600 }); }
    catch (error) { if (error.code !== 'EEXIST') throw error; }
  }
  const manifest = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (manifest.version !== 1 || !/^[a-f0-9-]{36}$/.test(manifest.id) || manifest.channel !== channel || manifest.workspace !== workspace) {
    throw new Error(`Desktop environment identity does not match this launch: ${file}`);
  }
  const paths = Object.fromEntries(['state', 'sessions', 'desktop', 'home', 'logs', 'tmp', 'tools'].map(name => [name, path.join(root, name)]));
  for (const directory of Object.values(paths)) fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  return { ...manifest, root, paths, sharedCredentials: path.join(appData, 'Muteki', 'shared', 'credentials'), generation: randomUUID(), name: channel === 'dev' ? 'Muteki Dev' : channel === 'candidate' ? 'Muteki Candidate' : 'Muteki' };
}

function serviceEnvironment(context, parent = process.env) {
  // No inherited Python/Node injection, provider secrets, MUTEKI_* settings or
  // native CLI homes. Tool binaries are discovered only in explicit locations.
  const env = {};
  for (const key of ['SYSTEMROOT', 'SystemRoot', 'WINDIR', 'COMSPEC', 'PATHEXT', 'LANG', 'LC_ALL', 'LC_CTYPE', 'TZ', 'DISPLAY', 'WAYLAND_DISPLAY', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS', 'SSL_CERT_FILE', 'SSL_CERT_DIR']) {
    if (parent[key]) env[key] = parent[key];
  }
  Object.assign(env, {
    HOME: context.paths.home, USERPROFILE: context.paths.home,
    XDG_CONFIG_HOME: path.join(context.paths.home, '.config'), XDG_CACHE_HOME: path.join(context.paths.home, '.cache'), XDG_DATA_HOME: path.join(context.paths.home, '.local', 'share'),
    TMPDIR: context.paths.tmp, TMP: context.paths.tmp, TEMP: context.paths.tmp,
    PATH: [path.join(context.paths.tools, 'bin'), '/usr/local/bin', '/opt/homebrew/bin', '/usr/bin', '/bin', ...(process.platform === 'win32' ? [path.join(parent.SystemRoot || 'C:\\Windows', 'System32')] : [])].join(path.delimiter),
    MUTEKI_ENVIRONMENT_ROOT: context.root, MUTEKI_ENVIRONMENT_ID: context.id,
    MUTEKI_ENVIRONMENT_CHANNEL: context.channel, MUTEKI_MANAGED_DESKTOP: '1',
    MUTEKI_SESSIONS_ROOT: context.paths.sessions, MUTEKI_STATE_ROOT: context.paths.state,
    MUTEKI_COORDINATOR_CONTROL_ROOT: path.join(context.paths.state, 'control'),
    MUTEKI_COORDINATOR_GRAPH_ROOT: path.join(context.paths.state, 'control', 'graphs'),
    MUTEKI_ENV_FILE: path.join(context.root, 'config', '.env'), MUTEKI_HOST_DISCOVERY: '0',
    MUTEKI_WEB_BIND: '127.0.0.1', MUTEKI_UI_HOST: '127.0.0.1', MUTEKI_CONTROL_BIND: '127.0.0.1', MUTEKI_CONTROL_PORT: '0',
    PYTHONDONTWRITEBYTECODE: '1', PYTHONNOUSERSITE: '1', PYTHONUNBUFFERED: '1',
    NODE_ENV: 'production', NEXT_TELEMETRY_DISABLED: '1',
    NPM_CONFIG_PREFIX: context.paths.tools, NPM_CONFIG_CACHE: path.join(context.paths.home, '.npm'),
  });
  if (context.channel !== 'candidate') env.MUTEKI_SHARED_CREDENTIALS_ROOT = context.sharedCredentials;
  if (context.maintenance) env.MUTEKI_DESKTOP_MAINTENANCE = '1';
  return env;
}

function runtimePaths({ packaged, resources, workspace, node = process.env.MUTEKI_DESKTOP_NODE || process.env.npm_node_execpath || process.execPath }) {
  if (!packaged) {
    return { code: workspace, python: path.join(workspace, '.venv', process.platform === 'win32' ? 'Scripts/python.exe' : 'bin/python'), node,
      next: path.join(workspace, 'apps/desktop/build/runtime/web/server.js'), release: 'development' };
  }
  const root = path.join(resources, 'runtime');
  const manifest = JSON.parse(fs.readFileSync(path.join(root, 'manifest.json'), 'utf8'));
  if (manifest.version !== 1 || manifest.platform !== process.platform || manifest.arch !== process.arch) throw new Error('Bundled runtime does not match this platform');
  const inside = relative => {
    const result = path.resolve(root, relative);
    if (!result.startsWith(root + path.sep)) throw new Error('Invalid bundled runtime path');
    return result;
  };
  return { code: inside(manifest.code), python: inside(manifest.python), node: inside(manifest.node), next: inside(manifest.next), release: manifest.release };
}

module.exports = { atomicJson, desktopEnvironment, serviceEnvironment, runtimePaths };
