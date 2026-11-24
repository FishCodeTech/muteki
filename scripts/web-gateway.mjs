import http from 'node:http';
import https from 'node:https';
import net from 'node:net';
import { readFile } from 'node:fs/promises';
import { createHmac, randomBytes } from 'node:crypto';
export async function createWebGateway({ host, port, env }) {
  const local = ['127.0.0.1', '::1', 'localhost'].includes(host);
  const cert = env.MUTEKI_WEB_TLS_CERT;
  const key = env.MUTEKI_WEB_TLS_KEY;
  if (Boolean(cert) !== Boolean(key) || (!local && !cert)) {
    throw new Error('Non-loopback Web access requires MUTEKI_WEB_TLS_CERT and MUTEKI_WEB_TLS_KEY.');
  }
  const probe = net.createServer();
  await new Promise((resolve, reject) => { probe.once('error', reject); probe.listen(0, '127.0.0.1', resolve); });
  const upstreamPort = probe.address().port;
  await new Promise(resolve => probe.close(resolve));
  env.MUTEKI_WEB_PROXY_KEY = randomBytes(32).toString('base64url');
  const scheme = cert ? 'https' : 'http';
  const headers = req => {
    const ip = req.socket.remoteAddress;
    const stamp = String(Math.floor(Date.now() / 1000));
    const payload = [stamp, ip, scheme, req.method, req.url.split('?')[0]].join('\n');
    const signature = createHmac('sha256', env.MUTEKI_WEB_PROXY_KEY).update(payload).digest('hex');
    const result = { ...req.headers, 'x-muteki-proxy-peer': [stamp, ip, scheme, signature].join('|'), 'x-forwarded-proto': scheme };
    delete result.forwarded; delete result['x-forwarded-for']; delete result['x-real-ip'];
    return result;
  };
  const handler = (req, res) => {
    const upstream = http.request({ host: '127.0.0.1', port: upstreamPort, path: req.url, method: req.method, headers: headers(req) }, response => {
      res.writeHead(response.statusCode, response.headers); response.pipe(res);
    });
    upstream.on('error', () => { if (!res.headersSent) res.writeHead(502); res.end('Web frontend unavailable'); });
    req.on('aborted', () => upstream.destroy()); res.on('close', () => upstream.destroy()); req.pipe(upstream);
  };
  const server = cert ? https.createServer({ cert: await readFile(cert), key: await readFile(key) }, handler) : http.createServer(handler);
  server.on('upgrade', (req, socket, head) => {
    const upstream = http.request({ host: '127.0.0.1', port: upstreamPort, path: req.url, method: req.method, headers: headers(req) });
    upstream.on('upgrade', (response, peer, data) => {
      socket.write(`HTTP/1.1 ${response.statusCode} ${response.statusMessage}\r\n`);
      for (const [name, value] of Object.entries(response.headers)) socket.write(`${name}: ${value}\r\n`);
      socket.write('\r\n'); if (data.length) socket.write(data); if (head.length) peer.write(head);
      peer.pipe(socket); socket.pipe(peer); socket.on('error', () => peer.destroy()); peer.on('error', () => socket.destroy());
      socket.on('close', () => peer.destroy());
    });
    upstream.on('error', () => socket.destroy()); upstream.on('response', () => socket.destroy()); upstream.end();
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(Number(port), host, resolve); });
  return { upstreamPort, origin: `${scheme}://${net.isIP(host) === 6 ? `[${host}]` : host}:${server.address().port}`, close: () => { server.close(); server.closeAllConnections(); } };
}
