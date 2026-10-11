const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { DEFAULT_ORIGIN, normalizeOrigin, normalizeDesktopRoute, allowedNavigation, partitionFor } = require('../src/policy.cjs');
const { readPreferences, writePreferences } = require('../src/preferences.cjs');

test('accepts HTTP and HTTPS service origins, with canonical session identities', () => {
  assert.equal(normalizeOrigin(' https://MUTEKI.example:443/ '), 'https://muteki.example');
  assert.equal(normalizeOrigin('http://localhost:3001/'), 'http://localhost:3001');
  assert.equal(normalizeOrigin('http://[::1]:3001'), 'http://[::1]:3001');
  assert.equal(normalizeOrigin('http://10.211.55.13:3001/'), 'http://10.211.55.13:3001');
  assert.equal(normalizeOrigin('http://muteki.example:3001/'), 'http://muteki.example:3001');
  assert.equal(normalizeOrigin('http://localhost.evil.example/'), 'http://localhost.evil.example');
  assert.equal(normalizeOrigin('http://127.0.0.1.evil.example/'), 'http://127.0.0.1.evil.example');
  assert.equal(partitionFor('https://MUTEKI.example:443/'), partitionFor('https://muteki.example'));
  assert.notEqual(partitionFor(DEFAULT_ORIGIN), partitionFor('http://127.0.0.1:3002'));
});

test('rejects credential-bearing URLs, non-root paths and non-web schemes', () => {
  for (const value of [null, {}, '', 'muteki.example', 'file:///etc/hosts', 'javascript:alert(1)',
    'https://user:password@muteki.example', 'https://muteki.example/?token=example',
    'https://muteki.example/#example', 'https://muteki.example/chat']) {
    assert.throws(() => normalizeOrigin(value));
  }
});

test('keeps service navigation on its origin and limits blob access to attachment previews', () => {
  const origin = 'https://muteki.example';
  assert.equal(allowedNavigation(`${origin}/chat/123`, origin), true);
  for (const value of ['https://other.example', 'https://muteki.example:444', 'file:///etc/hosts',
    'data:text/html,hello', 'javascript:alert(1)', 'https://user:password@muteki.example',
    'blob:https://other.example/abc']) {
    assert.equal(allowedNavigation(value, origin, { preview: true }), false, value);
  }
  assert.equal(allowedNavigation('about:blank', origin), false);
  assert.equal(allowedNavigation(`blob:${origin}/abc`, origin), false);
  assert.equal(allowedNavigation('about:blank', origin, { preview: true }), true);
  assert.equal(allowedNavigation(`blob:${origin}/abc`, origin, { preview: true }), true);
});

test('reports corrupt preferences without connecting an unrelated default service', () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-preferences-'));
  const file = path.join(directory, 'connection.json');
  try {
    assert.deepEqual(readPreferences(file), { origin: '', routes: {} });
    fs.writeFileSync(file, '{corrupt');
    assert.equal(readPreferences(file).error.code, 'desktop.preferences_invalid');
    writePreferences(file, 'https://muteki.example/');
    assert.deepEqual(JSON.parse(fs.readFileSync(file)), { origin: 'https://muteki.example', routes: {} });
    assert.throws(() => writePreferences(file, 'https://muteki.example/?token=example'));
    assert.deepEqual(readPreferences(file), { origin: 'https://muteki.example', routes: {} });
    assert.equal(fs.readdirSync(directory).some(name => name.endsWith('.tmp')), false);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('validates encoded chat routes before they reach the renderer', () => {
  assert.equal(normalizeDesktopRoute('/chat/thread-%E4%B8%AD'), '/chat/thread-%E4%B8%AD');
  for (const route of ['/chat/thread-%', '/chat/thread-%E4']) {
    assert.throws(() => normalizeDesktopRoute(route), error => error.code === 'desktop.route_invalid');
  }
});

test('shared settings declarations are covered by desktop path and method policy', () => {
  const registry = require('../../web/ui/components/settings/registry.json');
  const { allowedApiPath, allowedApiRequest } = require('../src/transport.cjs');
  assert.equal(registry.defaultPage, 'appearance');
  for (const page of Object.values(registry.pages)) for (const operation of page.requiredApis) {
    const paths = operation.match === 'prefix' ? [operation.path, operation.path + '/fixture'] : [operation.path];
    for (const target of paths) {
      assert.equal(allowedApiPath(target), true, target);
      for (const method of operation.methods) assert.equal(allowedApiRequest(target, method), true, `${page.id}: ${method} ${target}`);
    }
  }
  assert.equal(allowedApiRequest('/api/runs', 'GET'), true);
  for (const method of ['POST', 'PUT', 'PATCH', 'DELETE']) assert.equal(allowedApiRequest('/api/runs', method), false);
  for (const target of ['/api/runs/run-example', '/api/runs/run-example/control', '/api/runs-extra', '/api/domain-modules-extra', '/api/capability-management-extra']) {
    for (const method of ['GET', 'POST', 'DELETE']) assert.equal(allowedApiRequest(target, method), false, `${method} ${target}`);
  }
});

test('retention settings allow only the supported paths and methods', () => {
  const { allowedApiRequest } = require('../src/transport.cjs');
  for (const [target, permitted] of [
    ['/api/settings/run-retention', 'GET'],
    ['/api/settings/run-retention/ctf', 'PUT'],
    ['/api/settings/run-retention/pentest', 'PUT'],
  ]) {
    for (const method of ['GET', 'POST', 'PUT', 'PATCH', 'DELETE']) {
      assert.equal(allowedApiRequest(target, method), method === permitted, `${method} ${target}`);
    }
  }
  for (const target of ['/api/settings/run-retention/chat', '/api/settings/run-retention/ctf/extra', '/api/settings/run-retention-extra']) {
    for (const method of ['GET', 'PUT']) assert.equal(allowedApiRequest(target, method), false, `${method} ${target}`);
  }
});

test('anchor targets are transient while path and query remain durable', () => {
  const { desktopNavigationTarget } = require('../src/policy.cjs');
  for (const route of ['/settings/agents?engine=codex#setting-usage-cursor-account', '/usage#limits', '/ctf#progress']) {
    const { pathname, search, hash } = new URL(route, 'muteki-desktop://app');
    assert.deepEqual(desktopNavigationTarget(route), {route: pathname + search, hash});
    assert.equal(normalizeDesktopRoute(route), pathname + search);
  }
  assert.throws(() => desktopNavigationTarget('https://other.example/settings/agents#setting-agents-models'));
});

test('preflight does not authorize a runs mutation', async () => {
  const { forwardService } = require('../src/transport.cjs');
  const scope = {origin: 'http://127.0.0.1:18193', host: 'service-policy', requests: new Set(), closed: false};
  const preflight = method => forwardService(new Request('muteki-desktop://service-policy/api/runs', {
    method: 'OPTIONS', headers: {'Access-Control-Request-Method': method},
  }), scope, () => {});
  const read = await preflight('GET');
  assert.equal(read.status, 204);
  assert.equal(read.headers.get('Access-Control-Allow-Methods'), 'GET');
  assert.equal((await preflight('DELETE')).status, 403);
});

test('desktop environment identity persists and runtime inheritance is explicit', t => {
  const { desktopEnvironment, serviceEnvironment } = require('../src/environment.cjs');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-environment-'));
  try {
    const options = {appData: root, root: path.join(root, 'stable'), packaged: true, channel: 'stable'};
    const first = desktopEnvironment(options), second = desktopEnvironment(options);
    assert.equal(first.id, second.id);
    assert.notEqual(first.generation, second.generation);
    assert.throws(() => desktopEnvironment({...options, channel: 'candidate'}));
    const host = path.join(root, 'host');
    t.mock.method(os, 'userInfo', () => ({homedir: host}));
    fs.mkdirSync(path.join(host, '.nvm', 'versions', 'node', 'v99.1.0', 'bin'), {recursive: true});
    const env = serviceEnvironment(first, {HOME: host, PATH: '/host/path', PYTHONPATH: '/checkout', NODE_OPTIONS: '--inspect', MUTEKI_STATE_ROOT: '/wrong', OPENAI_API_KEY: 'fixture', LANG: 'en_US.UTF-8'});
    for (const name of ['PYTHONPATH', 'NODE_OPTIONS', 'OPENAI_API_KEY']) assert.equal(env[name], undefined);
    assert.equal(env.MUTEKI_STATE_ROOT, first.paths.state);
    assert.equal(env.MUTEKI_HOST_DISCOVERY, '1');
    assert.equal(env.HOME, host);
    assert.ok(env.PATH.split(path.delimiter).includes('/host/path'));
    assert.ok(env.PATH.split(path.delimiter).includes(path.join(host, '.nvm', 'versions', 'node', 'v99.1.0', 'bin')));
    assert.equal(env.LANG, 'en_US.UTF-8');
    const candidate = desktopEnvironment({...options, root: path.join(root, 'candidate'), channel: 'candidate'});
    t.mock.method(os, 'userInfo', () => { throw new Error('Candidate read the host account'); });
    t.mock.method(fs, 'readdirSync', () => { throw new Error('Candidate inspected native tool directories'); });
    const candidateEnv = serviceEnvironment(candidate, {HOME: host, PATH: '/host/path', CODEX_HOME: '/host/codex', XDG_DATA_HOME: '/host/data', OPENAI_API_KEY: 'fixture', MUTEKI_HOST_DISCOVERY: '1'});
    assert.equal(candidateEnv.HOME, candidate.paths.home);
    assert.equal(candidateEnv.MUTEKI_HOST_DISCOVERY, '0');
    assert.ok(!candidateEnv.PATH.split(path.delimiter).includes('/host/path'));
    assert.ok(!candidateEnv.PATH.includes(path.join(host, '.nvm')));
    assert.equal(candidateEnv.OPENAI_API_KEY, undefined);
    assert.equal(candidateEnv.CODEX_HOME, undefined);
    assert.equal(candidateEnv.XDG_DATA_HOME, path.join(candidate.paths.home, '.local', 'share'));
  } finally { fs.rmSync(root, {recursive: true, force: true}); }
});

test('stable and dev desktop normalize inherited Agent homes to the OS account', t => {
  const { desktopEnvironment, serviceEnvironment } = require('../src/environment.cjs');
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-native-home-'));
  const host = path.join(root, 'host');
  t.mock.method(os, 'userInfo', () => ({homedir: host}));
  t.mock.method(os, 'homedir', () => { throw new Error('Environment-dependent home lookup'); });
  try {
    const privateRoot = path.join(root, 'stable', 'state', '_platform', 'chat-plugins', 'sessions', 'fixture');
    const parent = {HOME: path.join(privateRoot, 'home'), USERPROFILE: path.join(privateRoot, 'home'),
      XDG_DATA_HOME: path.join(privateRoot, 'data'), XDG_CONFIG_HOME: path.join(privateRoot, 'home', '.config'), XDG_CACHE_HOME: path.join(privateRoot, 'cache'),
      CODEX_HOME: path.join(privateRoot, 'home', '.codex'), CLAUDE_CONFIG_DIR: path.join(privateRoot, 'home', '.claude'), GROK_HOME: path.join(privateRoot, 'home', '.grok'),
      CURSOR_CONFIG_DIR: path.join(privateRoot, 'home', '.cursor'), KIMI_CODE_HOME: path.join(privateRoot, 'home', '.kimi-code'), PI_CODING_AGENT_DIR: path.join(privateRoot, 'home', '.pi', 'agent'),
      PATH: [path.join(privateRoot, 'home', 'bin'), '/usr/bin'].join(path.delimiter), MUTEKI_CHAT_PRIVATE_ROOT: privateRoot};
    for (const channel of ['stable', 'dev']) {
      const context = desktopEnvironment({appData: root, root: path.join(root, channel), packaged: channel === 'stable', channel, workspace: process.cwd()});
      const env = serviceEnvironment(context, parent);
      assert.equal(env.HOME, host); assert.equal(env.USERPROFILE, host);
      assert.equal(env.XDG_DATA_HOME, path.join(host, '.local', 'share'));
      assert.equal(env.XDG_CONFIG_HOME, path.join(host, '.config'));
      assert.equal(env.XDG_CACHE_HOME, path.join(host, '.cache'));
      for (const key of ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'GROK_HOME', 'CURSOR_CONFIG_DIR', 'KIMI_CODE_HOME', 'PI_CODING_AGENT_DIR', 'MUTEKI_CHAT_PRIVATE_ROOT']) assert.equal(env[key], undefined, key);
      assert.ok(!env.PATH.includes(privateRoot));
      assert.ok(env.PATH.split(path.delimiter).includes(path.join(host, '.local', 'bin')));
    }
    assert.equal(parent.HOME, path.join(privateRoot, 'home'));
  } finally { fs.rmSync(root, {recursive: true, force: true}); }
});

test('native custom roots survive trusted launches while managed roots are removed', t => {
  const { hostEnvironment, serviceEnvironment } = require('../src/environment.cjs');
  const home = path.resolve('/native-account');
  t.mock.method(os, 'userInfo', () => ({homedir: home}));
  const parent = {HOME: home, USERPROFILE: home, CODEX_HOME: '/native-custom/codex', CLAUDE_CONFIG_DIR: '/native-custom/claude',
    XDG_DATA_HOME: '/native-custom/data', XDG_CONFIG_HOME: '/native-custom/config', XDG_CACHE_HOME: '/native-custom/cache',
    GROK_HOME: '/desktop/state/_platform/chat-plugins/sessions/id/home/.grok'};
  const host = hostEnvironment(parent);
  const env = serviceEnvironment({channel: 'stable', root: '/desktop', paths: {home: '/desktop/home', tools: '/desktop/tools', tmp: '/desktop/tmp', sessions: '/desktop/sessions', state: '/desktop/state'}, sharedCredentials: '/shared'}, parent);
  for (const key of ['CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME']) {
    assert.equal(host[key], parent[key]); assert.equal(env[key], parent[key]);
  }
  assert.equal(host.GROK_HOME, undefined); assert.equal(env.GROK_HOME, undefined);
});

test('update recovery relaunch uses the OS account and excludes inherited private roots', async t => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'muteki-update-home-'));
  const host = path.join(root, 'host');
  const privateRoot = path.join(root, 'state', '_platform', 'chat-plugins', 'sessions', 'id');
  t.mock.method(os, 'userInfo', () => ({homedir: host}));
  const original = {...process.env};
  let launched;
  t.mock.method(require('node:child_process'), 'spawn', (executable, args, options) => {
    launched = {executable, args, options}; return {unref() {}};
  });
  const runner = require.resolve('../src/update-runner.cjs');
  delete require.cache[runner];
  try {
    Object.assign(process.env, {HOME: path.join(privateRoot, 'home'), USERPROFILE: path.join(privateRoot, 'home'), CODEX_HOME: path.join(privateRoot, 'home', '.codex'), XDG_DATA_HOME: path.join(privateRoot, 'data'), MUTEKI_CHAT_PRIVATE_ROOT: privateRoot});
    const file = path.join(root, 'transaction.json');
    fs.writeFileSync(file, JSON.stringify({id: 'fixture', phase: 'waiting-for-owner', environmentRoot: root, current: path.join(root, 'Muteki.app'), identity: {executable: 'Muteki'}}));
    await require(runner).run(file, true);
    assert.equal(JSON.parse(fs.readFileSync(file)).phase, 'rolled-back');
    assert.equal(launched.options.env.HOME, host);
    assert.equal(launched.options.env.XDG_DATA_HOME, path.join(host, '.local', 'share'));
    assert.equal(launched.options.env.CODEX_HOME, undefined);
    assert.equal(launched.options.env.MUTEKI_CHAT_PRIVATE_ROOT, undefined);
    assert.deepEqual(launched.args, [`--muteki-environment-root=${root}`]);
  } finally {
    for (const key of Object.keys(process.env)) if (!(key in original)) delete process.env[key];
    Object.assign(process.env, original); delete require.cache[runner];
    fs.rmSync(root, {recursive: true, force: true});
  }
});

test('managed partitions follow environment identity across port changes', () => {
  const { partitionFor } = require('../src/policy.cjs');
  assert.equal(partitionFor('http://127.0.0.1:41001', 'stable-id'), partitionFor('http://127.0.0.1:42001', 'stable-id'));
  assert.notEqual(partitionFor('http://127.0.0.1:41001', 'stable-id'), partitionFor('http://127.0.0.1:41001', 'dev-id'));
  assert.notEqual(partitionFor('http://127.0.0.1:41001'), partitionFor('http://127.0.0.1:42001'));
});
