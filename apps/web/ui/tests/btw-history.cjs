/* Exercise the real panel hooks with mocked HTTP and presentational widgets. */
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const Module = require('node:module');
const ts = require('typescript');
const React = require('react');
const { act, create } = require('react-test-renderer');
global.IS_REACT_ACT_ENVIRONMENT = true;
global.window = { confirm: () => true };
let apiFetch;
const widget = ({ children }) => React.createElement('div', null, children);
const originalLoad = Module._load;
Module._load = function (request, ...args) {
  if (request === '@heroui/react') return { Drawer: Object.fromEntries(['Backdrop', 'Content', 'Dialog', 'Header', 'Body', 'Footer'].map(k => [k, widget])), Spinner: widget };
  if (request === '@/components/Icon') return { Icon: () => null };
  if (request === '@/components/ai-native/message-markdown') return { MessageMarkdown: ({ text }) => React.createElement('p', null, text) };
  if (request === '@/lib/i18n') return { useT: () => key => key };
  if (request === '@/lib/useRun') return { apiFetch: (...a) => apiFetch(...a) };
  if (request.endsWith('.module.css')) return {};
  return originalLoad.call(this, request, ...args);
};
const filename = path.resolve(__dirname, '../components/BtwPanel.tsx');
const compiled = ts.transpileModule(fs.readFileSync(filename, 'utf8'), {
  compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, esModuleInterop: true },
}).outputText;
const componentModule = new Module(filename, module);
componentModule.filename = filename;
componentModule.paths = module.paths;
componentModule._compile(compiled, filename);
const { BtwPanel } = componentModule.exports;
Module._load = originalLoad;
const response = turns => ({ ok: true, json: async () => ({ turns }) });
const row = (id, question, answer) => ({ id, question, answer, created_at: 1000, status: 'completed', engine: 'codex', model: 'fake' });
const text = renderer => JSON.stringify(renderer.toJSON());


test('history loads on open, survives reopen, and deleting removes a question/answer pair', async () => {
  let history = [row('one', 'Saved question', 'Saved answer')];
  apiFetch = async (url, options = {}) => {
    if (options.method === 'DELETE') history = [];
    return response(history);
  };
  let renderer;
  await act(async () => { renderer = create(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })); });
  assert.match(text(renderer), /Saved answer/);
  await act(async () => renderer.update(React.createElement(BtwPanel, { open: false, runId: 'a', onClose() {} })));
  await act(async () => renderer.update(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })));
  assert.match(text(renderer), /Saved question/);
  await act(async () => renderer.root.findByProps({ 'aria-label': '删除这条问答' }).props.onClick());
  assert.doesNotMatch(text(renderer), /Saved question|Saved answer/);
  await act(async () => renderer.unmount());
});

test('late history response from another run cannot overwrite the selected run', async () => {
  let resolveA;
  apiFetch = (url) => url.includes('/a/') ? new Promise(resolve => { resolveA = resolve; }) : Promise.resolve(response([row('b', 'Question B', 'Answer B')]));
  let renderer;
  await act(async () => { renderer = create(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })); });
  await act(async () => renderer.update(React.createElement(BtwPanel, { open: true, runId: 'b', onClose() {} })));
  await act(async () => resolveA(response([row('a', 'Question A', 'Answer A')])));
  assert.match(text(renderer), /Answer B/);
  assert.doesNotMatch(text(renderer), /Answer A/);
  await act(async () => renderer.unmount());
});

test('clear history sends DELETE and removes all saved turns', async () => {
  let history = [row('one', 'Question one', 'Answer one'), row('two', 'Question two', 'Answer two')];
  const requests = [];
  apiFetch = async (url, options = {}) => {
    requests.push([url, options.method]);
    if (options.method === 'DELETE') history = [];
    return response(history);
  };
  let renderer;
  await act(async () => { renderer = create(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })); });
  await act(async () => renderer.root.findByProps({ 'aria-label': '清空 BTW 历史' }).props.onClick());
  assert.ok(requests.some(([url, method]) => url === '/api/runs/a/btw/history' && method === 'DELETE'));
  assert.doesNotMatch(text(renderer), /Answer one|Answer two/);
  await act(async () => renderer.unmount());
});

test('reopening observes the existing background job without POSTing a new question', async () => {
  const requests = [];
  let poll;
  global.window.setInterval = fn => { poll = fn; return 1; };
  global.window.clearInterval = () => {};
  let active = true;
  apiFetch = async (url, options) => {
    requests.push([url, options?.method || 'GET']);
    return { ok: true, json: async () => ({
      turns: [{ ...row('one', 'Question', active ? 'Partial' : 'Complete'), status: active ? 'incomplete' : 'completed' }],
      active, progress: { phase: 'running' },
    }) };
  };
  let renderer;
  await act(async () => { renderer = create(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })); });
  await act(async () => renderer.update(React.createElement(BtwPanel, { open: false, runId: 'a', onClose() {} })));
  await act(async () => renderer.update(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })));
  assert.match(text(renderer), /Partial/);
  active = false;
  await act(async () => poll());
  assert.match(text(renderer), /Complete/);
  assert.ok(requests.every(([, method]) => method === 'GET'));
  await act(async () => renderer.unmount());
});

test('stop explicitly cancels the server job instead of only disconnecting', async () => {
  global.window.setInterval = () => 1;
  global.window.clearInterval = () => {};
  const requests = [];
  let active = true;
  apiFetch = async (url, options) => {
    requests.push([url, options?.method || 'GET']);
    if (url.endsWith('/cancel')) active = false;
    return { ok: true, json: async () => ({ turns: [row('one', 'Question', 'Partial')], active }) };
  };
  let renderer;
  await act(async () => { renderer = create(React.createElement(BtwPanel, { open: true, runId: 'a', onClose() {} })); });
  const stop = renderer.root.findAllByType('button').find(button => button.children.includes('停止'));
  assert.ok(stop);
  await act(async () => stop.props.onClick());
  assert.ok(requests.some(([url, method]) => url === '/api/runs/a/btw/cancel' && method === 'POST'));
  await act(async () => renderer.unmount());
});
