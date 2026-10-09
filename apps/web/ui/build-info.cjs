// One source fingerprint for the Web build and the locally bundled desktop UI.
const fs = require('node:fs');
const path = require('node:path');
const { createHash } = require('node:crypto');
function uiBuildId() {
  const hash = createHash('sha256');
  function visit(directory) {
    for (const item of fs.readdirSync(directory, {withFileTypes: true}).sort((a, b) => a.name.localeCompare(b.name))) {
      const filename = path.join(directory, item.name);
      if (item.isDirectory()) visit(filename);
      else if (/\.(tsx?|css|json)$/.test(item.name)) { hash.update(path.relative(__dirname, filename)); hash.update(fs.readFileSync(filename)); }
    }
  }
  for (const directory of ['app', 'components', 'lib', 'styles']) visit(path.join(__dirname, directory));
  return `${require('./package.json').version}+${hash.digest('hex').slice(0, 12)}`;
}
module.exports = { uiBuildId };
