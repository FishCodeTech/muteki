#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { createServer } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import {
  MANIFEST_PATH,
  RECEIPT_PATH,
  ROOT,
  currentTreeState,
  defaultBaseRef,
  gitStdout,
  loadManifestFromDisk,
  resolveDiffTracks,
  runGit,
} from './common.mjs';

const argv = process.argv.slice(2);
if (argv.includes('--help')) {
  console.log(`Usage: node scripts/verify/run.mjs [options]
  --doctor                             Inspect .state symlink, ports 8100/3101/3103, kimi-webbridge, cua-driver, and safe isolated commands
  --base <ref>                         Diff base ref (default: auto-detect origin/main | main | HEAD~1)
  --scope <staged|worktree|branch>     Diff scope (default: branch = base...HEAD + worktree + untracked)
  --staged-only                        Alias for --scope staged (used by pre-commit)
  --skip-rust-props                    Skip muteki-verify-props execution
  --record-e2e <track>=<status>:<msg>  Record web|desktop|runner|model_runtime as pass:<msg> or skip:<msg>
  --stamp                              Print Markdown Verification Stamp for PR body`);
  process.exit(0);
}

function getArg(flag) {
  const idx = argv.findIndex((a) => a === flag || a.startsWith(`${flag}=`));
  if (idx < 0) return null;
  if (argv[idx].startsWith(`${flag}=`)) return argv[idx].slice(flag.length + 1);
  return argv[idx + 1] ?? null;
}

function getMultiArg(flag) {
  const values = [];
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === flag && argv[i + 1]) {
      values.push(argv[i + 1]);
      i += 1;
    } else if (argv[i].startsWith(`${flag}=`)) {
      values.push(argv[i].slice(flag.length + 1));
    }
  }
  return values;
}

const isDoctor = argv.includes('--doctor');
const stagedOnly = argv.includes('--staged-only');
const scope = stagedOnly ? 'staged' : (getArg('--scope') || 'branch');
const baseRef = getArg('--base') || defaultBaseRef();
const skipRustProps = argv.includes('--skip-rust-props');
const wantStamp = argv.includes('--stamp');
const e2eArgs = getMultiArg('--record-e2e');

if (!['staged', 'worktree', 'branch'].includes(scope)) {
  throw new Error(`Invalid --scope "${scope}". Expected staged, worktree, or branch.`);
}

async function freePort() {
  const srv = createServer();
  srv.listen(0, '127.0.0.1');
  await new Promise((resolve, reject) => {
    srv.once('listening', resolve);
    srv.once('error', reject);
  });
  const port = srv.address().port;
  await new Promise((resolve) => srv.close(resolve));
  return port;
}

function inspectCmd(command, args) {
  return spawnSync(command, args, { cwd: ROOT, encoding: 'utf8' });
}

async function runDoctor() {
  const state = currentTreeState();
  const branch = runGit(['branch', '--show-current']).stdout.trim() || '(detached)';
  console.log('=== Muteki Verification Doctor ===');
  console.log(`Repo root:    ${ROOT}`);
  console.log(`Branch/HEAD:  ${branch} @ ${state.headSha.slice(0, 12)} (${state.dirty ? 'dirty' : 'clean'})`);

  const statePath = path.join(ROOT, '.state');
  if (fs.existsSync(statePath)) {
    const lst = fs.lstatSync(statePath);
    const real = fs.realpathSync(statePath);
    console.log(`.state:       ${lst.isSymbolicLink() ? `SYMLINK -> ${real}` : `directory (${real})`}`);
  } else {
    console.log('.state:       not present yet');
  }

  for (const port of [8100, 3101, 3103]) {
    const lsof = inspectCmd('/usr/sbin/lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-t']);
    const pids = (lsof.stdout || '').trim().split(/\s+/).filter(Boolean);
    if (!pids.length) {
      console.log(`Port ${port}:    free`);
    } else {
      const ps = inspectCmd('/bin/ps', ['-p', pids.join(','), '-o', 'pid=,ppid=,comm=']);
      const cwdOut = inspectCmd('/usr/sbin/lsof', ['-a', '-d', 'cwd', '-p', pids.join(','), '-Fn']);
      console.log(`Port ${port}:    IN USE by ${ps.stdout.trim().replace(/\s+/g, ' ')}`);
      const cwdLines = (cwdOut.stdout || '').split('\n').filter((l) => l.startsWith('n')).map((l) => l.slice(1));
      if (cwdLines.length) console.log(`              cwd: ${cwdLines.join(', ')}`);
    }
  }

  const psAll = inspectCmd('/bin/ps', ['-axo', 'pid=,comm=']);
  const installedRunning = (psAll.stdout || '')
    .split('\n')
    .filter((l) => l.includes('/Applications/Muteki.app/Contents/'));
  console.log(`InstalledApp: ${installedRunning.length ? `RUNNING (${installedRunning.length} process(es) — DO NOT terminate)` : 'not running'}`);

  const serverBin = path.join(ROOT, '.cache/target/local/muteki-server');
  const desktopBin = path.join(ROOT, '.cache/target/local/muteki-desktop');
  const nextBuildId = path.join(ROOT, 'apps/frontend/.next/BUILD_ID');
  console.log(`Local bins:   server=${fs.existsSync(serverBin) ? 'yes' : 'missing'}, desktop=${fs.existsSync(desktopBin) ? 'yes' : 'missing'}, next=${fs.existsSync(nextBuildId) ? 'yes' : 'missing'}`);

  const wbBin = path.join(os.homedir(), '.kimi-webbridge/bin/kimi-webbridge');
  if (fs.existsSync(wbBin)) {
    const wb = inspectCmd(wbBin, ['status']);
    const summary = `${wb.stdout || ''} ${wb.stderr || ''}`.trim().replace(/\s+/g, ' ');
    console.log(`kimi-webbridge: ${summary || `exit ${wb.status}`}`);
  } else {
    console.log('kimi-webbridge: binary missing at ~/.kimi-webbridge/bin/kimi-webbridge');
  }

  const cua = inspectCmd('cua-driver', ['--version']);
  console.log(`cua-driver:   ${cua.status === 0 ? cua.stdout.trim() : 'missing or not on PATH'}`);

  const apiPort = await freePort();
  let uiPort = await freePort();
  while (uiPort === apiPort) uiPort = await freePort();
  console.log('\nSafe isolated E2E launch coordinates (never touches .state or /Applications/Muteki.app):');
  console.log(`  MUTEKI_STATE_DIR=/tmp/muteki-verify-${Date.now()}`);
  console.log(`  MUTEKI_API_PORT=${apiPort} MUTEKI_BACKEND_PORT=${apiPort} MUTEKI_UI_PORT=${uiPort} MUTEKI_UI_ORIGIN=http://127.0.0.1:${uiPort}`);
}

function collectChangedFiles() {
  const files = new Set();
  const addLines = (out) => {
    for (const line of (out || '').split(/\r?\n/)) {
      const trimmed = line.trim();
      if (trimmed) files.add(trimmed);
    }
  };
  if (scope === 'staged') {
    addLines(gitStdout(['diff', '--cached', '--name-only', '--diff-filter=ACMRTUXB']));
    return [...files].sort();
  }
  if (scope === 'branch' && baseRef) {
    const res = runGit(['diff', '--name-only', '--diff-filter=ACMRTUXB', `${baseRef}...HEAD`]);
    if (res.status === 0) addLines(res.stdout);
  }
  addLines(gitStdout(['diff', '--cached', '--name-only', '--diff-filter=ACMRTUXB']));
  addLines(gitStdout(['diff', '--name-only', '--diff-filter=ACMRTUXB']));
  addLines(gitStdout(['ls-files', '--others', '--exclude-standard']));
  return [...files].sort();
}

function runCheck(id, label, command, args) {
  console.log(`\n▶ [${id}] ${label}`);
  const started = Date.now();
  const res = spawnSync(command, args, { cwd: ROOT, stdio: 'inherit', env: process.env });
  const durationMs = Date.now() - started;
  if (res.status !== 0) {
    console.error(`✖ [${id}] FAILED in ${(durationMs / 1000).toFixed(1)}s (exit ${res.status ?? 1})`);
    return { id, status: 'fail', durationMs, command: `${command} ${args.join(' ')}` };
  }
  console.log(`✔ [${id}] PASSED in ${(durationMs / 1000).toFixed(1)}s`);
  return { id, status: 'pass', durationMs, command: `${command} ${args.join(' ')}` };
}

function parseE2eRecords(items) {
  const out = {};
  for (const raw of items) {
    const eq = raw.indexOf('=');
    const colon = raw.indexOf(':', eq + 1);
    if (eq < 1 || colon < eq + 2) {
      throw new Error(`Invalid --record-e2e "${raw}". Expected "<track>=<pass|skip>:<summary>".`);
    }
    const track = raw.slice(0, eq).trim();
    const status = raw.slice(eq + 1, colon).trim();
    const summary = raw.slice(colon + 1).trim();
    if (!['web', 'desktop', 'runner', 'model_runtime'].includes(track)) {
      throw new Error(`Unknown E2E track "${track}". Expected web, desktop, runner, or model_runtime.`);
    }
    if (!['pass', 'skip'].includes(status)) {
      throw new Error(`Invalid E2E status "${status}". Expected pass or skip.`);
    }
    const driver = track === 'web' ? 'kimi-webbridge' : track === 'desktop' ? 'cua-driver' : track;
    out[track] = { status, driver, summary, recorded_at: new Date().toISOString() };
  }
  return out;
}

function loadExistingReceipt() {
  if (!fs.existsSync(RECEIPT_PATH)) return null;
  try {
    return JSON.parse(fs.readFileSync(RECEIPT_PATH, 'utf8'));
  } catch {
    return null;
  }
}

function saveReceipt(receipt) {
  fs.mkdirSync(path.dirname(RECEIPT_PATH), { recursive: true });
  fs.writeFileSync(RECEIPT_PATH, `${JSON.stringify(receipt, null, 2)}\n`);
}

function formatStamp(receipt) {
  const payload = {
    version: 1,
    head_sha: receipt.head_sha,
    tree_hash: receipt.tree_hash,
    dirty: receipt.dirty,
    docs_only: receipt.docs_only,
    automated: Object.fromEntries(Object.entries(receipt.automated || {}).map(([k, v]) => [k, v.status])),
    e2e: receipt.e2e || {},
    required_e2e: receipt.required_e2e || [],
    rules_modified: receipt.rules_modified || [],
    generated_at: receipt.generated_at,
  };
  const lines = [
    '## Verification Stamp',
    '',
    `- **Commit**: \`${receipt.head_sha}\`${receipt.dirty ? ' (⚠️ dirty worktree when generated — re-run after final commit)' : ''}`,
    `- **Changed files**: ${receipt.changed_files.length}`,
    `- **Docs only**: ${receipt.docs_only ? 'yes' : 'no'}`,
    `- **Automated checks**: ${Object.entries(payload.automated).map(([k, s]) => `\`${k}\`=${s}`).join(', ') || 'none (docs-only)'}`,
  ];
  if (receipt.required_e2e.length) {
    lines.push(`- **Triggered E2E tracks**: ${receipt.required_e2e.map((t) => `\`${t}\``).join(', ')}`);
  }
  for (const [track, info] of Object.entries(receipt.e2e || {})) {
    lines.push(`  - \`${track}\` (${info.driver}): **${info.status}** — ${info.summary}`);
  }
  if (receipt.rules_modified?.length) {
    lines.push(`- **⚠️ Verification rules modified**: ${receipt.rules_modified.map((f) => `\`${f}\``).join(', ')}`);
  }
  lines.push('', `<!-- muteki-verify-stamp ${JSON.stringify(payload)} -->`);
  return lines.join('\n');
}

async function main() {
  if (isDoctor) {
    await runDoctor();
    if (!wantStamp && e2eArgs.length === 0 && !stagedOnly) return;
  }

  const manifest = loadManifestFromDisk();
  const treeState = currentTreeState();
  const changedFiles = collectChangedFiles();
  const plan = resolveDiffTracks(changedFiles, manifest);
  const recordedE2e = parseE2eRecords(e2eArgs);
  const prevReceipt = loadExistingReceipt();

  console.log(`\n=== Muteki Verify Plan (scope=${scope}, base=${baseRef || 'none'}) ===`);
  console.log(`HEAD: ${treeState.headSha.slice(0, 12)} | Changed files: ${changedFiles.length} | Docs-only: ${plan.docsOnly}`);
  if (plan.matchedTriggers.length) {
    console.log(`Matched triggers: ${plan.matchedTriggers.map((t) => t.id).join(', ')}`);
  }

  const commentArgs = scope === 'staged' ? ['--staged'] : [];
  if (scope === 'branch' && baseRef) {
    commentArgs.push('--base', gitStdout(['merge-base', baseRef, 'HEAD']));
  }
  const checkCommands = {
    lint_rules: ['Custom Lint Rules', 'node', ['scripts/lint_rules.mjs', ...commentArgs]],
    frontend_typecheck: ['Frontend TypeScript', 'npm', ['--prefix', 'apps/frontend', 'run', 'typecheck']],
    frontend_lint: ['Frontend ESLint', 'npm', ['--prefix', 'apps/frontend', 'run', 'lint']],
    contracts_check: ['OpenAPI Contracts Check', 'python3', ['scripts/generate_frontend_contracts.py', '--check']],
    frontend_props: ['Frontend fast-check Properties', 'npm', ['--prefix', 'apps/frontend', 'run', 'props']],
    rust_clippy: [
      'Rust Clippy (disallowed_methods)',
      'node',
      ['scripts/cargo.mjs', 'clippy', '--locked', '-p', 'muteki-server', '--', '-A', 'clippy::all', '-D', 'clippy::disallowed_methods'],
    ],
    rust_props: [
      'Rust proptest Properties',
      'node',
      ['scripts/cargo.mjs', 'run', '--locked', '--profile', 'local', '-p', 'muteki-server', '--features', 'verify-props', '--bin', 'muteki-verify-props'],
    ],
  };

  const canReuseAutomated =
    e2eArgs.length > 0 &&
    !stagedOnly &&
    prevReceipt &&
    prevReceipt.head_sha === treeState.headSha &&
    prevReceipt.tree_hash === treeState.treeHash &&
    prevReceipt.index_tree === treeState.indexTree &&
    prevReceipt.dirty === treeState.dirty &&
    prevReceipt.automated_ok === true &&
    plan.automated.every((id) => prevReceipt.automated?.[id]?.status === 'pass');

  const automatedResults = canReuseAutomated ? { ...prevReceipt.automated } : {};
  let failed = false;

  if (!canReuseAutomated && !plan.docsOnly) {
    for (const id of plan.automated) {
      if (id === 'rust_props' && skipRustProps) {
        automatedResults[id] = { id, status: 'skip', durationMs: 0, command: 'skipped via --skip-rust-props' };
        continue;
      }
      const spec = checkCommands[id];
      if (!spec) continue;
      const result = runCheck(id, spec[0], spec[1], spec[2]);
      automatedResults[id] = result;
      if (result.status !== 'pass') {
        failed = true;
        break;
      }
    }
  } else if (canReuseAutomated) {
    console.log('Reusing verified automated checks from current tree receipt.');
  }

  const mergedE2e = {
    ...(prevReceipt && prevReceipt.head_sha === treeState.headSha ? prevReceipt.e2e : {}),
    ...recordedE2e,
  };

  const receipt = {
    version: 1,
    scope,
    base_ref: baseRef,
    head_sha: treeState.headSha,
    tree_hash: treeState.treeHash,
    index_tree: treeState.indexTree,
    dirty: treeState.dirty,
    docs_only: plan.docsOnly,
    changed_files: changedFiles,
    matched_triggers: plan.matchedTriggers.map((t) => t.id),
    rules_modified: plan.rulesModified,
    automated_ok: !failed,
    automated: automatedResults,
    required_e2e: plan.e2eTracks,
    e2e: mergedE2e,
    runner_scripts: plan.runnerScripts,
    generated_at: new Date().toISOString(),
  };

  if (!failed) {
    saveReceipt(receipt);
  }

  if (plan.e2eTracks.length && !failed) {
    console.log('\n=== Required Pre-PR E2E Tracks ===');
    for (const track of plan.e2eTracks) {
      const driver = track === 'web' ? 'kimi-webbridge (Playwright forbidden)' : track === 'desktop' ? 'cua-driver' : track;
      const recorded = mergedE2e[track];
      console.log(`- ${track} [${driver}]: ${recorded ? `${recorded.status} (${recorded.summary})` : 'PENDING'}`);
    }
    if (plan.checklists.length) {
      console.log('\nChecklist:');
      for (const item of plan.checklists) console.log(`  * ${item}`);
    }
  }

  if (wantStamp && !failed) {
    console.log('\n' + formatStamp(receipt));
  }

  if (failed) {
    process.exit(1);
  }
}

await main();
