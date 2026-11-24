#!/usr/bin/env node
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const violations = [];

function addViolation(rule, file, line, detail, fix) {
  violations.push({
    rule,
    file: path.relative(root, file),
    line,
    detail,
    fix,
  });
}

function walkFiles(dir, exts, skipDirs = new Set(['node_modules', '.next', '.next-dev', '.next-package', 'target', '.cache', '.git', 'dist', 'output', 'vendor'])) {
  const out = [];
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.isSymbolicLink()) continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (skipDirs.has(entry.name) || entry.name.startsWith('.next-')) continue;
      out.push(...walkFiles(full, exts, skipDirs));
    } else if (entry.isFile()) {
      if (!exts || exts.some((ext) => entry.name.endsWith(ext))) {
        out.push(full);
      }
    }
  }
  return out;
}

function checkNoPlaywright() {
  const pkgPath = path.join(root, 'apps/frontend/package.json');
  if (fs.existsSync(pkgPath)) {
    const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
    for (const section of ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']) {
      const deps = pkg[section] || {};
      for (const name of Object.keys(deps)) {
        if (name === 'playwright' || name.startsWith('@playwright/') || name.startsWith('playwright-')) {
          addViolation(
            'no-playwright',
            pkgPath,
            1,
            `Forbidden Playwright dependency "${name}" in ${section}.`,
            'Remove Playwright dependency. Web E2E must use kimi-webbridge and Desktop E2E must use cua-driver (see .agents/skills/verify/SKILL.md).',
          );
        }
      }
    }
  }

  const codeFiles = [
    ...walkFiles(path.join(root, 'apps/frontend/app'), ['.ts', '.tsx', '.js', '.mjs', '.cjs']),
    ...walkFiles(path.join(root, 'apps/frontend/components'), ['.ts', '.tsx', '.js', '.mjs', '.cjs']),
    ...walkFiles(path.join(root, 'apps/frontend/lib'), ['.ts', '.tsx', '.js', '.mjs', '.cjs']),
    ...walkFiles(path.join(root, 'packages/muteki-ui/src'), ['.ts', '.tsx', '.js', '.mjs', '.cjs']),
    ...walkFiles(path.join(root, 'scripts'), ['.js', '.mjs', '.cjs', '.ts', '.py', '.sh']),
    ...walkFiles(path.join(root, 'crates/server/src'), ['.rs']),
    ...walkFiles(path.join(root, 'crates/desktop/src'), ['.rs']),
  ];

  const importOrCallRe =
    /(?:\bfrom\s+['"]@?playwright(?:\/[^'"]*)?['"]|\brequire\s*\(\s*['"]@?playwright(?:\/[^'"]*)?['"]\s*\)|\bimport\s*\(\s*['"]@?playwright(?:\/[^'"]*)?['"]\s*\)|npx\s+playwright\b|playwright-cli\b|from\s+playwright\.|import\s+playwright\b)/;

  for (const file of codeFiles) {
    const rel = path.relative(root, file);
    if (rel === 'scripts/lint_rules.mjs') continue;
    if (rel === 'scripts/verify_worker_image.sh') continue;
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
    lines.forEach((line, idx) => {
      if (importOrCallRe.test(line)) {
        addViolation(
          'no-playwright',
          file,
          idx + 1,
          `Forbidden Playwright usage: ${line.trim()}`,
          'Use kimi-webbridge (http://127.0.0.1:10086) for Web E2E or cua-driver for Desktop E2E.',
        );
      }
    });
  }
}

function checkVerifyAntiCheating() {
  const skipFiles = [
    path.join(root, 'crates/server/src/bin/verify_props.rs'),
    ...walkFiles(path.join(root, 'scripts/verify'), ['.js', '.mjs', '.cjs', '.py', '.sh']),
    ...walkFiles(path.join(root, 'scripts'), ['.py']).filter((f) => path.basename(f).startsWith('verify_')),
  ].filter((f) => fs.existsSync(f));

  const sleepFiles = new Set([
    path.join(root, 'crates/server/src/bin/verify_props.rs'),
    ...walkFiles(path.join(root, 'scripts/verify'), ['.js', '.mjs', '.cjs', '.py', '.sh']),
  ]);

  const skipRe = /(?:#\[ignore\b|\.(?:skip|only)\s*\(|@unittest\.skip|pytest\.mark\.skip)/;
  const longSleepMsRe = /(?:setTimeout|delay)\s*\(\s*(\d[\d_]*)\s*\)/;
  const longSleepSecRe = /(?:time\.sleep|std::thread::sleep.*from_secs|tokio::time::sleep.*from_secs)\s*\(\s*(\d+)\s*\)/;

  for (const file of skipFiles) {
    const checkSleep = sleepFiles.has(file);
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
    lines.forEach((line, idx) => {
      if (!line.includes('verify-allow-skip:') && skipRe.test(line)) {
        addViolation(
          'no-skipped-or-only-verify',
          file,
          idx + 1,
          `Skipped/focused check marker found: ${line.trim()}`,
          'Fix the root cause or remove the broken check with explanation; never leave #[ignore], .skip(), or .only() in verification suites.',
        );
      }
      if (checkSleep && !line.includes('verify-allow-sleep:')) {
        const msMatch = line.match(longSleepMsRe);
        if (msMatch && Number(msMatch[1].replaceAll('_', '')) >= 5000) {
          addViolation(
            'no-long-bare-sleep-in-verify',
            file,
            idx + 1,
            `Hardcoded sleep >= 5000ms found: ${line.trim()}`,
            'Poll an explicit readiness/state predicate with bounded timeout instead of sleeping >= 5s (or annotate with verify-allow-sleep: <reason>).',
          );
        }
        const secMatch = line.match(longSleepSecRe);
        if (secMatch && Number(secMatch[1]) >= 5) {
          addViolation(
            'no-long-bare-sleep-in-verify',
            file,
            idx + 1,
            `Hardcoded sleep >= 5s found: ${line.trim()}`,
            'Poll an explicit readiness/state predicate with bounded timeout instead of sleeping >= 5s (or annotate with verify-allow-sleep: <reason>).',
          );
        }
      }
    });
  }
}

function checkManifestAndSkill() {
  const manifestPath = path.join(root, 'scripts/verify/manifest.json');
  const skillPath = path.join(root, '.agents/skills/verify/SKILL.md');

  if (!fs.existsSync(manifestPath)) {
    addViolation(
      'verify-manifest-valid',
      manifestPath,
      1,
      'Missing scripts/verify/manifest.json.',
      'Restore scripts/verify/manifest.json on the repository.',
    );
  } else {
    try {
      const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
      if (!Array.isArray(manifest.triggers) || manifest.triggers.length === 0) {
        addViolation('verify-manifest-valid', manifestPath, 1, 'manifest.triggers must be a non-empty array.', 'Define diff-triggered verification rules in scripts/verify/manifest.json.');
      }
      if (!Array.isArray(manifest.docs_only_globs) || !Array.isArray(manifest.rules_files)) {
        addViolation('verify-manifest-valid', manifestPath, 1, 'manifest must define docs_only_globs and rules_files arrays.', 'Update scripts/verify/manifest.json schema.');
      }
      for (const trigger of manifest.triggers || []) {
        if (!trigger.id || !Array.isArray(trigger.patterns) || !Array.isArray(trigger.automated) || !Array.isArray(trigger.e2e_tracks)) {
          addViolation('verify-manifest-valid', manifestPath, 1, `Invalid trigger entry: ${JSON.stringify(trigger.id || trigger)}`, 'Each trigger requires id, patterns, automated, and e2e_tracks.');
        }
      }
    } catch (err) {
      addViolation('verify-manifest-valid', manifestPath, 1, `Invalid JSON in manifest.json: ${err.message}`, 'Fix JSON syntax in scripts/verify/manifest.json.');
    }
  }

  if (!fs.existsSync(skillPath)) {
    addViolation(
      'verify-manifest-valid',
      skillPath,
      1,
      'Missing project-local skill .agents/skills/verify/SKILL.md.',
      'Create .agents/skills/verify/SKILL.md inside the repository.',
    );
  } else {
    const raw = fs.readFileSync(skillPath, 'utf8');
    if (!raw.startsWith('---\n') || !raw.includes('\nname: verify\n')) {
      addViolation(
        'verify-manifest-valid',
        skillPath,
        1,
        '.agents/skills/verify/SKILL.md must start with YAML frontmatter containing "name: verify".',
        'Add valid YAML frontmatter with name: verify and description.',
      );
    }
  }

  const forbiddenLegacy = /\b(pack:dev|apps\/web\/ui)\b/;
  const docAndVerifyFiles = [
    skillPath,
    ...walkFiles(path.join(root, 'scripts/verify'), ['.js', '.mjs', '.json', '.md']),
  ].filter((f) => fs.existsSync(f));

  for (const file of docAndVerifyFiles) {
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
    lines.forEach((line, idx) => {
      if (forbiddenLegacy.test(line)) {
        addViolation(
          'no-legacy-electron-commands',
          file,
          idx + 1,
          `Legacy Electron/Python reference found: ${line.trim()}`,
          'Use current Rust/Next commands from docs/development.md (node scripts/dev.mjs, install-macos.mjs, package-web.mjs).',
        );
      }
    });
  }
}

function checkNoHeroUiOrLegacyUi() {
  const bannedPkgs = [
    'react-aria',
    'react-aria-components',
    'tailwind-variants',
    'tw-animate-css',
    'input-otp',
  ];
  const bannedPrefixes = [
    '@heroui/',
    '@react-aria/',
    '@react-stately/',
    '@react-types/',
    '@internationalized/',
    '@radix-ui/',
  ];
  for (const relPkg of ['apps/frontend/package.json', 'packages/muteki-ui/package.json']) {
    const pkgPath = path.join(root, relPkg);
    if (!fs.existsSync(pkgPath)) continue;
    const pkg = JSON.parse(fs.readFileSync(pkgPath, 'utf8'));
    for (const section of ['dependencies', 'devDependencies', 'optionalDependencies', 'peerDependencies']) {
      const deps = pkg[section] || {};
      for (const name of Object.keys(deps)) {
        if (bannedPkgs.includes(name) || bannedPrefixes.some((p) => name.startsWith(p))) {
          addViolation(
            'no-heroui-or-legacy-ui',
            pkgPath,
            1,
            `Forbidden legacy UI dependency "${name}" in ${section}.`,
            'Remove legacy HeroUI/React Aria/Radix dependency; use @muteki/ui built on @ui-sdk/*',
          );
        }
      }
    }
  }

  const mutekiUiFiles = walkFiles(path.join(root, 'packages/muteki-ui/src'), ['.ts', '.tsx']);
  const appCouplingRe = /(?:from\s+['"](?:@\/|apps\/frontend)|import\s*\(\s*['"](?:@\/|apps\/frontend))/;
  for (const file of mutekiUiFiles) {
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
    lines.forEach((line, idx) => {
      if (appCouplingRe.test(line)) {
        addViolation(
          'no-heroui-or-legacy-ui',
          file,
          idx + 1,
          `@muteki/ui must stay app-agnostic; found app-coupled import: ${line.trim()}`,
          'Move app-specific helpers/adapters to apps/frontend and keep packages/muteki-ui pure.',
        );
      }
    });
  }
}

function splitTopLevel(value) {
  const out = [];
  let depth = 0;
  let cur = '';
  for (const ch of value) {
    if (ch === '(') depth += 1;
    if (ch === ')') depth -= 1;
    if (ch === ',' && depth === 0) {
      out.push(cur.trim());
      cur = '';
    } else {
      cur += ch;
    }
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}

function checkShadowCardsUseRing() {
  const cssFiles = [
    ...walkFiles(path.join(root, 'apps/frontend/app'), ['.css']),
    ...walkFiles(path.join(root, 'apps/frontend/components'), ['.css']),
    ...walkFiles(path.join(root, 'apps/frontend/styles'), ['.css']),
    ...walkFiles(path.join(root, 'packages/muteki-ui/src'), ['.css']),
  ];
  const sources = cssFiles.map((file) => [file, fs.readFileSync(file, 'utf8').replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ' '))]);
  const vars = new Map();
  for (const [, src] of sources) {
    for (const m of src.matchAll(/(--[\w-]+)\s*:\s*([^;{}]+)/g)) {
      if (!vars.has(m[1])) vars.set(m[1], m[2].trim());
    }
  }
  const resolve = (value, depth = 0) => (depth > 6 ? value : value.replace(/var\((--[\w-]+)(?:\s*,\s*([^()]*))?\)/g, (m, name, fallback) => {
    if (/^--cx-ring/.test(name) || /^--[\w-]*-ring$/.test(name)) return 'RING';
    if (vars.has(name)) return resolve(vars.get(name), depth + 1);
    return fallback ? resolve(fallback, depth + 1) : m;
  }));
  const colorFn = /(?:rgba?|hsla?|color-mix|oklch|oklab|color)\((?:[^()]|\([^()]*\))*\)/g;
  const layers = (raw) => splitTopLevel(resolve(raw)).filter((layer) => !/\binset\b/.test(layer)).map((layer) => {
    const color = (layer.match(colorFn) || [])[0] || (layer.match(/#[0-9a-f]{3,8}\b|\bRING\b|\bvar\(--[\w-]+\)/i) || [])[0] || '';
    const nums = layer.replace(colorFn, ' ').match(/-?\d*\.?\d+(?:px|rem)?/g)?.map((n) => parseFloat(n)) ?? [];
    const [x = 0, y = 0, blur = 0, spread = 0] = nums;
    return { drop: nums.length >= 2 && (blur > 0 || x !== 0 || y !== 0), hairline: x === 0 && y === 0 && blur === 0 && spread > 0 && spread <= 1, color };
  });
  const opaqueHairline = (color) => /^#(?:[0-9a-f]{3}|[0-9a-f]{6})$/i.test(color) || /^var\(--(?:line|border|cx-border)/.test(color) || /^rgb\([^/]*\)$/.test(color);
  const hairlineBorder = (prop, value) => {
    const v = resolve(value).trim();
    if (/\b(?:none|hidden|transparent)\b/.test(v) || /^0(?:px)?$/.test(v)) return false;
    if (prop === 'border') return /(?:^|\s)1px\b/.test(v) && /\b(?:solid|dashed|dotted)\b/.test(v);
    if (prop === 'border-width') return /^1px$/.test(v);
    return false;
  };
  for (const [file, src] of sources) {
    for (const m of src.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
      const selector = m[1].trim();
      if (selector.startsWith('@') || /^(?:from|to|\d+%)/.test(selector)) continue;
      const decls = [...m[2].matchAll(/([\w-]+)\s*:\s*([^;]+)/g)].map((d) => [d[1].trim(), d[2].trim()]);
      const shadow = decls.find(([prop]) => prop === 'box-shadow');
      if (!shadow) continue;
      const shadowLayers = layers(shadow[1]);
      if (!shadowLayers.some((l) => l.drop)) continue;
      const line = src.slice(0, m.index + m[0].indexOf('{')).split('\n').length;
      if (decls.some(([prop, value]) => hairlineBorder(prop, value))) {
        addViolation('shadow-card-uses-ring', file, line, `"${selector.replace(/\s+/g, ' ').slice(0, 120)}" combines a 1px border with a drop shadow.`, 'Remove the border and put the edge in the shadow stack as `0 0 0 1px var(--cx-ring)` (tokens such as --shadow-pop and --cx-shadow-pop already include it).');
      }
      const ring = shadowLayers.find((l) => l.hairline && opaqueHairline(l.color));
      if (ring) {
        addViolation('shadow-card-uses-ring', file, line, `"${selector.replace(/\s+/g, ' ').slice(0, 120)}" pairs an opaque 1px ring (${ring.color}) with a drop shadow, which reads as a pale line over the shadow.`, 'Use var(--cx-ring) or var(--cx-ring-strong) so the edge darkens with the shadow.');
      }
    }
  }

  const tsxFiles = [
    ...walkFiles(path.join(root, 'apps/frontend/app'), ['.tsx', '.ts']),
    ...walkFiles(path.join(root, 'apps/frontend/components'), ['.tsx', '.ts']),
    ...walkFiles(path.join(root, 'packages/muteki-ui/src'), ['.tsx', '.ts']),
  ];
  const borderClass = /^(?:[\w-]+:)*border(?:-\[1px\]|-1)?$/;
  const dropShadowClass = /^(?:[\w-]+:)*shadow-(?:cx-(?:xs|sm|md|pop|composer)|xs|sm|md|lg|xl|2xl)$/;
  for (const file of tsxFiles) {
    const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);
    lines.forEach((text, idx) => {
      for (const literal of text.match(/(["'`])(?:(?!\1).)*\1/g) || []) {
        const tokens = literal.slice(1, -1).split(/\s+/);
        const variants = (t) => t.split(':').slice(0, -1).join(':');
        for (const b of tokens.filter((t) => borderClass.test(t))) {
          const s = tokens.find((t) => dropShadowClass.test(t) && variants(t) === variants(b));
          if (s) {
            addViolation('shadow-card-uses-ring', file, idx + 1, `"${b}" and "${s}" put a border and a drop shadow on the same element.`, 'Drop the border classes; use `ring-1 ring-cx-ring` with the shadow (shadow-cx-pop already includes the ring).');
          }
        }
      }
    });
  }
}
execFileSync('uv', [
  'run', '--no-project', '--python', '3.12',
  '--with-requirements', 'scripts/verify/comments-requirements.txt',
  'python', 'scripts/verify/comments.py',
  ...process.argv.slice(2),
], { cwd: root, stdio: 'inherit' });

checkNoPlaywright();
checkVerifyAntiCheating();
checkManifestAndSkill();
checkNoHeroUiOrLegacyUi();
checkShadowCardsUseRing();

if (violations.length > 0) {
  console.error(`LINT RULES FAILED (${violations.length} violation${violations.length === 1 ? '' : 's'}):`);
  for (const v of violations) {
    console.error(`\n[${v.rule}] ${v.file}:${v.line}`);
    console.error(`  Problem: ${v.detail}`);
    console.error(`  Fix:     ${v.fix}`);
  }
  process.exit(1);
}

console.log('LINT_RULES_OK: no-new-comments, no-playwright, no-skipped-or-only-verify, no-long-bare-sleep-in-verify, verify-manifest-valid, no-legacy-electron-commands, shadow-card-uses-ring');
