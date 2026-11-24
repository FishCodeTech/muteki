import { test } from 'node:test';
import assert from 'node:assert/strict';
import { authenticate, deviceIdentity } from './auth.mjs';

test('password authentication sets up only unconfigured hosts and stops on setup failure', async () => {
  const device = { device_id: 'browser-id', device_name: 'Browser' };
  for (const configured of [false, true]) {
    const calls = [];
    const api = async (path, body) => {
      calls.push([path, body]);
      return path === '/auth/setup' ? { configured: true } : { token: 'device-token' };
    };
    assert.deepEqual(await authenticate(api, 'secret-pass', configured, device), { token: 'device-token' });
    assert.deepEqual(calls, [
      ...(!configured ? [['/auth/setup', { password: 'secret-pass' }]] : []),
      ['/auth/login', { password: 'secret-pass', device_id: 'browser-id', device_name: 'Browser' }],
    ]);
  }
  const calls = [];
  await assert.rejects(authenticate(async path => {
    calls.push(path);
    throw new Error('409');
  }, 'password', false, device), /409/);
  assert.deepEqual(calls, ['/auth/setup']);
});

test('a browser mints its device id once and reuses it on later logins', () => {
  const stored = new Map();
  const storage = { getItem: key => stored.get(key) ?? null, setItem: (key, value) => stored.set(key, value) };
  const first = deviceIdentity(storage, () => 'minted-id');
  assert.deepEqual(first, { device_id: 'minted-id', device_name: 'Browser' });
  assert.deepEqual(deviceIdentity(storage, () => { throw new Error('must reuse'); }), first);
});
