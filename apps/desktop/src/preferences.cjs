const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { normalizeOrigin } = require('./policy.cjs');

function notificationConsentKey(scope) {
  if (!scope || typeof scope.serviceId !== 'string' || !scope.serviceId
    || typeof scope.identityId !== 'string' || !scope.identityId) return '';
  const workspace = scope.environmentId ? `managed:${scope.environmentId}` : normalizeOrigin(scope.origin);
  return JSON.stringify([workspace, scope.serviceId, scope.identityId]);
}

function notificationConsents(value) {
  if (value === undefined) return undefined;
  if (!value || typeof value !== 'object' || Array.isArray(value)
    || Object.values(value).some(choice => typeof choice !== 'boolean')) {
    throw new Error('Notification consent preferences must contain boolean choices.');
  }
  return { ...value };
}

function readPreferences(file) {
  try {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    const origin = normalizeOrigin(data.origin);
    const consents = notificationConsents(data.notificationConsents);
    return { origin, routes: data.routes && typeof data.routes === 'object' ? data.routes : {},
      ...(data.mode === 'local' || data.mode === 'external' ? { mode: data.mode } : {}),
      ...(data.bounds && typeof data.bounds === 'object' ? { bounds: data.bounds } : {}),
      ...(consents !== undefined ? { notificationConsents: consents } : {}) };
  } catch (error) {
    if (error.code === 'ENOENT') return { origin: '', routes: {} };
    return { origin: '', routes: {}, error: { code: 'desktop.preferences_invalid',
      message: '服务连接偏好无法读取，请重新选择工作台地址。' } };
  }
}

function writePreferences(file, origin, extras = {}) {
  const consents = notificationConsents(extras.notificationConsents);
  const data = { ...(consents !== undefined ? { notificationConsents: consents } : {}), origin: normalizeOrigin(origin), routes: extras.routes || {}, bounds: extras.bounds,
    ...(extras.mode === 'local' || extras.mode === 'external' ? { mode: extras.mode } : {}) };
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temporary = `${file}.${randomUUID()}.tmp`;
  try {
    if (fs.existsSync(file) && readPreferences(file).error) {
      fs.copyFileSync(file, `${file}.invalid-${randomUUID()}.bak`, fs.constants.COPYFILE_EXCL);
    }
    fs.writeFileSync(temporary, `${JSON.stringify(data, null, 2)}\n`, { mode: 0o600 });
    fs.renameSync(temporary, file);
  } finally { if (fs.existsSync(temporary)) fs.unlinkSync(temporary); }
}

module.exports = { readPreferences, writePreferences, notificationConsentKey };
