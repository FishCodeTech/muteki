#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { copyFile, mkdir, stat } from 'node:fs/promises';
import { createReadStream } from 'node:fs';
import { createServer, request as httpRequest } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const output = path.join(root, 'dist/web');
const args = new Set(process.argv.slice(2));
const allowed = new Set(['--help', '-h', '--serve', '--serve-only']);
if ([...args].some(arg => !allowed.has(arg))) {
  console.error('Unknown option. Use node scripts/build-web.mjs --help.');
  process.exit(1);
}
if (args.has('--help') || args.has('-h')) {
  console.log(`Build: node scripts/build-web.mjs
Build and serve: node scripts/build-web.mjs --serve
Serve existing build: node scripts/build-web.mjs --serve-only

Requires Rust target wasm32-unknown-unknown and sccache on PATH.
Output: dist/web. UI is Rust/Wasm; nsh.js is NSH's supplied DOM runtime.
Server: http://127.0.0.1:3101. Proxies /api and /health to http://127.0.0.1:8100.
Environment: PORT, MUTEKI_API_ORIGIN (proxy upstream).
Run the backend separately: cargo run -p muteki-server`);
  process.exit(0);
}

function cargo(arguments_, wasm = false) {
  const env = {
    ...process.env,
    CARGO_TARGET_DIR: path.join(root, '.cache/target'),
    RUSTC_WRAPPER: 'sccache', CARGO_INCREMENTAL: '0',
    SCCACHE_DIR: path.join(root, '.cache/sccache'),
    SCCACHE_SERVER_UDS: path.join(root, '.cache/sccache.sock'),
  };
  if (wasm) {
    const flags = env.CARGO_ENCODED_RUSTFLAGS?.split('\x1f') ?? (env.RUSTFLAGS ? env.RUSTFLAGS.split(/\s+/).filter(Boolean) : []);
    env.CARGO_ENCODED_RUSTFLAGS = [...flags, '-C', 'link-arg=-zstack-size=8388608', '--cfg', 'getrandom_backend="custom"'].join('\x1f');
    delete env.RUSTFLAGS;
  }
  const result = spawnSync('cargo', arguments_, { cwd: root, env, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`cargo ${arguments_.join(' ')} failed (${result.status ?? result.signal})`);
}

async function build() {
  await mkdir(output, { recursive: true });
  cargo(['run', '-p', 'muteki-desktop', '--', '--export-web-assets', path.join(output, 'assets')]);
  cargo(['build', '-p', 'muteki-web', '--target', 'wasm32-unknown-unknown', '--profile', 'web'], true);
  await copyFile(path.join(root, '.cache/target/wasm32-unknown-unknown/web/muteki_web.wasm'), path.join(output, 'muteki.wasm'));
  await copyFile(path.join(root, 'vendor/upstream-nsh/crates/nsh-platform-web/web/nsh.js'), path.join(output, 'nsh.js'));
  const metadata = spawnSync('cargo', ['metadata', '--format-version', '1', '--locked'], { cwd: root, encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  if (metadata.error) throw metadata.error;
  if (metadata.status !== 0) throw new Error(`Cannot resolve syntax assets: ${metadata.stderr}`);
  const syntaxPackage = JSON.parse(metadata.stdout).packages.find(package_ => package_.name === 'two-face');
  if (syntaxPackage) await copyFile(path.join(path.dirname(syntaxPackage.manifest_path), 'generated/syntaxes-fancy-newlines.bin'), path.join(output, 'nsh-syntaxes.bin'));
  await copyFile(path.join(root, 'crates/web/index.html'), path.join(output, 'index.html'));
  console.log(`Built Rust web app: ${output}`);
}

async function serve() {
  await stat(path.join(output, 'index.html'));
  const upstream = new URL(process.env.MUTEKI_API_ORIGIN ?? 'http://127.0.0.1:8100');
  if (upstream.protocol !== 'http:') throw new Error('MUTEKI_API_ORIGIN must use http:// for the local service.');
  const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.wasm': 'application/wasm', '.json': 'application/json', '.png': 'image/png', '.svg': 'image/svg+xml', '.jpg': 'image/jpeg', '.webp': 'image/webp', '.ttf': 'font/ttf', '.otf': 'font/otf' };
  const server = createServer(async (request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    if (url.pathname === '/api' || url.pathname.startsWith('/api/') || url.pathname === '/health') {
      const proxy = httpRequest(new URL(request.url, upstream), { method: request.method, headers: { ...request.headers, host: upstream.host } }, reply => {
        response.writeHead(reply.statusCode ?? 502, reply.headers);
        reply.pipe(response);
      });
      proxy.on('error', error => {
        if (!response.headersSent) response.writeHead(502, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ error: `Local Muteki service unavailable: ${error.message}` }));
      });
      request.on('aborted', () => proxy.destroy());
      request.pipe(proxy);
      return;
    }
    try {
      const pathname = decodeURIComponent(url.pathname);
      const file = path.resolve(output, `.${pathname === '/' ? '/index.html' : pathname}`);
      if (!file.startsWith(`${output}${path.sep}`)) { response.writeHead(403); response.end(); return; }
      if (!(await stat(file)).isFile()) throw new Error('not a file');
      response.writeHead(200, { 'content-type': types[path.extname(file)] ?? 'application/octet-stream', 'cache-control': 'no-cache' });
      createReadStream(file).on('error', () => response.destroy()).pipe(response);
    } catch { response.writeHead(404); response.end('Not found'); }
  });
  const port = Number(process.env.PORT ?? 3101);
  server.on('error', error => { console.error(`Cannot serve Muteki: ${error.message}`); process.exitCode = 1; });
  server.listen(port, '127.0.0.1', () => console.log(`Muteki: http://127.0.0.1:${port} (API: ${upstream.origin})`));
}

try {
  if (!args.has('--serve-only')) await build();
  if (args.has('--serve') || args.has('--serve-only')) await serve();
} catch (error) {
  console.error(`Web build failed: ${error.message}`);
  process.exitCode = 1;
}
