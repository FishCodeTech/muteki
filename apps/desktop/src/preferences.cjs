const fs = require('node:fs');
const path = require('node:path');
const { randomUUID } = require('node:crypto');
const { normalizeOrigin } = require('./policy.cjs');

function readPreferences(file) {
  try {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    const origin = normalizeOrigin(data.origin);
    return { origin, routes: data.routes && typeof data.routes === 'object' ? data.routes : {},
      ...(data.bounds && typeof data.bounds === 'object' ? { bounds: data.bounds } : {}) };
  } catch (error) {
    if (error.code === 'ENOENT') return { origin: '', routes: {} };
    return { origin: '', routes: {}, error: { code: 'desktop.preferences_invalid',
      message: '服务连接偏好无法读取，请重新选择工作台地址。' } };
  }
}

function writePreferences(file, origin, extras = {}) {
  const data = { origin: normalizeOrigin(origin), routes: extras.routes || {}, bounds: extras.bounds };
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

module.exports = { readPreferences, writePreferences };
