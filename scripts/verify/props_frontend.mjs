#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../../', import.meta.url));
const frontendRoot = path.join(root, 'apps/frontend');
const req = createRequire(path.join(frontendRoot, 'package.json'));
const ts = req('typescript');
const fc = req('fast-check');

const numRuns = Math.max(20, Number.parseInt(process.env.MUTEKI_FASTCHECK_RUNS || '100', 10) || 100);
const moduleCache = new Map();

function resolveTsFile(baseDir, specifier) {
  let target;
  if (specifier.startsWith('@/')) {
    target = path.join(frontendRoot, specifier.slice(2));
  } else if (specifier.startsWith('.')) {
    target = path.resolve(baseDir, specifier);
  } else {
    return null;
  }
  const candidates = [
    target,
    `${target}.ts`,
    `${target}.tsx`,
    `${target}.js`,
    path.join(target, 'index.ts'),
    path.join(target, 'index.tsx'),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return candidate;
  }
  throw new Error(`Cannot resolve TypeScript module "${specifier}" from "${baseDir}"`);
}

function loadTsModule(filePath) {
  const canonical = path.resolve(filePath);
  if (moduleCache.has(canonical)) return moduleCache.get(canonical).exports;
  const mod = { exports: {} };
  moduleCache.set(canonical, mod);
  const source = fs.readFileSync(canonical, 'utf8');
  const transpiled = ts.transpileModule(source, {
    fileName: canonical,
    compilerOptions: {
      module: ts.ModuleKind.CommonJS,
      target: ts.ScriptTarget.ES2022,
      esModuleInterop: true,
      isolatedModules: true,
    },
  }).outputText;

  const localRequire = (specifier) => {
    const resolved = resolveTsFile(path.dirname(canonical), specifier);
    if (resolved) return loadTsModule(resolved);
    return req(specifier);
  };
  const context = {
    module: mod,
    exports: mod.exports,
    require: localRequire,
    __filename: canonical,
    __dirname: path.dirname(canonical),
    console,
    process,
    Date,
    Math,
    JSON,
    Number,
    String,
    Boolean,
    Array,
    Object,
    Map,
    Set,
    RegExp,
    Error,
    URL,
    URLSearchParams,
    TextEncoder,
    Uint8Array,
    Uint32Array,
    DataView,
    ArrayBuffer,
  };
  vm.runInNewContext(transpiled, context, { filename: canonical });
  return mod.exports;
}

const { dispatchRoster, isDispatchDraft } = loadTsModule(path.join(frontendRoot, 'lib/dispatchRoster.ts'));
const {
  applyConversationInboxPatch,
  isInternalCtfThread,
  sanitizeConversationInboxSnapshot,
} = loadTsModule(path.join(frontendRoot, 'lib/conversationInbox.ts'));
const { emptyDeck } = loadTsModule(path.join(frontendRoot, 'lib/run-events/state.ts'));
const {
  formatCostUsd,
  summarizeContextWindow,
  summarizeRunCost,
  summarizeTokenBreakdown,
} = loadTsModule(path.join(frontendRoot, 'lib/runInspectorMetrics.ts'));
const { reduce } = loadTsModule(path.join(frontendRoot, 'lib/run-events/reduce.ts'));
const {
  classifyHitlRequest,
  groupHitlRequests,
  partitionHitlDeck,
} = loadTsModule(path.join(frontendRoot, 'lib/run-events/hitl.ts'));
const { swarmPhase } = loadTsModule(path.join(frontendRoot, 'lib/run-events/selectors.ts'));
const { EventType } = loadTsModule(path.join(frontendRoot, 'lib/run-events/types.ts'));
const {
  msUntilNextTimeSlot,
  readHomeClock,
  resolveConversationHomePrompt,
  resolveHomeTimePeriod,
  resolveHomeTitle,
} = loadTsModule(path.join(frontendRoot, 'lib/conversationHomePrompt.ts'));
const {
  buildOrchestrationSavePatches,
  cloneOrchestrationProfile,
  createSrcOverrideFromBase,
  defaultOrchestrationProfile,
  resolveModeOrchestration,
} = loadTsModule(path.join(frontendRoot, 'lib/orchestrationDraft.ts'));
const {
  buildModelCapabilitiesPayload,
  formatTokenCount,
  recommendPiModelCapabilities,
} = loadTsModule(path.join(frontendRoot, 'components/settings/agents/piModelRecommendations.ts'));
const { buildAgentCollaborationModel } = loadTsModule(path.join(frontendRoot, 'lib/agentCollaboration.ts'));
const { layoutAgents, LAYOUT } = loadTsModule(path.join(frontendRoot, 'lib/agentCollaborationLayout.ts'));
const {
  UNDO_GROUP_DELAY_MS,
  UNDO_MAX_DEPTH,
  classifyInputType,
  emptyUndoState,
  recordUndo,
  redoStep,
  sealUndo,
  undoSnapshotKey,
  undoStep,
} = loadTsModule(path.join(frontendRoot, 'lib/composerUndoHistory.ts'));

function runProp(name, arbitrary, predicate) {
  fc.assert(fc.property(arbitrary, predicate), { numRuns });
  console.log(`PASS  [frontend-prop] ${name} (${numRuns} runs)`);
}

const idArb = fc.stringMatching(/^[a-z0-9][a-z0-9-]{0,10}$/);
const engineArb = fc.constantFrom('pi', 'codex', 'claude', 'cursor', 'kimi', 'agy');

const profileArb = fc.record({
  id: idArb,
  engine: engineArb,
  label: fc.string({ minLength: 1, maxLength: 16 }),
  enabled: fc.boolean(),
  model: fc.option(fc.string({ minLength: 1, maxLength: 16 }), { nil: undefined }),
});

const settingsArb = fc.record({
  engines: fc.array(fc.oneof(engineArb, idArb), { maxLength: 6 }),
  worker_profiles: fc.uniqueArray(profileArb, { selector: (p) => p.id, maxLength: 6 }),
  overrides: fc.dictionary(
    fc.constantFrom('web', 'pwn', 'crypto', 'misc'),
    fc.record({ engines: fc.array(fc.oneof(engineArb, idArb), { maxLength: 4 }) }),
    { maxKeys: 3 },
  ),
});

runProp(
  'dispatchRoster_draft_unique_keys_and_respects_enabled_profiles',
  fc.record({
    runId: fc.constantFrom('', 'draft-1', 'draft-abc'),
    category: fc.constantFrom('', 'web', 'misc'),
    settings: fc.option(settingsArb, { nil: null }),
  }),
  ({ runId, category, settings }) => {
    const deck = { ...emptyDeck(runId), category };
    assert.equal(isDispatchDraft(runId), true);
    const rows = dispatchRoster(deck, settings);
    if (!settings) {
      assert.equal(rows.length, 0);
      return;
    }
    const keys = rows.map((r) => r.key);
    assert.equal(new Set(keys).size, keys.length, 'draft roster keys must be unique');
    for (const row of rows) {
      assert.equal(row.state, 'configured');
    }
    const disabledIds = new Set(
      (settings.worker_profiles || []).filter((p) => p.enabled === false).map((p) => p.id),
    );
    for (const row of rows) {
      assert.equal(disabledIds.has(row.key), false, `disabled profile ${row.key} leaked into roster`);
    }
  },
);

runProp(
  'dispatchRoster_active_filters_generation_and_surfaces_degraded',
  fc.record({
    currentGen: fc.integer({ min: 1, max: 5 }),
    finished: fc.boolean(),
    lanesSpec: fc.array(
      fc.record({
        idx: fc.integer({ min: 0, max: 7 }),
        engine: engineArb,
        gen: fc.integer({ min: 0, max: 5 }),
        online: fc.boolean(),
        solved: fc.boolean(),
      }),
      { maxLength: 6 },
    ),
    degraded: fc.dictionary(engineArb, fc.string({ minLength: 1, maxLength: 24 }), { maxKeys: 3 }),
  }),
  ({ currentGen, finished, lanesSpec, degraded }) => {
    const deck = {
      ...emptyDeck('run-prop-1'),
      executionGeneration: currentGen,
      finished,
      degradedEngines: degraded,
      lanes: Object.fromEntries(
        lanesSpec.map((item) => {
          const suffix = item.gen > 0 ? `-g${item.gen}` : '';
          const id = `cli-${item.engine}-${item.idx}${suffix}`;
          return [
            id,
            {
              solverId: id,
              engine: item.engine,
              profileId: `prof-${item.engine}-${item.idx}`,
              profileLabel: item.engine,
              status: item.online ? 'running' : 'idle',
              reasoning: '',
              toolLines: [],
              solved: item.solved,
              online: item.online,
            },
          ];
        }),
      ),
    };
    const rows = dispatchRoster(deck, null);
    const keys = rows.map((r) => r.key);
    assert.equal(new Set(keys).size, keys.length, 'active roster keys must be unique');

    for (const row of rows) {
      if (row.key.startsWith('cli-') && row.key.includes('-g')) {
        const genMatch = row.key.match(/-g(\d+)$/);
        if (genMatch) {
          assert.equal(Number(genMatch[1]), currentGen, 'stale generation worker leaked');
        }
      }
    }
    for (const eng of Object.keys(degraded)) {
      assert.equal(
        rows.some((r) => r.state === 'degraded' && (r.key === eng || r.engine === eng)),
        true,
        `degraded engine ${eng} missing from active roster`,
      );
    }
  },
);

const eventTypeList = [
  EventType.RUN_PREPARING,
  EventType.RUN_STARTED,
  EventType.RUN_TITLED,
  EventType.RUN_FINISHED,
  EventType.WORKER_STATUS,
  EventType.TEXT_MESSAGE_DELTA,
  EventType.COST_UPDATE,
  EventType.CONTROL_COMMAND,
];

const mutekiEventArb = fc.record({
  event_type: fc.constantFrom(...eventTypeList),
  seq: fc.integer({ min: 1, max: 1000 }),
  ts: fc.integer({ min: 1_700_000_000, max: 1_900_000_000 }),
  run_id: fc.constant('run-prop-1'),
  solver_id: fc.option(fc.constantFrom('reason', 'cli-pi-1', 'cli-codex-1'), { nil: null }),
  payload: fc.record({
    execution_generation: fc.option(fc.integer({ min: 0, max: 6 }), { nil: undefined }),
    control_generation: fc.option(fc.integer({ min: 0, max: 6 }), { nil: undefined }),
    title: fc.option(fc.string({ maxLength: 32 }), { nil: undefined }),
    status: fc.option(fc.constantFrom('running', 'completed', 'failed'), { nil: undefined }),
    delta: fc.option(fc.string({ maxLength: 40 }), { nil: undefined }),
    usd: fc.option(fc.double({ min: 0, max: 10, noNaN: true }), { nil: undefined }),
  }),
});

runProp(
  'run_events_reduce_monotonic_generations_and_rejects_stale',
  fc.array(mutekiEventArb, { minLength: 1, maxLength: 30 }),
  (events) => {
    let state = emptyDeck('run-prop-1');
    for (const ev of events) {
      const prevExec = state.executionGeneration ?? 0;
      const prevCtrl = state.controlGeneration ?? 0;
      const next = reduce(state, ev);
      const evExec = Number(ev.payload?.execution_generation);
      if (Number.isFinite(evExec) && evExec >= 0 && evExec < prevExec) {
        assert.equal(next, state, 'stale execution_generation event must return exact prev state');
      }
      assert.ok(
        (next.executionGeneration ?? 0) >= prevExec,
        'executionGeneration must never decrease',
      );
      assert.ok(
        (next.controlGeneration ?? 0) >= prevCtrl,
        'controlGeneration must never decrease',
      );
      assert.ok(Array.isArray(next.chat));
      assert.ok(Array.isArray(next.blackboard.facts));
      assert.ok(next.lanes && typeof next.lanes === 'object');
      state = next;
    }
  },
);

runProp(
  'run_events_reduce_preserves_container_identity_on_noop',
  fc.integer({ min: 1, max: 100 }),
  (seq) => {
    const initial = emptyDeck('run-prop-noop');
    const noopEvent = {
      event_type: 'unknown.noop.event',
      seq,
      ts: 1_800_000_000,
      run_id: 'run-prop-noop',
      solver_id: null,
      payload: {},
    };
    const next = reduce(initial, noopEvent);
    assert.equal(next.lanes, initial.lanes);
    assert.equal(next.graph, initial.graph);
    assert.equal(next.sharedGraph, initial.sharedGraph);
    assert.equal(next.reason, initial.reason);
    assert.equal(next.blackboard, initial.blackboard);
  },
);

runProp(
  'conversationHomePrompt_time_period_accuracy_and_no_day_night_inversion',
  fc.record({
    hour: fc.integer({ min: 0, max: 23 }),
    isWeekend: fc.boolean(),
    seed: fc.integer({ min: -1000, max: 1000 }),
  }),
  ({ hour, isWeekend, seed }) => {
    const period = resolveHomeTimePeriod(hour);
    const expected =
      hour >= 5 && hour < 8 ? 'dawn'
        : hour >= 8 && hour < 11 ? 'morning'
          : hour >= 11 && hour < 14 ? 'midday'
            : hour >= 14 && hour < 18 ? 'afternoon'
              : hour >= 18 && hour < 22 ? 'evening'
                : (hour >= 22 || hour < 1) ? 'late_night'
                  : 'after_midnight';
    assert.equal(period, expected);

    const title = resolveHomeTitle({ period, dayKey: '2026-11-20', isWeekend, hour }, seed);
    assert.ok(title.trim().length > 0);
    assert.ok(resolveHomeTitle(null, seed).trim().length > 0);

    if (hour >= 6 && hour <= 17) {
      assert.equal(/(夜|晚|凌晨|守夜)/.test(title), false, `daytime hour ${hour} produced night copy: ${title}`);
    }
    if (hour >= 18 || hour <= 4) {
      assert.equal(
        /(早|晨|上午|中午|午间|午后|下午)/.test(title.replaceAll('凌晨', '')),
        false,
        `night hour ${hour} produced daytime copy: ${title}`,
      );
    }
  },
);

runProp(
  'conversationHomePrompt_context_awareness_and_next_slot_bounds',
  fc.record({
    ts: fc.integer({ min: Date.UTC(2025, 0, 1), max: Date.UTC(2027, 11, 31) }),
    seed: fc.integer({ min: 0, max: 500 }),
    projectName: fc.oneof(
      fc.constant(''),
      fc.constant('   '),
      fc.stringMatching(/^[a-zA-Z0-9_-]{1,18}$/),
      fc.stringMatching(/^[a-zA-Z0-9_-]{26,40}$/),
    ),
    hasProjects: fc.boolean(),
    workspaceMode: fc.constantFrom('shared_checkout', 'existing_worktree', 'new_worktree'),
    worktreeBranch: fc.constantFrom('', 'feat/home-copy'),
    interactionMode: fc.constantFrom('default', 'plan'),
    attachmentCount: fc.integer({ min: 0, max: 5 }),
  }),
  ({ ts, seed, projectName, hasProjects, workspaceMode, worktreeBranch, interactionMode, attachmentCount }) => {
    const now = new Date(ts);
    const ms = msUntilNextTimeSlot(now);
    assert.ok(ms >= 1000 && ms <= 4 * 60 * 60 * 1000 + 1000, `unexpected slot wait ms=${ms}`);
    const next = new Date(now.getTime() + ms);
    const crossesMidnight =
      next.getDate() !== now.getDate()
      || next.getMonth() !== now.getMonth()
      || next.getFullYear() !== now.getFullYear();
    assert.ok(
      crossesMidnight || resolveHomeTimePeriod(next.getHours()) !== resolveHomeTimePeriod(now.getHours()),
      'next time slot must advance period or roll over midnight',
    );

    const clock = readHomeClock(now);
    const trimmedProject = projectName.trim();
    const prompt = resolveConversationHomePrompt({
      clock,
      seed,
      projectName,
      hasSelectedProject: Boolean(trimmedProject),
      hasProjects,
      workspaceMode,
      worktreeBranch,
      interactionMode,
      attachmentCount,
    });
    assert.ok(prompt.subtitle.trim().length > 0);
    assert.ok(prompt.placeholder.trim().length > 0);
    assert.equal(prompt.suggestions.length, 4);

    if (trimmedProject && trimmedProject.length <= 24) {
      assert.ok(prompt.subtitle.includes(trimmedProject), `subtitle should include ${trimmedProject}`);
    }
    if (trimmedProject && trimmedProject.length > 24) {
      assert.ok(prompt.subtitle.includes('…'), 'long project name should be truncated');
    }
    if (attachmentCount === 0 && interactionMode === 'plan') {
      assert.ok(prompt.subtitle.includes('规划模式'), 'plan mode subtitle should mention 规划模式');
    }
    if (!trimmedProject) {
      assert.equal(prompt.suggestions.some((s) => Boolean(s.workspace)), false);
    } else {
      assert.equal(prompt.suggestions.some((s) => Boolean(s.workspace)), true);
    }
  },
);

runProp(
  'orchestration_two_layer_isolation_and_patches',
  fc.record({
    chatProvider: fc.constantFrom('', 'claude', 'codex'),
    srcProvider: fc.constantFrom('codex', 'gemini', 'pi'),
    srcModel: fc.stringMatching(/^[a-z0-9._-]{3,18}$/),
    srcEffort: fc.constantFrom('low', 'medium', 'high'),
    srcDefaultRole: fc.constantFrom('', 'reviewer', 'researcher'),
    editChat: fc.boolean(),
    editSrc: fc.boolean(),
    revertSrc: fc.boolean(),
  }),
  ({ chatProvider, srcProvider, srcModel, srcEffort, srcDefaultRole, editChat, editSrc, revertSrc }) => {
    const savedChat = defaultOrchestrationProfile();
    if (chatProvider) {
      savedChat.roles = savedChat.roles.map((r) =>
        r.id === 'reviewer' ? { ...r, mode: 'pinned', provider: chatProvider, model: 'chat-model' } : r,
      );
    }
    const savedSrc = revertSrc ? createSrcOverrideFromBase(savedChat) : null;

    // Inherited SRC resolves Chat base; independent SRC resolves override
    const inheritedView = resolveModeOrchestration(savedChat, null, 'src');
    assert.equal(inheritedView.inherited, true);
    assert.equal(inheritedView.scope, 'default');
    assert.deepEqual(inheritedView.profile, savedChat);

    const draftChat = cloneOrchestrationProfile(savedChat);
    if (editChat) {
      draftChat.roles = draftChat.roles.map((r) =>
        r.id === 'planner' ? { ...r, effort: r.effort === 'high' ? 'medium' : 'high' } : r,
      );
    }

    let draftSrc = savedSrc ? cloneOrchestrationProfile(savedSrc) : null;
    if (revertSrc) {
      draftSrc = null;
    } else if (editSrc) {
      draftSrc = createSrcOverrideFromBase(draftChat);
      draftSrc.default_role = srcDefaultRole;
      draftSrc.roles = draftSrc.roles.map((r) =>
        r.id === 'reviewer'
          ? { ...r, mode: 'pinned', provider: srcProvider, model: srcModel, effort: srcEffort }
          : r,
      );
      // Mutating SRC override must not mutate draftChat or savedChat
      const chatReviewer = draftChat.roles.find((r) => r.id === 'reviewer');
      assert.equal(chatReviewer?.provider, chatProvider);
      const srcView = resolveModeOrchestration(draftChat, draftSrc, 'src');
      assert.equal(srcView.inherited, false);
      assert.equal(srcView.scope, 'mode_override');
      assert.equal(srcView.profile.roles.find((r) => r.id === 'reviewer')?.model, srcModel);
    }

    const patches = buildOrchestrationSavePatches(savedChat, draftChat, savedSrc, draftSrc);
    if (!editChat) {
      assert.equal(
        patches.some((p) => p.orchestration_mode === 'conversation'),
        false,
        'conversation patch must not be emitted when only SRC changed',
      );
    } else {
      assert.equal(patches.some((p) => p.orchestration_mode === 'conversation'), true);
    }

    if (revertSrc) {
      const srcPatch = patches.find((p) => p.orchestration_mode === 'src');
      assert.ok(srcPatch);
      assert.equal(srcPatch.orchestration, null);
    } else if (!editSrc) {
      assert.equal(
        patches.some((p) => p.orchestration_mode === 'src'),
        false,
        'src patch must not be emitted when only chat changed',
      );
    } else {
      const srcPatch = patches.find((p) => p.orchestration_mode === 'src');
      assert.ok(srcPatch);
      assert.notEqual(srcPatch.orchestration, null);
    }
  },
);

const { createNotificationInboxDrain } = loadTsModule(path.join(frontendRoot, 'lib/notificationInboxCursor.ts'));
await fc.assert(fc.asyncProperty(fc.integer({ min: 3, max: 100 }), async (lastSequence) => {
  let acknowledge;
  const receipt = new Promise((resolve) => { acknowledge = resolve; });
  const seen = [];
  const notifications = createNotificationInboxDrain(async (snapshot) => {
    seen.push(snapshot.inbox_seq);
    if (snapshot.inbox_seq === 1) await receipt;
  });
  for (let seq = 1; seq <= lastSequence; seq++) {
    notifications.offer({ inbox_seq: seq, broker_epoch: 'broker', threads: [], summaries: [] });
  }
  assert.deepEqual(seen, [1]);
  acknowledge();
  await new Promise(setImmediate);
  assert.deepEqual(seen, [1, lastSequence]);
}), { numRuns });
console.log(`PASS  [frontend-prop] inbox_recovers_latest_state_after_notification_receipt (${numRuns} runs)`);

const piModelIdArb = fc.oneof(
  fc.constantFrom(
    'deepseek-flash',
    'deepseek-v4.1-flash',
    'deepseek-r1',
    'claude-sonnet-4-6',
    'claude-opus-4-6[1m]',
    'gpt-5.4',
    'gpt-4.1',
    'gemini-2.5-pro',
    'kimi-k3',
    'glm-5.3',
    'qwen3-coder',
    'minimax-m3.1-flash',
    'grok-4.6',
    'custom-model-256k',
    'unknown-local-model',
  ),
  fc.stringMatching(/^[a-z0-9][a-z0-9._/-]{0,20}$/),
);

runProp(
  'piModelRecommendations_bounds_and_endpoint_precedence',
  fc.record({
    modelId: piModelIdArb,
    baseUrl: fc.constantFrom(
      '',
      'https://api.example.invalid/v1',
      'https://ark.cn-beijing.volces.com/api/plan/v3',
    ),
    endpointCtx: fc.option(fc.integer({ min: 4096, max: 2_000_000 }), { nil: undefined }),
    endpointOut: fc.option(fc.integer({ min: 1024, max: 400_000 }), { nil: undefined }),
  }),
  ({ modelId, baseUrl, endpointCtx, endpointOut }) => {
    const discovered = endpointCtx || endpointOut
      ? { id: modelId, context_window: endpointCtx, max_output_tokens: endpointOut }
      : null;
    const rec = recommendPiModelCapabilities(modelId, baseUrl, discovered);
    if (!rec) {
      assert.equal(discovered, null);
      return;
    }
    assert.ok(Number.isInteger(rec.context_window) && rec.context_window > 0);
    assert.ok(Number.isInteger(rec.max_output_tokens) && rec.max_output_tokens > 0);
    assert.ok(rec.context_window >= rec.max_output_tokens);
    if (endpointCtx !== undefined) {
      assert.equal(rec.source, 'endpoint');
      assert.equal(rec.context_window, endpointCtx);
    }
    if (endpointOut !== undefined) {
      assert.equal(rec.source, 'endpoint');
      assert.equal(rec.max_output_tokens, Math.min(endpointOut, rec.context_window));
    }
  },
);

runProp(
  'piModelCapabilitiesPayload_validation_and_thinking_map_preservation',
  fc.record({
    model: fc.constantFrom('deepseek-flash', 'gpt-5.4'),
    ctx: fc.oneof(
      fc.constant(''),
      fc.integer({ min: 1024, max: 2_000_000 }).map(String),
      fc.constantFrom('0', '-128', 'abc', '12.5'),
    ),
    out: fc.oneof(
      fc.constant(''),
      fc.integer({ min: 256, max: 200_000 }).map(String),
      fc.constantFrom('0', '-1', 'xyz'),
    ),
    reasoning: fc.boolean(),
    imageInput: fc.boolean(),
    preserveMap: fc.boolean(),
  }),
  ({ model, ctx, out, reasoning, imageInput, preserveMap }) => {
    const existing = preserveMap
      ? { [model]: { thinkingLevelMap: { low: 'low', high: 'high' } } }
      : undefined;
    const res = buildModelCapabilitiesPayload(
      { [model]: { contextWindow: ctx, maxOutputTokens: out, reasoning, imageInput } },
      existing,
    );
    const validInt = (s) => s === '' || (/^\d+$/.test(s) && Number(s) > 0);
    if (!validInt(ctx) || !validInt(out)) {
      assert.equal(res.ok, false);
      return;
    }
    assert.equal(res.ok, true);
    if (ctx === '' && out === '' && !reasoning && !imageInput && !preserveMap) {
      assert.equal(res.value[model], undefined);
      return;
    }
    const entry = res.value[model];
    assert.ok(entry);
    assert.deepEqual(JSON.parse(JSON.stringify(entry.input)), imageInput ? ['text', 'image'] : ['text']);
    if (preserveMap) {
      assert.deepEqual(JSON.parse(JSON.stringify(entry.thinkingLevelMap)), { low: 'low', high: 'high' });
    }
  },
);

runProp(
  'formatTokenCount_positive_integers_render_compact_labels',
  fc.integer({ min: 1, max: 5_000_000 }),
  (tokens) => {
    assert.equal(formatTokenCount(128_000), '128K');
    assert.equal(formatTokenCount(256_000), '256K');
    assert.equal(formatTokenCount(262_144), '256K');
    assert.equal(formatTokenCount(512_000), '512K');
    const label = formatTokenCount(tokens);
    assert.ok(label.length > 0);
    if (tokens >= 1_000_000) assert.ok(label.endsWith('M'));
    else if (tokens >= 1_000) assert.ok(label.endsWith('K'));
    else assert.equal(label, String(tokens));
  },
);

runProp(
  'collaboration_fgs_lineage_origin_dispatch_and_dropped_steps',
  fc.record({
    mode: fc.constantFrom('ctf', 'pentest'),
    challengerSuperseded: fc.boolean(),
    useRequiredPoc: fc.boolean(),
    baseTs: fc.integer({ min: 1_750_000_000, max: 1_850_000_000 }),
  }),
  ({ mode, challengerSuperseded, useRequiredPoc, baseTs }) => {
    let seq = 0;
    const ev = (event_type, dt, solver_id, payload) => ({
      event_type,
      seq: ++seq,
      ts: baseTs + dt,
      run_id: 'run-prop-collab',
      solver_id,
      payload,
    });
    const events = [
      ev(EventType.RUN_STARTED, 0, null, { mode, kind: 'swarm', challenge: { mode, description: 'http://target.test/' } }),
      ev(EventType.BLACKBOARD_DELTA, 1, 'origin', { kind: 'fact_origin', fact_seq: 2, actor: 'origin', fact: 'http://target.test/' }),
      ev(EventType.BLACKBOARD_DELTA, 2, 'reason', { kind: 'intent_proposed', intent_id: 'step_001', goal: 'Recon root', from_facts: [2] }),
      ev(EventType.BLACKBOARD_DELTA, 3, 'cli-pi', { kind: 'intent_claimed', intent_id: 'step_001', worker: 'cli-pi' }),
      ev(EventType.BLACKBOARD_DELTA, 3, 'cli-pi', { kind: 'worker_spawned', worker: 'cli-pi', phase: 'explore', engine: 'pi', intent_id: 'step_001' }),
      ev(EventType.BLACKBOARD_DELTA, 4, 'reason', { kind: 'intent_proposed', intent_id: 'step_002', goal: 'Probe paths', from_facts: [2] }),
      ev(EventType.BLACKBOARD_DELTA, 5, 'cli-pi-2', { kind: 'intent_claimed', intent_id: 'step_002', worker: 'cli-pi-2' }),
      ev(EventType.BLACKBOARD_DELTA, 5, 'cli-pi-2', { kind: 'worker_spawned', worker: 'cli-pi-2', phase: 'explore', engine: 'pi', intent_id: 'step_002' }),
      ev(EventType.BLACKBOARD_DELTA, 6, 'reason', { kind: 'intent_drop_requested', intent_id: 'step_002', worker: 'cli-pi-2', reason: 'Decide dropped' }),
      ev(EventType.BLACKBOARD_DELTA, 10, 'cli-pi', { kind: 'fact_added', fact_seq: 11, fact: 'Found /download', verified: true, actor: 'cli-pi', source_solver: 'cli-pi', intent_id: 'step_001' }),
      ev(EventType.BLACKBOARD_DELTA, 11, 'cli-pi', { kind: 'poc_saved', poc_id: 'poc-1', name: 'probe.py', intent_id: 'step_001', status: 'available' }),
      ev(EventType.BLACKBOARD_DELTA, 12, 'cli-pi', { kind: 'intent_concluded', intent_id: 'step_001', worker: 'cli-pi', result: 'explored', to_fact_seq: 11 }),
      ev(EventType.BLACKBOARD_DELTA, 14, 'challenger', {
        kind: 'intent_proposed',
        intent_id: 'step_003',
        goal: 'Test /download sign',
        from_facts: [11],
        depends_on: ['step_001'],
        required_pocs: useRequiredPoc ? ['poc-1'] : [],
      }),
      ev(EventType.BLACKBOARD_DELTA, 15, 'cli-pi-3', { kind: 'intent_claimed', intent_id: 'step_003', worker: 'cli-pi-3' }),
      ev(EventType.BLACKBOARD_DELTA, 15, 'cli-pi-3', { kind: 'worker_spawned', worker: 'cli-pi-3', phase: 'explore', engine: 'pi', intent_id: 'step_003' }),
    ];
    if (challengerSuperseded) {
      events.push(
        ev(EventType.BLACKBOARD_DELTA, 16, 'challenger', {
          kind: 'fact_superseded',
          actor: 'challenger',
          fact_seq: 11,
          reason: 'Hypothesis falsified',
        }),
      );
    }

    let deck = emptyDeck('run-prop-collab');
    for (const item of events) deck = reduce(deck, item);
    const model = buildAgentCollaborationModel(deck, 'all');

    assert.equal(model.agentById.has('origin'), true, 'origin node must exist');
    assert.equal(model.coordinator.coordination.verifiedFacts, challengerSuperseded ? 0 : 1);

    for (const rel of model.relations) {
      assert.notEqual(rel.source, rel.target, `self-loop relation ${rel.id}`);
    }
    assert.equal(
      model.relations.some((r) => r.kind === 'dispatch' && r.source === 'coordinator' && r.target === 'cli-pi'),
      true,
    );
    assert.equal(
      model.relations.some((r) => r.kind === 'dispatch' && r.source === 'challenger' && r.target === 'cli-pi-3'),
      true,
    );
    assert.equal(
      model.relations.some((r) => r.kind === 'handoff' && r.source === 'origin' && r.target === 'cli-pi'),
      true,
    );
    assert.equal(
      model.relations.some((r) => r.kind === 'handoff' && r.source === 'cli-pi' && r.target === 'cli-pi-3'),
      true,
    );
    if (challengerSuperseded) {
      assert.equal(
        model.relations.some((r) => r.kind === 'review' && r.source === 'challenger' && r.target === 'cli-pi'),
        true,
      );
    }

    const droppedIntent = deck.blackboard.intents.find((i) => i.id === 'step_002');
    assert.equal(droppedIntent?.status, 'done');
    assert.equal(droppedIntent?.dispatchState, 'retired');
    assert.equal(
      model.relations.some((r) => r.kind === 'report' && r.source === 'cli-pi-2'),
      false,
    );
  },
);

runProp(
  'collaboration_layout_fgs_dag_ordering_and_cycle_safety',
  fc.record({
    workerCount: fc.integer({ min: 3, max: 10 }),
    edges: fc.array(
      fc.record({
        u: fc.integer({ min: 0, max: 9 }),
        v: fc.integer({ min: 0, max: 9 }),
      }),
      { maxLength: 14 },
    ),
    includeReview: fc.boolean(),
    includeVerifier: fc.boolean(),
    includePeerVerify: fc.boolean(),
    includeCycle: fc.boolean(),
  }),
  ({ workerCount, edges, includeReview, includeVerifier, includePeerVerify, includeCycle }) => {
    const baseAgent = (id, role, firstSeenAt) => ({
      id,
      role,
      generation: 1,
      isCurrent: true,
      identity: {},
      presentation: {},
      phase: role,
      statusKind: 'offline',
      online: false,
      intents: [],
      knowledge: [],
      searchText: id,
      latestActivity: '',
      firstSeenAt,
      lifecycleSource: 'lane',
      tokens: 0,
      usd: 0,
      locks: 0,
      facts: 0,
      observations: 0,
      candidates: 0,
      deadEnds: 0,
      pocs: 0,
      reviews: 0,
      isAnomaly: false,
      pendingHitl: 0,
    });

    const agents = [baseAgent('coordinator', 'coordinator', 100)];
    const workerIds = [];
    for (let i = 0; i < workerCount; i += 1) {
      const id = `cli-pi-${i + 1}`;
      workerIds.push(id);
      agents.push(baseAgent(id, 'worker', 200 + i * 10));
    }

    const relations = [];
    const dagPairs = [];
    let relIdx = 0;
    const seenPairs = new Set();
    for (const { u, v } of edges) {
      const a = u % workerCount;
      const b = v % workerCount;
      if (a === b) continue;
      const src = Math.min(a, b);
      const dst = Math.max(a, b);
      const key = `${src}:${dst}`;
      if (seenPairs.has(key)) continue;
      seenPairs.add(key);
      dagPairs.push([workerIds[src], workerIds[dst]]);
      relations.push({
        id: `handoff:${++relIdx}`,
        kind: 'handoff',
        source: workerIds[src],
        target: workerIds[dst],
        count: 1,
        refIds: [`fact:${relIdx}`],
        ts: 200 + dst * 10,
        lastTs: 200 + dst * 10,
        timestamps: [200 + dst * 10],
        active: false,
      });
    }

    if (includeCycle && workerCount >= 2) {
      relations.push({
        id: `handoff:cycle`,
        kind: 'handoff',
        source: workerIds[workerCount - 1],
        target: workerIds[0],
        count: 1,
        refIds: ['fact:cycle'],
        ts: 900,
        lastTs: 900,
        timestamps: [900],
        active: false,
      });
    }

    if (includePeerVerify && workerCount >= 2) {
      relations.push({
        id: 'verify:peer',
        kind: 'verify',
        source: workerIds[0],
        target: workerIds[1],
        count: 1,
        refIds: ['fact:peer'],
        ts: 250,
        lastTs: 250,
        timestamps: [250],
        active: false,
      });
    }

    if (includeReview) {
      agents.push(baseAgent('cli-claude-review', 'review', 500));
      relations.push({
        id: 'review:r1',
        kind: 'review',
        source: 'cli-claude-review',
        target: workerIds[0],
        count: 1,
        refIds: ['review:r1'],
        ts: 500,
        lastTs: 500,
        timestamps: [500],
        active: false,
      });
    }

    if (includeVerifier) {
      agents.push(baseAgent('cli-verifier-1', 'verifier', 520));
      relations.push({
        id: 'verify:v1',
        kind: 'verify',
        source: 'cli-verifier-1',
        target: workerIds[workerCount - 1],
        count: 1,
        refIds: ['fact:v1'],
        ts: 520,
        lastTs: 520,
        timestamps: [520],
        active: false,
      });
    }

    const positions = layoutAgents(agents, relations);
    const coords = new Set();
    for (const agent of agents) {
      const pos = positions[agent.id];
      assert.ok(pos && Number.isFinite(pos.x) && Number.isFinite(pos.y), `missing/invalid pos for ${agent.id}`);
      const slotKey = `${pos.x}:${pos.y}`;
      assert.equal(coords.has(slotKey), false, `overlapping node slot ${slotKey}`);
      coords.add(slotKey);
    }

    for (const [src, dst] of dagPairs) {
      assert.ok(positions[dst].x > positions[src].x, `expected ${dst}.x > ${src}.x`);
    }
    if (includeReview) {
      assert.ok(positions['cli-claude-review'].x > positions[workerIds[0]].x);
    }
    if (includeVerifier) {
      assert.ok(positions['cli-verifier-1'].x > positions[workerIds[workerCount - 1]].x);
    }
    const col1X = LAYOUT.origin + LAYOUT.nodeWidth + LAYOUT.columnGap;
    if (includePeerVerify && !dagPairs.some(([, dst]) => dst === workerIds[1])) {
      assert.equal(positions[workerIds[1]].x, col1X);
    }
    if (!includeCycle && !dagPairs.some(([, dst]) => dst === workerIds[0])) {
      assert.equal(positions[workerIds[0]].x, col1X);
    }
  },
);

runProp(
  'local_pi_and_multi_flag_run_finished_status_consistency',
  fc.record({
    multiFlag: fc.boolean(),
    expectedFlags: fc.integer({ min: 1, max: 3 }),
    platformRequired: fc.boolean(),
    payloadSolved: fc.boolean(),
    finishReason: fc.constantFrom('solved', 'operator_stop', 'budget_exhausted', 'no_progress', 'finished'),
  }),
  ({ multiFlag, expectedFlags, platformRequired, payloadSolved, finishReason }) => {
    const runId = 'run-prop-pi';
    const flag = 'flag{prop-test-1}';
    let deck = emptyDeck(runId);
    deck = reduce(deck, {
      event_type: EventType.RUN_STARTED,
      seq: 1,
      ts: 1_800_000_001,
      run_id: runId,
      solver_id: null,
      payload: {
        kind: 'local_pi',
        driver_kind: 'local_pi',
        challenge: {
          mode: 'ctf',
          multi_flag: multiFlag,
          expected_flags: expectedFlags,
          platform_confirmation_required: platformRequired,
        },
      },
    });
    deck = reduce(deck, {
      event_type: EventType.LOCAL_PI_SESSION,
      seq: 2,
      ts: 1_800_000_002,
      run_id: runId,
      solver_id: 'cli-pi#1',
      payload: {
        chat_thread_id: 'ctf-pi-prop',
        provider_session_id: 'sess-1',
        model: 'test-model',
        turn_index: 1,
      },
    });
    deck = reduce(deck, {
      event_type: EventType.BLACKBOARD_DELTA,
      seq: 3,
      ts: 1_800_000_003,
      run_id: runId,
      solver_id: 'cli-pi#1',
      payload: {
        kind: 'flag_found',
        flag,
        actor: 'cli-pi#1',
        source: 'cli-pi#1',
      },
    });
    if (multiFlag && expectedFlags <= 1) {
      assert.equal(swarmPhase(deck), 'collecting');
    }
    deck = reduce(deck, {
      event_type: EventType.LOCAL_PI_TURN_COMPLETED,
      seq: 4,
      ts: 1_800_000_004,
      run_id: runId,
      solver_id: 'cli-pi#1',
      payload: {
        turn_index: 1,
        interrupted: false,
      },
    });
    if (multiFlag && expectedFlags <= 1) {
      assert.equal(swarmPhase(deck), 'paused');
    }
    deck = reduce(deck, {
      event_type: EventType.RUN_FINISHED,
      seq: 5,
      ts: 1_800_000_005,
      run_id: runId,
      solver_id: 'cli-pi#1',
      payload: {
        solved: payloadSolved,
        flag,
        flags: [flag],
        expected_flags: expectedFlags,
        multi_flag: multiFlag,
        reason: finishReason,
      },
    });
    assert.equal(deck.localPiSession?.sessionState, 'stopped');
    assert.equal(deck.localPiSession?.turnActive, false);
    if (!platformRequired && (payloadSolved || (multiFlag && expectedFlags <= 1))) {
      assert.equal(deck.solved, true);
      assert.equal(deck.outcomeReason, 'solved');
      assert.equal(swarmPhase(deck), 'solved');
    }
    if (platformRequired && !payloadSolved) {
      assert.equal(deck.solved, false);
    }
  },
);

runProp(
  'run_inspector_context_window_bounds_and_tones',
  fc.record({
    used: fc.option(fc.integer({ min: 0, max: 2_000_000 }), { nil: null }),
    limit: fc.option(fc.integer({ min: -100, max: 1_000_000 }), { nil: null }),
  }),
  ({ used, limit }) => {
    const summary = summarizeContextWindow(used, limit);
    if (limit == null || limit <= 0) {
      assert.equal(summary.limit, null);
      assert.equal(summary.ratio, null);
      assert.equal(summary.percent, null);
      assert.equal(summary.tone, 'unknown');
      return;
    }
    if (used == null) {
      assert.equal(summary.ratio, null);
      assert.equal(summary.tone, 'unknown');
      return;
    }
    assert.ok(summary.percent != null && summary.percent >= 0 && summary.percent <= 100);
    const ratio = used / limit;
    if (ratio >= 0.85) {
      assert.equal(summary.tone, 'danger');
    } else if (ratio >= 0.7) {
      assert.equal(summary.tone, 'warn');
    } else {
      assert.equal(summary.tone, 'normal');
    }
  },
);

runProp(
  'run_inspector_unpriced_solve_does_not_surface_auxiliary_cost_as_primary',
  fc.record({
    auxCost: fc.double({ min: 0.00001, max: 100, noNaN: true, noDefaultInfinity: true }),
    solveRecords: fc.integer({ min: 1, max: 10 }),
    inTok: fc.integer({ min: 1, max: 500_000 }),
    outTok: fc.integer({ min: 1, max: 500_000 }),
    cacheTok: fc.integer({ min: 0, max: 5_000_000 }),
  }),
  ({ auxCost, solveRecords, inTok, outTok, cacheTok }) => {
    const ledger = {
      records: solveRecords + 1,
      input_tokens: inTok + 100,
      output_tokens: outTok + 50,
      cache_read_tokens: cacheTok,
      total_tokens: inTok + outTok + 150,
      cost: auxCost,
      reported_cost: null,
      estimated_cost: auxCost,
      unpriced: solveRecords,
      pricing_coverage: { catalog: 1, unpriced: solveRecords },
    };
    const solve = {
      records: solveRecords,
      total_tokens: inTok + outTok,
      cost: null,
      reported_cost: null,
      estimated_cost: null,
      unpriced: solveRecords,
    };
    const auxiliary = {
      records: 1,
      total_tokens: 150,
      cost: auxCost,
      reported_cost: null,
      estimated_cost: auxCost,
      unpriced: 0,
    };
    const zh = summarizeRunCost(ledger, solve, auxiliary, false);
    assert.equal(zh.primary, '未定价');
    assert.equal(zh.muted, true);
    assert.ok(zh.secondary && zh.secondary.includes(formatCostUsd(auxCost)));
    const en = summarizeRunCost(ledger, solve, auxiliary, true);
    assert.equal(en.primary, 'Unpriced');
    assert.ok(en.secondary && en.secondary.includes(formatCostUsd(auxCost)));
  },
);

runProp(
  'run_inspector_token_breakdown_precedence_and_fallback',
  fc.record({
    hasLedger: fc.boolean(),
    inTok: fc.integer({ min: 0, max: 1_000_000 }),
    outTok: fc.integer({ min: 0, max: 1_000_000 }),
    cacheTok: fc.integer({ min: 0, max: 5_000_000 }),
    fbIn: fc.integer({ min: 0, max: 100_000 }),
    fbOut: fc.integer({ min: 0, max: 100_000 }),
  }),
  ({ hasLedger, inTok, outTok, cacheTok, fbIn, fbOut }) => {
    const ledger = hasLedger
      ? {
          records: 1,
          input_tokens: inTok,
          output_tokens: outTok,
          cache_read_tokens: cacheTok,
          total_tokens: inTok + outTok,
          cost: null,
          reported_cost: null,
          estimated_cost: null,
          unpriced: 1,
        }
      : {
          records: 0,
          total_tokens: null,
          cost: null,
          reported_cost: null,
          estimated_cost: null,
          unpriced: 0,
        };
    const res = summarizeTokenBreakdown(ledger, {
      input_tokens: fbIn,
      output_tokens: fbOut,
      total_tokens: fbIn + fbOut,
    });
    assert.ok(res.hasAny);
    if (hasLedger) {
      assert.equal(res.input, inTok);
      assert.equal(res.output, outTok);
      assert.equal(res.cacheRead, cacheTok);
      assert.equal(res.total, inTok + outTok);
    } else {
      assert.equal(res.input, fbIn);
      assert.equal(res.output, fbOut);
      assert.equal(res.total, fbIn + fbOut);
    }
  },
);

runProp(
  'conversation_inbox_filters_internal_ctf_threads',
  fc.record({
    normalCount: fc.integer({ min: 1, max: 6 }),
    taggedCtfCount: fc.integer({ min: 1, max: 4 }),
    prefixCtfCount: fc.integer({ min: 1, max: 4 }),
    patchNormalCount: fc.integer({ min: 0, max: 3 }),
    patchCtfCount: fc.integer({ min: 1, max: 3 }),
  }),
  ({ normalCount, taggedCtfCount, prefixCtfCount, patchNormalCount, patchCtfCount }) => {
    const mkThread = (thread_id, ctf_run_id = null) => ({
      thread_id,
      title: `T ${thread_id}`,
      mode: 'conversation',
      ...(ctf_run_id !== null ? { ctf_run_id } : {}),
      state: { status: 'idle', unread: true },
    });
    const mkSummary = (thread_id) => ({
      thread_id,
      revision: 1,
      status: 'idle',
      running: false,
      unread: true,
      pending_kind: 'none',
      pending_id: null,
      title: `T ${thread_id}`,
      preview: '',
      fingerprint: thread_id,
    });

    assert.equal(isInternalCtfThread(mkThread('chat-blank', '   ')), false);
    assert.equal(isInternalCtfThread(mkThread('chat-tagged', ' run-99 ')), true);
    assert.equal(isInternalCtfThread(mkThread('ctf-pi-run-123')), true);

    const normalIds = Array.from({ length: normalCount }, (_, i) => `chat-init-${i}`);
    const ctfTaggedIds = Array.from({ length: taggedCtfCount }, (_, i) => `tagged-ctf-${i}`);
    const ctfPrefixIds = Array.from({ length: prefixCtfCount }, (_, i) => `ctf-pi-init-${i}`);

    const threads = [
      ...normalIds.map((id) => mkThread(id)),
      ...ctfTaggedIds.map((id, i) => mkThread(id, `run-tag-${i}`)),
      ...ctfPrefixIds.map((id) => mkThread(id)),
    ];
    const summaries = [
      ...normalIds.map((id) => mkSummary(id)),
      ...ctfTaggedIds.map((id) => mkSummary(id)),
      ...ctfPrefixIds.map((id) => mkSummary(id)),
    ];

    const sanitized = sanitizeConversationInboxSnapshot({
      inbox_seq: 1,
      broker_epoch: 'ep1',
      threads,
      summaries,
    });
    assert.deepEqual(Array.from(sanitized.threads, (t) => t.thread_id), normalIds);
    assert.deepEqual(Array.from(sanitized.summaries, (s) => s.thread_id), normalIds);

    const patchNormalIds = Array.from({ length: patchNormalCount }, (_, i) => `chat-patch-${i}`);
    const patchTaggedIds = Array.from({ length: patchCtfCount }, (_, i) => `patch-tagged-${i}`);
    const patchPrefixIds = Array.from({ length: patchCtfCount }, (_, i) => `ctf-pi-patch-${i}`);

    const patched = applyConversationInboxPatch(sanitized, {
      base_seq: 1,
      inbox_seq: 2,
      broker_epoch: 'ep1',
      threads: [
        ...patchNormalIds.map((id) => mkThread(id)),
        ...patchTaggedIds.map((id, i) => mkThread(id, `run-patch-${i}`)),
        ...patchPrefixIds.map((id) => mkThread(id)),
      ],
      summaries: [
        ...patchNormalIds.map((id) => mkSummary(id)),
        ...patchTaggedIds.map((id) => mkSummary(id)),
        ...patchPrefixIds.map((id) => mkSummary(id)),
      ],
      removed_thread_ids: [],
    });

    assert.ok(patched !== null);
    const expectedAfterPatch = [...normalIds, ...patchNormalIds].sort((a, b) => (a < b ? -1 : Number(a > b)));
    assert.deepEqual(Array.from(patched.threads, (t) => t.thread_id), expectedAfterPatch);
    assert.deepEqual(Array.from(patched.summaries, (s) => s.thread_id), expectedAfterPatch);
  },
);

const {
  parseSrcDebugLogsMetadata,
  srcDebugLogsFilename,
  formatSrcDebugLogsSummary,
} = loadTsModule(path.join(frontendRoot, 'lib/srcDebugLogs.ts'));

runProp(
  'srcDebugLogsFilename always emits a safe .tar.gz filename',
  fc.record({
    taskId: fc.string({ minLength: 0, maxLength: 80 }),
    dispositionKind: fc.constantFrom('none', 'utf8', 'ascii', 'malformed', 'traversal'),
    rawName: fc.string({ minLength: 0, maxLength: 60 }),
    epochMs: fc.integer({ min: 0, max: 2_000_000_000_000 }),
  }),
  ({ taskId, dispositionKind, rawName, epochMs }) => {
    let contentDisposition = null;
    if (dispositionKind === 'utf8') {
      contentDisposition = `attachment; filename*=UTF-8''${encodeURIComponent(rawName)}`;
    } else if (dispositionKind === 'ascii') {
      contentDisposition = `attachment; filename="${rawName.replace(/["\r\n]/g, '')}"`;
    } else if (dispositionKind === 'malformed') {
      contentDisposition = `attachment; filename*=UTF-8''%E0%A4%A`;
    } else if (dispositionKind === 'traversal') {
      contentDisposition = `attachment; filename="../../evil\\path:name?.tar.gz"`;
    }
    const name = srcDebugLogsFilename({
      contentDisposition,
      taskId,
      date: new Date(epochMs),
    });
    assert.ok(name.length > '.tar.gz'.length);
    assert.ok(name.endsWith('.tar.gz'));
    assert.equal(/[<>:"/\\|?*\x00-\x1f]/.test(name), false);
    assert.equal(name.includes('..'), false);
  },
);

runProp(
  'parseSrcDebugLogsMetadata validates integer telemetry headers without throwing',
  fc.record({
    schema: fc.string({ minLength: 1, maxLength: 32 }),
    task_id: fc.string({ minLength: 1, maxLength: 32 }),
    src_collection_count: fc.nat(500),
    src_document_count: fc.nat(5000),
    thread_count: fc.nat(200),
    chat_event_count: fc.nat(50000),
    related_document_count: fc.nat(5000),
    included_log_count: fc.nat(10),
  }),
  (meta) => {
    const valid = meta.schema.trim().length > 0 && meta.task_id.trim().length > 0;
    const parsed = parseSrcDebugLogsMetadata(JSON.stringify(meta));
    if (valid) {
      assert.deepEqual(JSON.parse(JSON.stringify(parsed)), JSON.parse(JSON.stringify(meta)));
      const summary = formatSrcDebugLogsSummary({
        filename: 'muteki-src-debug-demo.tar.gz',
        sizeBytes: 2048,
        metadata: parsed,
      });
      assert.ok(summary.includes('muteki-src-debug-demo.tar.gz'));
      assert.ok(summary.includes(String(meta.thread_count)));
      assert.ok(summary.includes(String(meta.chat_event_count)));
    } else {
      assert.equal(parsed, null);
    }
    assert.equal(parseSrcDebugLogsMetadata('{bad-json'), null);
    assert.equal(parseSrcDebugLogsMetadata(JSON.stringify({ ...meta, thread_count: -1 })), null);
    assert.equal(parseSrcDebugLogsMetadata(null), null);
    const fallbackSummary = formatSrcDebugLogsSummary({
      filename: 'fallback.tar.gz',
      sizeBytes: -10,
      metadata: null,
    });
    assert.ok(fallbackSummary.includes('fallback.tar.gz'));
    assert.ok(fallbackSummary.includes('0 B'));
  },
);

const composerUndoTextDoc = (text) => ({ segments: [{ type: 'text', text }], nodes: {} });
const composerUndoSnap = (text, selection) => {
  const doc = composerUndoTextDoc(text);
  return { doc, key: undoSnapshotKey(doc, []), selection, attachments: [], attachmentKeys: [] };
};
const composerUndoSel = (a, b, length) => {
  const clamp = (value) => Math.max(0, Math.min(length, value));
  const start = clamp(Math.min(a, b));
  const end = clamp(Math.max(a, b));
  return { start, end };
};

const composerUndoEditArb = fc.array(
  fc.record({
    kind: fc.constantFrom('type', 'delete', 'atomic'),
    gap: fc.integer({ min: 0, max: 1500 }),
    data: fc.oneof(fc.constantFrom('a', 'b', ' ', 'xy', '你'), fc.constant(null)),
    selMode: fc.constantFrom('contig', 'rand', 'range'),
    selPos: fc.nat({ max: 24 }),
    selPos2: fc.nat({ max: 24 }),
  }),
  { minLength: 1, maxLength: 24 },
);

runProp(
  'composerUndoHistory_random_sequences_undo_to_initial_and_redo_roundtrip',
  composerUndoEditArb,
  (edits) => {
    const clamp = (value, length) => Math.max(0, Math.min(length, value));
    let text = '';
    let selection = { start: 0, end: 0 };
    let at = 0;
    let state = emptyUndoState();
    let current = composerUndoSnap('', selection);
    const initialKey = current.key;
    let atomicSeq = 0;

    for (const edit of edits) {
      at += edit.gap;
      const before = current;
      let caret = text.length;
      if (edit.kind === 'type') {
        const insert = edit.data && edit.data.length ? edit.data : 'x';
        const pos = clamp(selection.start, text.length);
        text = `${text.slice(0, pos)}${insert}${text.slice(pos)}`;
        caret = pos + insert.length;
      } else if (edit.kind === 'delete') {
        if (!text.length) {
          text = 'x';
          caret = 1;
        } else {
          const start = Math.min(selection.start, selection.end);
          const end = Math.max(selection.start, selection.end);
          if (start === end) {
            const pos = clamp(selection.start, text.length);
            const cut = pos > 0 ? pos - 1 : pos;
            text = `${text.slice(0, cut)}${text.slice(pos)}`;
            caret = cut;
          } else {
            text = `${text.slice(0, start)}${text.slice(end)}`;
            caret = start;
          }
        }
      } else {
        atomicSeq += 1;
        text = `${text}#${atomicSeq}`;
        caret = text.length;
      }
      const length = text.length;
      let nextSelection;
      if (edit.selMode === 'contig') {
        nextSelection = { start: caret, end: caret };
      } else if (edit.selMode === 'range') {
        nextSelection = composerUndoSel(edit.selPos, edit.selPos2, length);
      } else {
        const pos = clamp(edit.selPos, length);
        nextSelection = { start: pos, end: pos };
      }
      const after = composerUndoSnap(text, nextSelection);
      state = recordUndo(state, before, after, { kind: edit.kind, at, data: edit.data ?? null });
      current = after;
      selection = nextSelection;
    }

    let drained = state;
    let cursor = current;
    let deepest = null;
    for (;;) {
      const step = undoStep(drained, cursor);
      if (!step) break;
      drained = step.state;
      cursor = step.snapshot;
      deepest = step.snapshot;
    }
    assert.ok(deepest, 'expected at least one undo step');
    assert.equal(deepest.key, initialKey, 'undoing everything must return to the initial snapshot');

    const undone = undoStep(state, current);
    if (undone) {
      const redone = redoStep(undone.state, undone.snapshot);
      assert.ok(redone, 'redo after undo must succeed');
      assert.equal(redone.snapshot.key, current.key, 'redo must return the pre-undo snapshot');
      const beforeNew = composerUndoSnap(text, selection);
      const afterNew = composerUndoSnap(`${text}!`, { start: 0, end: 0 });
      const withNew = recordUndo(undone.state, beforeNew, afterNew, { kind: 'atomic', at: at + 10_000 });
      assert.equal(withNew.redo.length, 0, 'recording a new edit must clear the redo stack');
    }

    assert.ok(state.undo.length <= UNDO_MAX_DEPTH, 'undo stack must respect UNDO_MAX_DEPTH');
    assert.ok(state.redo.length <= UNDO_MAX_DEPTH + edits.length, 'redo stack must stay bounded by recorded steps');
  },
);

runProp(
  'composerUndoHistory_depth_capped_at_max',
  fc.integer({ min: UNDO_MAX_DEPTH + 1, max: UNDO_MAX_DEPTH + 40 }),
  (count) => {
    let state = emptyUndoState();
    let text = '';
    for (let index = 0; index < count; index += 1) {
      const before = composerUndoSnap(text, { start: 0, end: 0 });
      text = `${text}x${index}`;
      const after = composerUndoSnap(text, { start: 0, end: 0 });
      state = recordUndo(state, before, after, { kind: 'atomic', at: index });
    }
    assert.equal(state.undo.length, UNDO_MAX_DEPTH);
    assert.equal(state.redo.length, 0);
  },
);

runProp(
  'composerUndoHistory_atomic_edits_undo_exactly_n_times',
  fc.integer({ min: 1, max: 12 }),
  (count) => {
    let state = emptyUndoState();
    let text = '';
    let current = composerUndoSnap('', { start: 0, end: 0 });
    for (let index = 0; index < count; index += 1) {
      const before = current;
      text = `${text}#${index}`;
      const after = composerUndoSnap(text, { start: 0, end: 0 });
      state = recordUndo(state, before, after, { kind: 'atomic', at: index * 10 });
      current = after;
    }
    const initialKey = composerUndoSnap('', { start: 0, end: 0 }).key;
    let undos = 0;
    let cursor = current;
    for (;;) {
      const step = undoStep(state, cursor);
      if (!step) break;
      undos += 1;
      state = step.state;
      cursor = step.snapshot;
    }
    assert.equal(undos, count, 'N atomic edits must undo exactly N times');
    assert.equal(cursor.key, initialKey);
  },
);

const composerUndoTypeRun = (chars, gaps) => {
  let state = emptyUndoState();
  let current = composerUndoSnap('', { start: 0, end: 0 });
  let at = 0;
  for (let index = 0; index < chars.length; index += 1) {
    at += gaps[index] ?? 100;
    const before = current;
    const caret = index + 1;
    const after = composerUndoSnap(chars.slice(0, caret), { start: caret, end: caret });
    state = recordUndo(state, before, after, { kind: 'type', at, data: chars[index] });
    current = after;
  }
  return { state, current };
};

runProp(
  'composerUndoHistory_word_boundary_and_pause_split_groups',
  fc.constant(null),
  () => {
    const word = 'hello world';
    const tight = Array.from({ length: word.length }, () => 100);
    const baseline = composerUndoTypeRun(word, tight);
    assert.equal(baseline.state.undo.length, 2, '"hello world" typed quickly must produce two undo entries');

    const paused = [...tight];
    paused[2] = UNDO_GROUP_DELAY_MS + 1;
    const split = composerUndoTypeRun(word, paused);
    assert.equal(split.state.undo.length, 3, 'a pause mid-word must add one undo entry');

    const edge = composerUndoTypeRun('ab', [0, UNDO_GROUP_DELAY_MS]);
    assert.equal(edge.state.undo.length, 1, 'gap equal to the group delay must still merge');
    const over = composerUndoTypeRun('ab', [0, UNDO_GROUP_DELAY_MS + 1]);
    assert.equal(over.state.undo.length, 2, 'gap beyond the group delay must split');

    const grouped = composerUndoTypeRun('ab', [0, 100]);
    assert.notEqual(grouped.state.open, null);
    assert.equal(sealUndo(grouped.state).open, null, 'sealUndo must close the open group');
  },
);

runProp(
  'composerUndoHistory_chip_snapshot_preserves_nodes',
  fc.constant(null),
  () => {
    const nodes = {
      n1: {
        node_id: 'n1',
        kind: 'file',
        id: 'file-1',
        name: 'a.ts',
        locator: {},
        snapshot: { label: 'a.ts', text: 'content', captured_at: '2025-01-01T00:00:00Z' },
        status: 'ok',
        description: '',
        source: '',
        scope: '',
      },
    };
    const chipDoc = {
      segments: [
        { type: 'text', text: 'see ' },
        { type: 'ref', nodeId: 'n1' },
        { type: 'text', text: ' please' },
      ],
      nodes,
    };
    const before = {
      doc: chipDoc,
      key: undoSnapshotKey(chipDoc, []),
      selection: { start: 0, end: 0 },
      attachments: [],
      attachmentKeys: [],
    };
    const afterDoc = composerUndoTextDoc('see  please');
    const after = {
      doc: afterDoc,
      key: undoSnapshotKey(afterDoc, []),
      selection: { start: 8, end: 8 },
      attachments: [],
      attachmentKeys: [],
    };
    const state = recordUndo(emptyUndoState(), before, after, { kind: 'atomic', at: 1 });
    const step = undoStep(state, after);
    assert.ok(step);
    assert.equal(step.snapshot.key, before.key);
    assert.ok(step.snapshot.doc.segments.some((segment) => segment.type === 'ref'), 'chip segment must survive undo');
    assert.deepEqual(step.snapshot.doc.nodes, nodes, 'snapshot must preserve node records');
  },
);

runProp(
  'composerUndoHistory_classifyInputType_mapping',
  fc.constant(null),
  () => {
    assert.equal(classifyInputType('historyUndo', null), 'undo');
    assert.equal(classifyInputType('historyRedo', null), 'redo');
    assert.equal(classifyInputType('insertText', 'a'), 'type');
    assert.equal(classifyInputType('insertText', null), 'type');
    assert.equal(classifyInputType('insertText', 'a\nb'), 'atomic');
    assert.equal(classifyInputType('deleteContentBackward', null), 'delete');
    assert.equal(classifyInputType('deleteContentForward', null), 'delete');
    assert.equal(classifyInputType('insertCompositionText', 'a'), 'ignore');
    assert.equal(classifyInputType('deleteCompositionText', null), 'ignore');
    assert.equal(classifyInputType('insertFromComposition', null), 'ignore');
    assert.equal(classifyInputType('insertFromPaste', 'x'), 'atomic');
    assert.equal(classifyInputType('insertParagraph', null), 'atomic');
    assert.equal(classifyInputType('insertLineBreak', null), 'atomic');
    assert.equal(classifyInputType('deleteWordBackward', null), 'atomic');
    assert.equal(classifyInputType('deleteByCut', null), 'atomic');
    const keyed = undoSnapshotKey(composerUndoTextDoc('hi'), ['x', 'y']);
    assert.ok(keyed.startsWith('hi'));
    assert.ok(keyed.includes('\u0000'));
    assert.ok(keyed.includes('x\u0001y'));
  },
);

runProp(
  'hitl_partition_and_grouping_preserve_all_decisions',
  fc.uniqueArray(
    fc.record({
      id: idArb,
      prompt: fc.oneof(
        fc.constant('Need scope for https://example.com/auth/login and https://example.com/api/v1/auth/login'),
        fc.constant('Confirm access to https://example.com/api/v1/users/profile'),
        fc.string({ minLength: 1, maxLength: 60 }),
      ),
      worker: fc.option(fc.constantFrom('cli-omp#1', 'cli-claude#2', 'cli-codex#3'), { nil: undefined }),
      options: fc.array(fc.string({ minLength: 1, maxLength: 12 }), { maxLength: 3 }),
      needKind: fc.option(fc.constantFrom('external_blocker', 'lane_lock_request', 'route_dead_end', 'worker_uncertainty'), { nil: undefined }),
      pausesBehavior: fc.option(fc.boolean(), { nil: undefined }),
      deliveryCommandId: fc.option(idArb, { nil: undefined }),
      deliveryStatus: fc.option(fc.constantFrom('received', 'persisted', 'routed', 'partial', 'failed', 'unknown', 'rejected'), { nil: undefined }),
      ts: fc.integer({ min: 1, max: 100000 }),
    }),
    { selector: (r) => r.id, maxLength: 12 },
  ),
  (requests) => {
    const part = partitionHitlDeck(requests, []);
    assert.equal(part.pendingCount, part.pending.length);
    assert.equal(part.recoveringCount, part.recovering.length);
    assert.equal(part.pending.length + part.recovering.length + part.auto.length, requests.length);
    const seen = new Set([
      ...part.pending.map((r) => r.id),
      ...part.recovering.map((r) => r.id),
      ...part.auto.map((r) => r.id),
    ]);
    assert.equal(seen.size, requests.length);
    const pendingFlat = [...part.pendingGroups.flatMap((g) => g.items.map((r) => r.id))];
    const recoveringFlat = [...part.recoveringGroups.flatMap((g) => g.items.map((r) => r.id))];
    assert.deepEqual(pendingFlat.sort(), [...part.pending.map((r) => r.id)].sort());
    assert.deepEqual(recoveringFlat.sort(), [...part.recovering.map((r) => r.id)].sort());

    const sample = [
      {
        id: 'h1',
        worker: 'omp-1',
        prompt: '仅限 https://www.cloud.yishuit.cn/auth/login，请确认是否包含 https://www.cloud.yishuit.cn/api/v1/auth/login。',
        options: [],
        needKind: 'external_blocker',
        pausesBehavior: true,
        ts: 10,
      },
      {
        id: 'h2',
        worker: 'claude-2',
        prompt: '当前仅授权 https://www.cloud.yishuit.cn/auth/login；请确认 Step 是否允许 https://www.cloud.yishuit.cn/api/v1/auth/login/ 核验。',
        options: [],
        needKind: 'external_blocker',
        pausesBehavior: true,
        ts: 20,
      },
      {
        id: 'h3',
        worker: 'omp-1',
        prompt: 'authorizedScope 仅限 https://www.cloud.yishuit.cn/auth/login，需 POST https://www.cloud.yishuit.cn/api/v1/auth/login',
        options: [],
        needKind: 'external_blocker',
        pausesBehavior: true,
        ts: 30,
      },
      {
        id: 'h4',
        worker: 'codex-1',
        prompt: '需确认 https://www.cloud.yishuit.cn/api/v1/session/refresh 是否授权。',
        options: [],
        needKind: 'external_blocker',
        pausesBehavior: true,
        ts: 40,
      },
    ];
    const grouped = groupHitlRequests(sample);
    assert.equal(grouped.length, 2);
    assert.equal(grouped[0].items.length, 3);
    assert.equal(grouped[0].topic.label, '/api/v1/auth/login');
    assert.deepEqual([...grouped[0].workers], ['omp-1', 'claude-2']);
    assert.equal(grouped[1].items.length, 1);
    assert.equal(grouped[1].topic.label, '/api/v1/session/refresh');

    const routed = groupHitlRequests([
      { ...sample[0], id: 'rh-1', routeHash: 'rh-auth-login' },
      { ...sample[1], id: 'rh-2', routeHash: 'rh-auth-login' },
      { id: 'rh-3', prompt: 'missing credentials', routeHash: 'rh-opaque', options: [], ts: 50 },
    ]);
    assert.equal(routed.length, 2);
    assert.equal(routed[0].key, 'external_blocker|route:rh-auth-login');
    assert.equal(routed[0].topic.label, '/api/v1/auth/login');
    assert.equal(routed[0].items.length, 2);
    assert.equal(routed[1].topic.label, 'rh-opaque');
  },
);

runProp(
  'hitl_lifecycle_replay_and_resolution_invariants',
  fc.constant(null),
  () => {
    let seq = 1;
    let state = emptyDeck('run-hitl-prop');
    const apply = (event_type, payload, solver_id = null) => {
      state = reduce(state, {
        event_type,
        seq: seq++,
        ts: 1000 + seq,
        run_id: 'run-hitl-prop',
        solver_id,
        payload,
      });
    };

    apply(EventType.RUN_STARTED, { challenge: { name: 'pentest', mode: 'pentest' } });
    apply(EventType.HITL_REQUEST, { request_id: 'req-a', worker: 'w1', need: 'Need scope A', need_kind: 'external_blocker' }, 'w1');
    apply(EventType.HITL_TRANSLATED, { worker: 'w1', need: 'Need scope A', need_zh: '需要作用域 A' });
    apply(EventType.HITL_REQUEST, { request_id: 'req-b', worker: 'w2', need: 'Need scope B', need_kind: 'external_blocker' }, 'w2');
    apply(EventType.HITL_REQUEST, { request_id: 'req-c', worker: 'w3', need: 'Need scope C', need_kind: 'external_blocker' }, 'w3');
    assert.equal(state.awaitingOperator, 'Need scope A');
    assert.equal(state.hitlRequests.length, 3);

    apply(EventType.CONTROL_COMMAND, {
      command_id: 'cmd-a',
      action: 'answer_decision',
      request_id: 'req-a',
      decision_closed: false,
      status: 'partial',
      detail: '1/2 delivered',
    });
    assert.equal(state.hitlRequests.length, 3);
    assert.equal(classifyHitlRequest(state.hitlRequests.find((r) => r.id === 'req-a')), 'recovering');
    assert.equal(state.hitlRequests.find((r) => r.id === 'req-a').deliveryStatus, 'partial');
    assert.equal(state.awaitingOperator, 'Need scope B');

    apply(EventType.CONTROL_COMMAND, {
      command_id: 'cmd-b',
      action: 'answer_decision',
      request_id: 'req-b',
      decision_closed: false,
      status: 'unknown',
      effect: { kind: 'decision_delivery_unknown', detail: 'stdin unconfirmed' },
    });
    assert.equal(classifyHitlRequest(state.hitlRequests.find((r) => r.id === 'req-b')), 'recovering');
    assert.equal(state.hitlRequests.find((r) => r.id === 'req-b').deliveryStatus, 'unknown');
    assert.equal(state.awaitingOperator, 'Need scope C');

    apply(EventType.HITL_RESOLVED, {
      command_id: 'cmd-a',
      request_ids: ['req-a'],
      action: 'answer_decision',
      delivery: 'delivered',
    });
    apply(EventType.CONTROL_COMMAND, {
      command_id: 'cmd-a',
      action: 'answer_decision',
      request_id: 'req-a',
      decision_closed: true,
      status: 'effect_observed',
    });
    assert.equal(state.hitlRequests.some((r) => r.id === 'req-a'), false);
    assert.equal(state.hitlHistory.filter((h) => h.id === 'req-a').length, 1);
    const archivedA = state.hitlHistory.find((h) => h.id === 'req-a');
    assert.equal(archivedA.promptZh, '需要作用域 A');
    assert.equal(archivedA.deliveryMode, 'delivered');
    assert.equal(archivedA.deliveryCommandId, 'cmd-a');

    apply(EventType.CONTROL_COMMAND, {
      command_id: 'cmd-b',
      action: 'answer_decision',
      request_id: 'req-b',
      decision_closed: true,
      status: 'effect_observed',
    });
    apply(EventType.HITL_RESOLVED, {
      command_id: 'cmd-b',
      request_ids: ['req-b'],
      action: 'answer_decision',
      delivery: 'queued',
      continuation_intent_id: 'I-control-cmd-b',
    });
    assert.equal(state.hitlHistory.filter((h) => h.id === 'req-b').length, 1);
    const archivedB = state.hitlHistory.find((h) => h.id === 'req-b');
    assert.equal(archivedB.deliveryMode, 'queued');
    assert.equal(archivedB.continuationIntentId, 'I-control-cmd-b');
    assert.equal(state.awaitingOperator, 'Need scope C');

    apply(EventType.RUN_FINISHED, { reason: 'finished', solved: false });
    assert.equal(state.hitlRequests.length, 0);
    assert.equal(state.awaitingOperator, undefined);
    assert.equal(state.hitlHistory.length, 3);
    assert.equal(state.hitlHistory.find((h) => h.id === 'req-c').resolutionAction, 'run_finished');
  },
);

runProp(
  'hitl_duplicate_request_preserves_delivery_lock',
  fc.constant(null),
  () => {
    let state = emptyDeck('run-hitl-dup');
    state = reduce(state, {
      event_type: EventType.HITL_REQUEST,
      seq: 1,
      ts: 100,
      run_id: 'run-hitl-dup',
      solver_id: 'w1',
      payload: { request_id: 'req-dup', worker: 'w1', need: 'Need token', need_kind: 'external_blocker' },
    });
    state = reduce(state, {
      event_type: EventType.CONTROL_COMMAND,
      seq: 2,
      ts: 101,
      run_id: 'run-hitl-dup',
      payload: {
        command_id: 'cmd-dup',
        action: 'answer_decision',
        request_id: 'req-dup',
        decision_closed: true,
        status: 'partial',
      },
    });
    state = reduce(state, {
      event_type: EventType.HITL_REQUEST,
      seq: 3,
      ts: 102,
      run_id: 'run-hitl-dup',
      solver_id: 'w1',
      payload: { request_id: 'req-dup', worker: 'w1', need: 'Need token', need_kind: 'external_blocker' },
    });
    assert.equal(state.hitlRequests.length, 1);
    assert.equal(state.hitlRequests[0].deliveryStatus, 'partial');
    assert.equal(state.hitlRequests[0].deliveryCommandId, 'cmd-dup');
    assert.equal(classifyHitlRequest(state.hitlRequests[0]), 'recovering');
  },
);

const { randomUuid, sha256Hex, sha256HexSync } = loadTsModule(path.join(frontendRoot, 'lib/browserCrypto.ts'));

runProp(
  'browserCrypto_randomUuid_works_without_secure_context',
  fc.constantFrom(undefined, {}, { getRandomValues: (arr) => { for (let i = 0; i < arr.length; i += 1) arr[i] = (i * 37 + 11) & 0xff; return arr; } }),
  (mockCrypto) => {
    const id = randomUuid(mockCrypto);
    assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  },
);

await fc.assert(
  fc.asyncProperty(
    fc.oneof(
      fc.constantFrom('', 'abc', 'a'.repeat(55), 'a'.repeat(56), 'a'.repeat(63), 'a'.repeat(64), 'a'.repeat(65), '中文报告内容🚀\n## Heading'),
      fc.string({ maxLength: 512 }),
    ),
    async (text) => {
      const expected = createHash('sha256').update(text, 'utf8').digest('hex');
      assert.equal(sha256HexSync(new TextEncoder().encode(text)), expected);
      assert.equal(await sha256Hex(text, {}), expected);
      assert.equal(await sha256Hex(text, undefined), expected);
    },
  ),
  { numRuns },
);
console.log(`PASS  [frontend-prop] browserCrypto_sha256_matches_node_without_subtle (${numRuns} runs)`);

const { createFolderArrangeQueue } = loadTsModule(path.join(frontendRoot, 'lib/folderArrangeQueue.ts'));

await fc.assert(
  fc.asyncProperty(
    fc.array(
      fc.record({
        sections: fc.array(fc.constantFrom('active', 'recent'), { minLength: 1, maxLength: 4 }),
        reverseOrder: fc.boolean(),
      }),
      { minLength: 2, maxLength: 6 },
    ),
    async (steps) => {
      const ids = ['f1', 'f2', 'f3', 'f4'];
      const initial = ids.map((id, idx) => ({ id, section: 'recent', order: idx + 1 }));
      const server = new Map(initial.map((item) => [item.id, { ...item }]));
      let staleProp = initial;
      let activeCalls = 0;
      let settledCount = 0;
      const resolvers = [];
      const queue = createFolderArrangeQueue(
        () => staleProp,
        (id, patch) => {
          activeCalls += 1;
          assert.equal(activeCalls, 1, 'folder PATCH requests must be strictly serialized');
          return new Promise((resolve) => {
            resolvers.push(() => {
              const cur = server.get(id);
              assert.ok(cur);
              server.set(id, {
                id,
                section: patch.section ?? cur.section,
                order: patch.order ?? cur.order,
              });
              activeCalls -= 1;
              resolve(true);
            });
          });
        },
        () => {
          settledCount += 1;
        },
      );

      const desiredTargets = steps.map((step) => {
        const ordered = step.reverseOrder ? [...ids].reverse() : [...ids];
        return ordered.map((id, idx) => ({ id, section: step.sections[idx % step.sections.length] }));
      });
      const backToInitial = initial.map((item) => ({ id: item.id, section: item.section }));
      const allTargets = [...desiredTargets, backToInitial];

      const pending = allTargets.map((targets) => queue(targets));
      await new Promise(setImmediate);
      while (resolvers.length > 0) {
        resolvers.shift()();
        await new Promise(setImmediate);
      }
      const results = await Promise.all(pending);
      assert.ok(results.every(Boolean));
      assert.equal(settledCount, 1);

      const expected = allTargets[allTargets.length - 1];
      for (const [index, target] of expected.entries()) {
        const actual = server.get(target.id);
        assert.equal(actual.section, target.section);
        assert.equal(actual.order, index + 1);
      }
    },
  ),
  { numRuns },
);
console.log(`PASS  [frontend-prop] folderArrangeQueue_serializes_rapid_moves_and_reconciles_baseline (${numRuns} runs)`);

console.log('ALL FRONTEND PROPERTY SUITES PASSED');
