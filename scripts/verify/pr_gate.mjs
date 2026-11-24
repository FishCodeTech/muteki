#!/usr/bin/env node
import fs from 'node:fs';
import {
  RECEIPT_PATH,
  ROOT,
  currentTreeState,
  defaultBaseRef,
  gitStdout,
  loadManifestFromDisk,
  parseStampFromText,
  resolveDiffTracks,
  runGit,
} from './common.mjs';

const argv = process.argv.slice(2);
if (argv.includes('--help')) {
  console.log(`Usage: node scripts/verify/pr_gate.mjs (--local-push | --ci) [options]
  --local-push               Verify local receipt before git push (VERIFY_GATE=observe|enforce|off)
  --ci                       Verify PR stamp in GitHub Actions using BASE_SHA manifest (MUTEKI_VERIFY_GATE_MODE=observe|enforce|off)
  --base-sha <sha>           Override BASE_SHA (defaults to env BASE_SHA)
  --head-sha <sha>           Override HEAD_SHA (defaults to env HEAD_SHA)
  --pr-body-file <path>      Read PR body text from file (useful for local testing)`);
  process.exit(0);
}

function getArg(flag) {
  const idx = argv.findIndex((a) => a === flag || a.startsWith(`${flag}=`));
  if (idx < 0) return null;
  if (argv[idx].startsWith(`${flag}=`)) return argv[idx].slice(flag.length + 1);
  return argv[idx + 1] ?? null;
}

function normalizeMode(raw, fallback = 'observe') {
  const val = (raw || fallback).trim().toLowerCase();
  if (['off', 'observe', 'enforce'].includes(val)) return val;
  return fallback;
}

function appendStepSummary(lines) {
  const summaryPath = process.env.GITHUB_STEP_SUMMARY;
  if (!summaryPath) return;
  fs.appendFileSync(summaryPath, `${lines.join('\n')}\n`);
}

function localPushGate() {
  const mode = normalizeMode(process.env.VERIFY_GATE, 'observe');
  if (mode === 'off') {
    console.log('VERIFY_GATE=off — skipping local pre-push receipt check.');
    return 0;
  }

  const manifest = loadManifestFromDisk();
  const baseRef = defaultBaseRef();
  const treeState = currentTreeState();

  const files = new Set();
  if (baseRef) {
    const res = runGit(['diff', '--name-only', '--diff-filter=ACMRTUXB', `${baseRef}...HEAD`]);
    if (res.status === 0) {
      for (const line of (res.stdout || '').split(/\r?\n/)) {
        if (line.trim()) files.add(line.trim());
      }
    }
  }
  const plan = resolveDiffTracks([...files], manifest);
  if (plan.docsOnly) {
    console.log('PR_GATE_OK (local-push, docs-only or empty diff vs base)');
    return 0;
  }

  const headCommitMsg = gitStdout(['log', '-1', '--pretty=%B']);
  const { skipReason } = parseStampFromText(headCommitMsg);

  const issues = [];
  let receipt = null;
  if (!fs.existsSync(RECEIPT_PATH)) {
    issues.push('Missing .cache/verify-receipt.json. Run `node scripts/verify/run.mjs` first.');
  } else {
    try {
      receipt = JSON.parse(fs.readFileSync(RECEIPT_PATH, 'utf8'));
    } catch (err) {
      issues.push(`Unreadable .cache/verify-receipt.json: ${err.message}`);
    }
  }

  if (receipt) {
    if (receipt.head_sha !== treeState.headSha || receipt.tree_hash !== treeState.treeHash) {
      issues.push(
        `Stale receipt: receipt is for ${String(receipt.head_sha || '').slice(0, 12)}, current HEAD is ${treeState.headSha.slice(0, 12)}. Re-run \`node scripts/verify/run.mjs\`.`,
      );
    }
    if (!receipt.automated_ok) {
      issues.push('Last recorded automated verification did not pass.');
    }
    for (const checkId of plan.automated) {
      const status = receipt.automated?.[checkId]?.status;
      if (status !== 'pass') {
        issues.push(`Required automated check "${checkId}" is not recorded as pass (got ${status || 'missing'}).`);
      }
    }
    for (const track of plan.e2eTracks) {
      const entry = receipt.e2e?.[track];
      if (!entry && !skipReason) {
        issues.push(`Triggered E2E track "${track}" has no recorded result in receipt (use \`node scripts/verify/run.mjs --record-e2e "${track}=pass:..." --stamp\`).`);
      }
    }
  }

  if (issues.length === 0) {
    console.log(`PR_GATE_OK (local-push, HEAD=${treeState.headSha.slice(0, 12)})`);
    return 0;
  }

  const header = `[verify-gate:${mode}] Local pre-push check found ${issues.length} issue(s):`;
  console.warn(`\n${header}`);
  for (const issue of issues) console.warn(`  - ${issue}`);
  if (skipReason) console.warn(`  Note: Verify-Skip reason found: ${skipReason}`);

  if (mode === 'enforce' && !skipReason) {
    console.error('\nPush blocked (VERIFY_GATE=enforce). Resolve the issues above or add "Verify-Skip: <reason>" to your commit message.');
    return 1;
  }
  if (mode === 'observe') {
    console.warn('\nContinuing because VERIFY_GATE=observe. Set VERIFY_GATE=enforce to block incomplete pushes.\n');
  }
  return 0;
}

function loadManifestFromBase(baseSha) {
  if (baseSha && !/^0+$/.test(baseSha)) {
    const res = runGit(['show', `${baseSha}:scripts/verify/manifest.json`]);
    if (res.status === 0 && res.stdout.trim()) {
      return { manifest: JSON.parse(res.stdout), source: `base:${baseSha.slice(0, 12)}` };
    }
  }
  return { manifest: loadManifestFromDisk(), source: 'working-tree (bootstrap fallback)' };
}

function readPrBody(prBodyFile) {
  if (prBodyFile) {
    return { isPr: true, body: fs.readFileSync(prBodyFile, 'utf8') };
  }
  const eventPath = process.env.GITHUB_EVENT_PATH;
  const eventName = process.env.GITHUB_EVENT_NAME || '';
  if (eventPath && fs.existsSync(eventPath)) {
    const payload = JSON.parse(fs.readFileSync(eventPath, 'utf8'));
    if (payload.pull_request) {
      return { isPr: true, body: payload.pull_request.body || '' };
    }
    return { isPr: eventName === 'pull_request', body: '' };
  }
  return { isPr: eventName === 'pull_request', body: '' };
}

function ciGate() {
  const mode = normalizeMode(process.env.MUTEKI_VERIFY_GATE_MODE, 'observe');
  const baseSha = getArg('--base-sha') || process.env.BASE_SHA || '';
  const headSha = getArg('--head-sha') || process.env.HEAD_SHA || gitStdout(['rev-parse', 'HEAD']);
  const prBodyFile = getArg('--pr-body-file');

  const { manifest, source } = loadManifestFromBase(baseSha);

  let changedFiles = [];
  if (baseSha && !/^0+$/.test(baseSha)) {
    const diffRes = runGit(['diff', '--name-only', '-z', baseSha, headSha]);
    if (diffRes.status === 0) {
      changedFiles = (diffRes.stdout || '').split('\0').filter(Boolean);
    }
  }
  const plan = resolveDiffTracks(changedFiles, manifest);
  const summary = [
    `### Muteki Pre-PR Verification Gate (\`${mode}\`)`,
    `- **Manifest source**: \`${source}\``,
    `- **Changed files**: ${plan.files.length} | **Docs-only**: ${plan.docsOnly}`,
  ];

  if (plan.rulesModified.length > 0) {
    summary.push(`- **⚠️ Rules / Gate modified (requires careful human review)**: ${plan.rulesModified.map((f) => `\`${f}\``).join(', ')}`);
  }

  if (plan.docsOnly) {
    summary.push('- **Result**: PASS (docs-only change)');
    console.log(summary.join('\n'));
    appendStepSummary(summary);
    return 0;
  }

  const { isPr, body } = readPrBody(prBodyFile);
  if (!isPr) {
    summary.push('- **Result**: INFO (non-PR event; PR stamp check applies on pull_request)');
    console.log(summary.join('\n'));
    appendStepSummary(summary);
    return 0;
  }

  const { stamp, parseError, skipReason } = parseStampFromText(body);
  const issues = [];

  if (parseError) {
    issues.push(`Invalid muteki-verify-stamp JSON in PR body: ${parseError}`);
  } else if (!stamp && !skipReason) {
    issues.push('Missing `<!-- muteki-verify-stamp {...} -->` in PR description. Run `node scripts/verify/run.mjs --stamp` locally and paste it into the PR.');
  }

  if (stamp) {
    if (stamp.head_sha !== headSha) {
      issues.push(`Stamp head_sha (${String(stamp.head_sha || '').slice(0, 12)}) does not match PR HEAD (${headSha.slice(0, 12)}). Re-run verify after your latest commit.`);
    }
    if (stamp.dirty) {
      issues.push('Stamp was generated on a dirty working tree. Re-run `node scripts/verify/run.mjs --stamp` on a clean commit.');
    }
    if (plan.automated.includes('rust_props')) {
      const rustPropStatus = stamp.automated?.rust_props;
      if (rustPropStatus !== 'pass' && !skipReason) {
        issues.push(`Local Rust property suite (rust_props) is not marked pass in stamp (got ${rustPropStatus || 'missing'}).`);
      }
    }
    for (const track of plan.e2eTracks) {
      const rec = stamp.e2e?.[track];
      if (!rec && !skipReason) {
        issues.push(`Triggered E2E track \`${track}\` missing from PR verification stamp.`);
      } else if (rec && rec.status === 'skip' && !rec.summary && !skipReason) {
        issues.push(`Triggered E2E track \`${track}\` was skipped without explanation.`);
      }
    }
  }

  if (issues.length === 0) {
    summary.push(`- **Result**: PASS (HEAD \`${headSha.slice(0, 12)}\` verified${skipReason ? `; Verify-Skip: ${skipReason}` : ''})`);
    console.log(summary.join('\n'));
    appendStepSummary(summary);
    return 0;
  }

  summary.push(`- **Issues (${issues.length})**:`);
  for (const issue of issues) summary.push(`  - ${issue}`);
  if (skipReason) summary.push(`- **Verify-Skip**: ${skipReason}`);

  if (mode === 'enforce' && !(skipReason && !parseError)) {
    summary.push('- **Result**: FAIL (`MUTEKI_VERIFY_GATE_MODE=enforce`)');
    console.error(summary.join('\n'));
    appendStepSummary(summary);
    return 1;
  }

  summary.push(`- **Result**: WARN (\`MUTEKI_VERIFY_GATE_MODE=${mode}\`, non-blocking)`);
  console.warn(summary.join('\n'));
  appendStepSummary(summary);
  return 0;
}

if (argv.includes('--local-push')) {
  process.exit(localPushGate());
} else if (argv.includes('--ci')) {
  process.exit(ciGate());
} else {
  console.error('Specify --local-push or --ci. Use --help for options.');
  process.exit(1);
}
