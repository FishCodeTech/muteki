// Installs the agent CLIs TCODE_AGENTS selects into the data volume: each
// one's current release, the first time it is selected. From then on it is
// reused offline and updated by its own updater or Tcode's provider update.
import { spawnSync } from 'node:child_process';
import { constants as fsConstants } from 'node:fs';
import { access, mkdir, readFile, rm, symlink } from 'node:fs/promises';
import path from 'node:path';

const agents = JSON.parse(await readFile(new URL('./agents.json', import.meta.url)));
const selected = [...new Set((process.env.TCODE_AGENTS ?? Object.keys(agents).join(','))
  .split(',').map(name => name.trim()).filter(Boolean))];

for (const name of selected) {
  if (!Object.hasOwn(agents, name)) {
    throw new Error(`Unknown TCODE_AGENTS entry ${JSON.stringify(name)}; choose ${Object.keys(agents).join(',')}`);
  }
}

// npm packages go to the image's global npm prefix, which is on PATH: a CLI
// or Tcode updating it with `npm install --global` updates the copy that
// runs, and an agent's own global installs run and persist beside it.
const prefix = process.env.NPM_CONFIG_PREFIX;
const home = process.env.HOME;
const [links] = process.argv.slice(2);
if (!links || !path.isAbsolute(links) || !prefix || !path.isAbsolute(prefix) || !home) {
  throw new Error('Usage: HOME=... NPM_CONFIG_PREFIX=... install-agents.mjs ABSOLUTE_LINK_DIR');
}

function run(program, args) {
  const result = spawnSync(program, args, { stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${program} exited with ${result.status ?? result.signal}`);
}

async function executable(file) {
  try {
    await access(file, fsConstants.X_OK);
    return true;
  } catch (error) {
    if (error.code === 'ENOENT' || error.code === 'EACCES') return false;
    throw error;
  }
}

async function exists(file) {
  try {
    await access(file);
    return true;
  } catch {
    return false;
  }
}

await mkdir(links, { recursive: true });
// The link directory leads PATH and holds only links this script makes.
for (const { commands } of Object.values(agents)) {
  for (const command of commands) await rm(path.join(links, command), { force: true });
}

const available = [];
for (const [name, spec] of Object.entries(agents)) {
  const wanted = selected.includes(name);
  try {
    if (spec.package) {
      const installed = await exists(path.join(prefix, 'lib', 'node_modules', spec.package));
      if (!wanted) {
        // Off PATH means uninstalled: the prefix's bin is on PATH. Logins
        // and configuration stay under HOME for a later reinstall.
        if (installed) run('npm', ['uninstall', '--global', '--no-audit', '--no-fund', spec.package]);
        continue;
      }
      console.log(`Preparing ${name}`);
      if (!await executable(path.join(prefix, 'bin', spec.commands[0]))) {
        run('npm', ['install', '--global', '--no-audit', '--no-fund',
          `--allow-scripts=${spec.package}`, '--', `${spec.package}@latest`]);
      }
      // A binary the CLI's own updater moves forward outside the package
      // (Grok's ~/.grok/bin/grok) runs instead of the package's copy.
      if (spec.current && await executable(path.join(home, spec.current))) {
        for (const command of spec.commands) await symlink(path.join(home, spec.current), path.join(links, command));
      }
    } else {
      if (!wanted) continue;
      console.log(`Preparing ${name}`);
      // Cursor's own installer, whose updater keeps HOME's copy current.
      if (!await executable(path.join(home, '.local', 'bin', spec.commands[0]))) {
        run('bash', ['-c', `set -o pipefail; curl --fail --silent --show-error --location '${spec.installer}' | bash`]);
      }
      for (const command of spec.commands) {
        await symlink(path.join(home, '.local', 'bin', command), path.join(links, command));
      }
    }
    if (wanted) available.push(name);
  } catch (error) {
    // The machine starts without it, so it stays reachable; the next start
    // tries again.
    console.error(`Could not ${wanted ? 'install' : 'remove'} ${name}: ${error.message}`);
  }
}
console.log(`Available agents: ${available.join(',') || '(none)'}`);
