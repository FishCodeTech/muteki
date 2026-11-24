import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = fileURLToPath(new URL('../../', import.meta.url));
export const MANIFEST_PATH = path.join(ROOT, 'scripts/verify/manifest.json');
export const RECEIPT_PATH = path.join(ROOT, '.cache/verify-receipt.json');

export function runGit(args, options = {}) {
  const res = spawnSync('git', args, {
    cwd: ROOT,
    encoding: 'utf8',
    ...options,
  });
  return res;
}

export function gitStdout(args) {
  const res = runGit(args);
  if (res.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed: ${(res.stderr || res.stdout || '').trim()}`);
  }
  return (res.stdout || '').trim();
}

export function loadManifestFromDisk() {
  return JSON.parse(fs.readFileSync(MANIFEST_PATH, 'utf8'));
}

export function globToRegExp(pattern) {
  let out = '';
  let i = 0;
  while (i < pattern.length) {
    if (pattern.startsWith('**/', i)) {
      out += '(?:.*/)?';
      i += 3;
      continue;
    }
    if (pattern.startsWith('**', i)) {
      out += '.*';
      i += 2;
      continue;
    }
    const ch = pattern[i];
    if (ch === '*') {
      out += '[^/]*';
    } else if (ch === '?') {
      out += '[^/]';
    } else if ('.+^${}()|[]\\'.includes(ch)) {
      out += `\\${ch}`;
    } else {
      out += ch;
    }
    i += 1;
  }
  return new RegExp(`^${out}$`);
}

export function matchesAny(filePath, patterns) {
  const normalized = filePath.replaceAll('\\', '/');
  return patterns.some((pattern) => globToRegExp(pattern).test(normalized));
}

export function isDocsOnly(files, manifest) {
  if (!files.length) return true;
  return files.every((file) => matchesAny(file, manifest.docs_only_globs || []));
}

export function resolveDiffTracks(files, manifest) {
  const normalized = [...new Set(files.map((f) => f.replaceAll('\\', '/')).filter(Boolean))].sort();
  const docsOnly = isDocsOnly(normalized, manifest);
  const rulesModified = normalized.filter((f) => matchesAny(f, manifest.rules_files || []));

  if (docsOnly) {
    return {
      files: normalized,
      docsOnly: true,
      rulesModified,
      matchedTriggers: [],
      automated: [],
      e2eTracks: [],
      checklists: [],
      runnerScripts: [],
    };
  }

  const matchedTriggers = [];
  const automated = new Set(['lint_rules']);
  const e2eTracks = new Set();
  const checklists = [];

  for (const trigger of manifest.triggers || []) {
    const hits = normalized.filter((f) => matchesAny(f, trigger.patterns || []));
    if (hits.length > 0) {
      matchedTriggers.push({ ...trigger, matchedFiles: hits });
      for (const item of trigger.automated || []) automated.add(item);
      for (const track of trigger.e2e_tracks || []) e2eTracks.add(track);
      for (const line of trigger.checklist || []) {
        checklists.push(`[${trigger.id}] ${line}`);
      }
    }
  }

  if (matchedTriggers.length === 0 && normalized.length > 0) {
    automated.add('frontend_typecheck');
    automated.add('frontend_lint');
    automated.add('contracts_check');
    automated.add('rust_clippy');
    e2eTracks.add('web');
  }

  const runnerScripts = normalized.filter((f) => /^scripts\/verify_.*\.py$|^scripts\/verify_worker_image\.sh$/.test(f));

  return {
    files: normalized,
    docsOnly: false,
    rulesModified,
    matchedTriggers,
    automated: [...automated],
    e2eTracks: [...e2eTracks],
    checklists: [...new Set(checklists)],
    runnerScripts,
  };
}

export function parseStampFromText(text) {
  const raw = String(text || '');
  const stampMatch = raw.match(/<!--\s*muteki-verify-stamp\s+(\{[\s\S]*?\})\s*-->/);
  let stamp = null;
  let parseError = null;
  if (stampMatch) {
    try {
      stamp = JSON.parse(stampMatch[1]);
    } catch (err) {
      parseError = err.message;
    }
  }
  const skipMatches = [...raw.matchAll(/Verify-Skip\s*:\s*(.+)/gi)].map((m) => m[1].trim()).filter(Boolean);
  return {
    stamp,
    parseError,
    skipReason: skipMatches.join('; ') || stamp?.skip_reason || null,
  };
}

export function defaultBaseRef() {
  for (const candidate of ['origin/main', 'main', 'HEAD~1']) {
    const check = runGit(['rev-parse', '--verify', candidate]);
    if (check.status === 0) return candidate;
  }
  return null;
}

export function currentTreeState() {
  const headSha = gitStdout(['rev-parse', 'HEAD']);
  const statusOut = gitStdout(['status', '--porcelain']);
  const dirty = statusOut.length > 0;
  const treeHash = gitStdout(['rev-parse', 'HEAD^{tree}']);
  const indexTree = runGit(['write-tree']).stdout.trim();
  return { headSha, treeHash, indexTree, dirty };
}
