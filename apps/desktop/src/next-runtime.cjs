// Production Next handler with an owned socket and a private readiness pipe.
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { once } = require('node:events');
const readline = require('node:readline');

async function main() {
  const lines = readline.createInterface({ input: process.stdin });
  const [line] = await once(lines, 'line');
  const input = JSON.parse(line);
  const dir = path.dirname(input.next);
  const config = JSON.parse(fs.readFileSync(path.join(dir, '.next/required-server-files.json'), 'utf8')).config;
  config.distDir = '.next';
  // Packaged source is immutable. ISR and image caches must not write into it.
  config.experimental = { ...config.experimental, isrFlushToDisk: false };
  config.images = { ...config.images, unoptimized: true };
  process.env.__NEXT_PRIVATE_STANDALONE_CONFIG = JSON.stringify(config);
  process.env.MUTEKI_BACKEND = input.backend;
  const { getRequestHandlers } = require(path.join(dir, 'node_modules/next/dist/server/lib/start-server'));
  let handlers;
  let port;
  const trustedHost = request => [`127.0.0.1:${port}`, `localhost:${port}`].includes(request.headers.host);
  const server = http.createServer((request, response) => {
    if (!trustedHost(request)) { response.writeHead(403); response.end('Invalid workspace host'); return; }
    if (!handlers) { response.writeHead(503); response.end('Preparing workspace'); return; }
    void handlers.requestHandler(request, response);
  });
  const bind = port => new Promise((resolve, reject) => {
    const failed = error => { server.off('listening', ready); reject(error); };
    const ready = () => { server.off('error', failed); resolve(); };
    server.once('error', failed); server.once('listening', ready);
    server.listen(port, '127.0.0.1');
  });
  try { await bind(input.port || 0); }
  catch (error) { if (error.code !== 'EADDRINUSE') throw error; await bind(0); }
  port = server.address().port;
  handlers = await getRequestHandlers({ dir, port, hostname: '127.0.0.1', isDev: false, server, minimalMode: false, quiet: true });
  server.on('upgrade', (request, socket, head) => {
    if (!trustedHost(request) || (request.headers.origin && ![`http://127.0.0.1:${port}`, `http://localhost:${port}`].includes(request.headers.origin))) { socket.destroy(); return; }
    // Next route handlers don't proxy WebSocket upgrades. Keep them on the
    // same owned backend and preserve the browser's credentials and origin.
    if (request.url.startsWith('/api/')) {
      const upstream = http.request(new URL(request.url, input.backend), { headers: { ...request.headers, host: new URL(input.backend).host, ...(request.headers.origin ? { origin: input.backend } : {}) } });
      upstream.on('upgrade', (reply, peer, buffered) => {
        socket.write(`HTTP/1.1 101 Switching Protocols\r\n${Object.entries(reply.headers).map(([key, value]) => `${key}: ${value}`).join('\r\n')}\r\n\r\n`);
        if (buffered.length) socket.write(buffered);
        if (head.length) peer.write(head);
        peer.pipe(socket); socket.pipe(peer);
        socket.on('error', () => peer.destroy()); peer.on('error', () => socket.destroy());
      });
      upstream.on('response', reply => { socket.end(`HTTP/1.1 ${reply.statusCode} Rejected\r\nConnection: close\r\n\r\n`); reply.resume(); });
      upstream.on('error', () => socket.destroy()); socket.on('close', () => upstream.destroy()); upstream.end();
    } else void handlers.upgradeHandler(request, socket, head);
  });
  fs.writeSync(3, JSON.stringify({ environment_id: input.environment_id, generation: input.generation, port }) + '\n');
  const stop = () => { server.close(); server.closeAllConnections(); process.exit(0); };
  lines.on('close', stop); process.on('SIGTERM', stop); process.on('SIGINT', stop);
}
main().catch(error => { console.error(error); process.exit(1); });
