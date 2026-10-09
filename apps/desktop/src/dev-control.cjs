// Explicitly enabled only in a Dev environment. No production debug listener.
const http = require('node:http');
const { randomBytes, timingSafeEqual } = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { atomicJson } = require('./environment.cjs');
const { pageRead, pageTarget } = require('./preview-control.cjs');

async function startDevControl(environment, targets, status, stop) {
  if (environment.channel !== 'dev') throw new Error('Desktop automation is available only in a Dev environment');
  const token = randomBytes(32).toString('hex');
  const descriptor = path.join(environment.root, 'dev-control.json');
  const epochs = new WeakMap();
  const publicTarget = target => {
    if (!epochs.has(target.contents)) {
      epochs.set(target.contents, 0);
      target.contents.on('did-start-navigation', details => { if (details.isMainFrame) epochs.set(target.contents, epochs.get(target.contents) + 1); });
    }
    return { id: String(target.contents.id), windowId: target.windowId, kind: target.kind,
      url: target.contents.getURL(), title: target.contents.getTitle(), environmentId: environment.id,
      generation: `${environment.generation}:${target.contents.id}:${epochs.get(target.contents)}:${target.revision || ''}`, managed: target.managed, errors: target.errors || [],
      ...(target.windowState ? { window: target.windowState } : {}) };
  };
  const server = http.createServer(async (request, response) => {
    const reply = (code, value) => { response.writeHead(code, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' }); response.end(JSON.stringify(value)); };
    const supplied = Buffer.from(request.headers.authorization || '');
    const expected = Buffer.from(`Bearer ${token}`);
    if (request.headers.origin || supplied.length !== expected.length || !timingSafeEqual(supplied, expected)) return reply(403, {code: 'dev.control_unauthorized'});
    try {
      if (request.method === 'GET' && request.url === '/status') return reply(200, { environmentId: environment.id, generation: environment.generation, pid: process.pid, targets: targets().map(publicTarget), service: status() });
      if (request.method !== 'POST' || !['/action', '/stop'].includes(request.url)) return reply(404, {code: 'dev.control_not_found'});
      let body = '';
      for await (const chunk of request) {
        body += chunk;
        if (Buffer.byteLength(body) > 1024 * 1024) return reply(413, {code: 'dev.control_request_too_large'});
      }
      const input = JSON.parse(body);
      if (request.url === '/stop') {
        if (input.generation !== environment.generation) return reply(409, {code: 'dev.target_expired'});
        await stop(); return reply(200, {stopping: true});
      }
      const current = () => {
        const target = targets().find(item => String(item.contents.id) === input.target);
        if (!target || target.contents.isDestroyed()) throw Object.assign(new Error('Dev target is no longer available'), {code: 'dev.target_missing'});
        if (input.generation !== publicTarget(target).generation) throw Object.assign(new Error('Dev has restarted or navigated; acquire a new target and snapshot'), {code: 'dev.target_expired'});
        if (!target.managed) throw Object.assign(new Error('This window is connected to an external service; Dev automation is unavailable'), {code: 'dev.external_service'});
        return target;
      };
      const target = current(), contents = target.contents;
      let result;
      if (input.action === 'snapshot') {
        result = await contents.executeJavaScript(`(${pageRead.toString()})(${JSON.stringify({ mode: 'elements' })})`);
        current();
      } else if (input.action === 'screenshot') {
        result = { png: (await contents.capturePage()).toPNG().toString('base64') };
        current();
      } else if (['click', 'type'].includes(input.action)) {
        if (!Number.isSafeInteger(input.ref) || input.ref < 1) throw new Error('Use an element ref from a fresh snapshot');
        const point = await contents.executeJavaScript(`(${pageTarget.toString()})(${JSON.stringify({ ref: input.ref })})`);
        current();
        if (point.missing || point.covered_by) throw Object.assign(new Error('Target element is missing or covered; take a new snapshot'), {code: 'dev.element_unavailable'});
        const position = {x: Math.round(point.x), y: Math.round(point.y), button: 'left', clickCount: 1};
        contents.sendInputEvent({type: 'mouseDown', ...position}); contents.sendInputEvent({type: 'mouseUp', ...position});
        if (input.action === 'type') {
          if (typeof input.text !== 'string') throw new Error('type requires text');
          await contents.insertText(input.text);
        }
        result = { performed: true, target: publicTarget(current()) };
      } else throw new Error('Supported actions: snapshot, screenshot, click, type');
      return reply(200, result);
    } catch (error) { reply(409, {code: error.code || 'dev.control_failed', message: error.message}); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  atomicJson(descriptor, {version: 1, origin: `http://127.0.0.1:${server.address().port}`, token, environmentId: environment.id, generation: environment.generation, pid: process.pid});
  return () => {
    server.close(); server.closeAllConnections();
    if (fs.existsSync(descriptor) && JSON.parse(fs.readFileSync(descriptor, 'utf8')).pid === process.pid) fs.unlinkSync(descriptor);
  };
}
module.exports = { startDevControl };
