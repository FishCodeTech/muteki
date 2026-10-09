// Runs from a private copy outside the replaced .app and waits for its owner.
const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const { bundleHash, bundleIdentity, copyBundle, atomicJson, helperRunning } = require('./update-files.cjs');
const dataDirectories = ['state', 'sessions', 'desktop', 'home', 'config'];
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));

async function run(file, recover = false) {
  let journal = JSON.parse(fs.readFileSync(file, 'utf8'));
  if (helperRunning(journal) && journal.helperPid !== process.pid) throw new Error('Another update coordinator is still running');
  const write = patch => { journal = {...journal, ...patch}; atomicJson(file, journal); };
  const transaction = path.dirname(file), root = journal.environmentRoot;
  const previous = journal.current + `.previous-${journal.id}.app`;
  const incoming = journal.current + `.incoming-${journal.id}.app`;
  const backup = path.join(transaction, 'data-backup');
  let switched = fs.existsSync(previous), backedUp = Boolean(journal.backupComplete), next;
  const env = {...process.env};
  for (const key of Object.keys(env)) if (key.startsWith('MUTEKI_') || ['NODE_OPTIONS', 'NODE_PATH', 'ELECTRON_RUN_AS_NODE', 'PYTHONPATH', 'PYTHONHOME', 'VIRTUAL_ENV'].includes(key)) delete env[key];
  write({helperPid: process.pid});
  try {
    if (recover) throw new Error('Recovered an interrupted update before workspace writes were admitted');
    if (!['prepared', 'waiting-for-owner'].includes(journal.phase)) throw new Error('Update transaction is not prepared');
    const identity = bundleIdentity(journal.candidate);
    if (identity.id !== journal.identity.id || identity.team !== journal.identity.team || bundleHash(journal.candidate) !== journal.sha256) throw new Error('Candidate changed after acceptance');
    if (bundleHash(journal.current) !== journal.previousSha256) throw new Error('Installed application changed during update preparation');
    fs.mkdirSync(backup, {mode: 0o700});
    write({phase: 'backing-up'});
    for (const directory of dataDirectories) {
      const source = path.join(root, directory);
      if (fs.existsSync(source)) fs.cpSync(source, path.join(backup, directory), {recursive: true, verbatimSymlinks: true});
    }
    backedUp = true;
    write({phase: 'backed-up', backupComplete: true});
    copyBundle(journal.candidate, incoming);
    if (bundleHash(incoming) !== journal.sha256) throw new Error('Installed copy does not match the accepted candidate');
    write({phase: 'switching', previous});
    fs.renameSync(journal.current, previous);
    try { fs.renameSync(incoming, journal.current); switched = true; }
    catch (error) { fs.renameSync(previous, journal.current); throw error; }
    write({phase: 'validating', previous});
    next = spawn(path.join(journal.current, 'Contents/MacOS', identity.executable), [`--muteki-environment-root=${root}`, `--muteki-update-journal=${file}`], {env, detached: true, stdio: 'ignore'});
    await once(next, 'spawn'); next.unref();
    const deadline = Date.now() + 150000;
    while (Date.now() < deadline) {
      journal = JSON.parse(fs.readFileSync(file, 'utf8'));
      if (journal.phase === 'validated') {
        // Recovery and external side effects may begin after this durable point.
        write({phase: 'writes-open', activatedAt: new Date().toISOString()});
      } else if (journal.phase === 'complete') return;
      else if (journal.phase === 'failed') throw new Error(journal.error || 'New application failed validation');
      if (next.exitCode !== null || next.signalCode !== null) throw new Error('New application stopped during validation');
      await sleep(250);
    }
    throw new Error('New application did not complete startup in time');
  } catch (error) {
    const latest = JSON.parse(fs.readFileSync(file, 'utf8'));
    if (latest.activatedAt) {
      atomicJson(file, {...latest, phase: 'repair-required', error: error.message});
      return; // Never restore an old snapshot after writes were admitted.
    }
    if (next && next.exitCode === null && next.signalCode === null) {
      const exited = once(next, 'exit'); next.kill('SIGTERM');
      await Promise.race([exited, sleep(10000)]);
      if (next.exitCode === null && next.signalCode === null) { next.kill('SIGKILL'); await exited; }
    }
    // The private ownership pipes close with the failed app. Wait until its
    // backend releases its writer lock before touching any database files.
    if (switched) {
      const runtimeRoot = path.join(journal.candidate, 'Contents/Resources/runtime');
      const executable = path.join(runtimeRoot, JSON.parse(fs.readFileSync(path.join(runtimeRoot, 'manifest.json'), 'utf8')).python);
      const check = spawn(executable, ['-I', '-B', '-c', 'import fcntl,sys; f=open(sys.argv[1],"a+b"); fcntl.flock(f,fcntl.LOCK_EX)', path.join(root, 'backend.lock')], {stdio: 'ignore'});
      const timer = setTimeout(() => check.kill('SIGKILL'), 30000);
      const [code] = await once(check, 'exit'); clearTimeout(timer);
      if (code !== 0) { write({phase: 'recovery-required', error: `${error.message}; backend writer lock remains held`}); return; }
      if (fs.existsSync(journal.current)) fs.rmSync(journal.current, {recursive: true});
      fs.renameSync(previous, journal.current);
    }
    if (backedUp) for (const directory of dataDirectories) {
      const live = path.join(root, directory), saved = path.join(backup, directory);
      if (fs.existsSync(live)) fs.rmSync(live, {recursive: true});
      if (fs.existsSync(saved)) fs.cpSync(saved, live, {recursive: true, verbatimSymlinks: true});
    }
    write({phase: 'rolled-back', error: error.message});
    spawn(path.join(journal.current, 'Contents/MacOS', journal.identity.executable), [`--muteki-environment-root=${root}`], {env, detached: true, stdio: 'ignore'}).unref();
  }
}
if (require.main === module) {
  process.stdin.resume();
  process.stdin.once('end', () => run(process.argv[2], process.argv.includes('--recover')).catch(error => { console.error(error); process.exitCode = 1; }));
}
module.exports = {run};
