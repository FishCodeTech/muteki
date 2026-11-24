const { execFileSync } = require('node:child_process');
const { version } = require('./package.json');
function git(...args) {
  try {
    return execFileSync('git', args, { cwd: __dirname, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  } catch {
    return '';
  }
}
function uiBuildId() {
  const revision = process.env.MUTEKI_RELEASE_GIT_REF
    || git('describe', '--tags', '--exact-match')
    || git('branch', '--show-current')
    || 'working-tree';
  const dirty = git('status', '--porcelain', '--untracked-files=normal') ? '-dirty' : '';
  return `${version}+${revision}${dirty}`;
}
module.exports = { uiBuildId };
