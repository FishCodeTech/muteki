// The Dev executable keeps its signed identity while application code comes
// from an explicitly configured checkout. Stable builds never use this entry.
const fs = require('node:fs');
const path = require('node:path');
const metadata = require('../package.json');
if (metadata.mutekiChannel !== 'dev') throw new Error('Invalid development application');
const workspace = fs.realpathSync(process.env.MUTEKI_DESKTOP_WORKSPACE || metadata.mutekiWorkspace);
const entry = path.join(workspace, 'apps/desktop/src/main.cjs');
if (!fs.existsSync(entry)) throw new Error('Development workspace does not contain Muteki; select it with the Dev launcher');
process.env.MUTEKI_DESKTOP_WORKSPACE = workspace;
process.env.MUTEKI_DESKTOP_NODE ||= path.join(process.resourcesPath, 'runtime', process.platform === 'win32' ? 'node.exe' : 'node');
require(entry);
