const fs = require('node:fs');
const path = require('node:path');
const { DEFAULT_ORIGIN, normalizeOrigin } = require('./policy.cjs');

function readPreferences(file) {
  try {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    return { origin: normalizeOrigin(data.origin) };
  } catch {
    return { origin: DEFAULT_ORIGIN };
  }
}

function writePreferences(file, origin) {
  const data = { origin: normalizeOrigin(origin) };
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(`${file}.tmp`, `${JSON.stringify(data, null, 2)}\n`, { mode: 0o600 });
  fs.renameSync(`${file}.tmp`, file);
}

module.exports = { readPreferences, writePreferences };
