const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
if (process.platform !== 'darwin') { console.log('Native Apple Speech is built only on macOS.'); process.exit(0); }
const root = path.resolve(__dirname, '..');
const prefixes = [process.env.npm_config_nodedir, path.resolve(path.dirname(fs.realpathSync(process.execPath)), '..'), path.resolve(path.dirname(process.execPath), '..')].filter(Boolean);
const headers = prefixes.map(prefix => path.join(prefix, 'include', 'node')).find(candidate => fs.existsSync(path.join(candidate, 'node_api.h')));
if (!headers) throw new Error('Node development headers are required to build Apple Speech. Set npm_config_nodedir to the Node installation containing include/node/node_api.h.');
const output = path.join(root, 'native', 'build', 'macos-speech.node');
fs.mkdirSync(path.dirname(output), { recursive: true });
const result = spawnSync('xcrun', ['clang++', '-std=c++17', '-fobjc-arc', '-fblocks', '-mmacosx-version-min=11.0', '-arch', 'arm64', '-arch', 'x86_64', '-bundle', '-undefined', 'dynamic_lookup', '-I', headers, '-framework', 'Foundation', '-framework', 'AVFoundation', '-framework', 'Speech', '-framework', 'UserNotifications', path.join(root, 'native', 'speech.mm'), '-o', output], { stdio: 'inherit' });
if (result.error) throw result.error;
if (result.status !== 0) process.exit(result.status || 1);
console.log('Built universal Apple Speech Node-API module.');
