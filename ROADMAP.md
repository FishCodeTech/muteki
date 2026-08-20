# Project Muteki — Roadmap

> Rewritten 2026-07-05 to reflect the current architecture. The previous roadmap
> predated the CLI-only executor, the multi-flag work, pentest mode, and the
> current web deck, so it was archived. For the authoritative "what this is" and
> "how to run it", see [`README.md`](README.md) and [`AGENTS.md`](AGENTS.md).

> **Current execution scope:** the public near-term list is in the README
> Roadmap / TODO section. Cognitive studies, new swarm-core mechanisms, managed
> OAuth, strict shell-level offline isolation, credential-projection narrowing,
> TUI/platform automation, and other public feature expansion are currently
> deferred. The sections below retain the architecture history and long-term
> evidence requirements; they do not by themselves authorize implementation.

## Active architecture upgrade (foundation complete; cognitive research loop default-off)

The durable objective for future work is measurable cognition:
better thinking, search, learning, heterogeneous collaboration, local recovery and
hard long-chain reliability under the same complete budget.

Protocol 2 is a restricted live-local canary: single profile, local worker, agent
web tools off, no knowledge base, and a finite budget. Ordinary Web runs stay on
Protocol 1. Expanding Protocol 2 is currently deferred. The canary is implemented
through the hardened `S4-E` foundation: V1 loop containment; canonical ports and
identities; atomic append-only command/event/fold/CAS; hard-gate receipts;
per-attempt admission, budget, UNKNOWN and supervision; archive/purge/tombstone;
enforced live-local egress; and branch-scoped progress/search admission. The
worktree-bound `run-76191` release chain recorded 19 canonical events and 16
receipts, replayed to the same checksum/projection, and set this canary path's
`production_enabled=true`. That flag applies only to the restricted canary.
Protocol 1 runs are still drained rather than hot-switched.

The existing provenance gate remains hardcoded and its accepted set does not widen.
Above that foundation, the default-off canonical chain now joins exact assignment and
materialization with a preregistered distinct reproduction, separately admitted and
accounted deterministic checker, and receipt-only verification resolver. The resolver
rebuilds lineage from the complete prefix; checker/resolver capabilities cannot cross,
and real `host_popen` evidence remains `HELD_UNKNOWN` while containment is unproven. A
pure bridge can project resolver-owned facts into belief and recommend one next
experiment. Exact selection binding is now implemented default-off: the same
transaction resolves the complete pre-admission prefix, reruns the planner and binds
the exact `next_assignment`. The resolver-fact inventory is store-owned; candidate,
future-cost and remaining-scalar-budget inputs remain frozen caller proposals and are
labelled as such.

The Reason/coordinator bridge remains an offline adapter over frozen ordinary intents;
the production coordinator has no research import, switch or second annotation call.
The simple V1 planner is the current research baseline. V2 remains killed. V3's old
8/16 planning-family result is no longer interpretable as eight terminal failures:
independent enumeration found outcome-correct, cheaper paths rejected only for using a
different evaluator-authored role name. This correction does not promote V3. A new
bounded terminal-completion policy then entered development against outcome-equivalent
V1 and simple ablations, with byte-identical V1 fallback and deletion on no strict gain.
V4 and its reliability-tail V5 revision both failed that rule: V4 has eight
terminal-control tail losses. V5 produced no aggregate gain and retained two losses;
independent review then disproved its claimed exact dynamic program with a valid case
where removing candidates improved the alleged optimum. V5 policy/evaluator/tests were
deleted rather than repaired. This exposed bank is closed to further policy tuning.
Profile evidence is non-steering, supplied recovery claims are not
canonical evidence, and UNKNOWN is never an automatic retry. Any canary still requires
source-disjoint tasks, complete multi-axis ledger binding, paired hard/long-chain
results and a separate release decision.

## Where we are

Muteki is a heterogeneous, multi-model **CTF-solving agent swarm**. The current
architecture is built and verified:

- **Worker executor = a shelled full-model CLI agent.** Each worker is a real
  Claude, Codex, Cursor, Pi, OMP, Kimi, Grok, OpenCode, or DeepSeek Harness
  process running its own agentic shell loop;
  Muteki orchestrates these CLIs rather than re-implementing a model loop
  (`muteki/solver/cli_driver.py`, `muteki/solver/cli_solver.py`).
- **Shared, event-sourced evidence graph.** The swarm collaborates on one
  append-only blackboard (`muteki/swarm/shared_graph.py`); workers read/write it
  through the `muteki-blackboard` skill. An independent **Reason phase**
  (`muteki/solver/reason.py`) plans typed intents from the graph.
- **Provenance gate.** A flag is accepted only when it appears verbatim in real
  execution output (`muteki/solver/gate.py` + anti-laundering checks in
  `cli_solver.py`). Zero false flags is the correctness bar.
- **Web uses the coordinator by default.** The web path enables the coordinator
  and may run an initial race-scout round. The explicit TUI `--swarm` path uses a
  direct race. Callers that construct `Swarm` directly must select the intended
  mode explicitly.
- **Multi-flag.** `Challenge.expected_flags` (default 1). `expected_flags=1` is
  byte-identical to "first flag wins"; only `>1` engages the multi-flag paths.
- **Engagement modes.** `mode="ctf"` (recover a flag; completion = the provenance
  gate) and `mode="pentest"` (operator-defined goal + scope; completion is
  goal-driven, findings kept honest by the same witness gate).
- **Frontends are dumb bus subscribers.** `apps/web/` (FastAPI SSE/WS backend +
  Next.js command deck) and `apps/tui/` (Textual) render the event stream and
  never call the solver core directly.
- **Runtime backends.** Workers run as host CLIs (`local`) or as sibling
  containers driven by an in-container Go supervisor over a reverse connection
  (`container` / RCP; `cmd/runtime-agent/`, `muteki/solver/container_exec.py`).

**Evaluation.** Full NYU CTF Bench `test` set (200 challenges): 200/200 solved in
the capability evaluation. See [`eval_nyu/_reports/`](eval_nyu/_reports/) and the
Evaluation section of the README. Treat this as a capability snapshot, not a
leaderboard verdict.

## Recorded cognitive direction (currently deferred)

The route is sequential, capability-first, and falsifiable:

1. **Done — canonical assignment/structural terminal:** exact cognitive assignment,
   structural execution observation, composite capability, atomic store commit,
   replay and UNKNOWN/no-redispatch are implemented. Structural execution is not a
   verified partition and cannot update belief.
2. **Done — exact runtime materialization:** one admitted experiment travels through
   the same ContextPacket, prompt CAS and host launch. The resolver reports
   `HOST_LAUNCH_ONLY`; planner selection and child/provider consumption stay false.
3. **Done — independent verification mechanics:** exact source/reproduction lineage,
   deterministic checker accounting and resolver-owned fact writing are implemented.
   UNKNOWN/disagreement/ineligible/uncontained evidence cannot learn or redispatch.
4. **Done — belief recommendation controls initial admission:** rerun the pinned canonical
   planner under the pre-admission store transaction and require full equality between
   its `next_assignment` and the exact admitted assignment. Merely H5-eligible
   substitution, stale prefix, omitted fact, changed cost/budget or reused selection
   must roll back atomically.
5. **Done default-off — distinct-experiment UNKNOWN recovery:** preserve every historical hold,
   cost and attempted-program fingerprint, but allow the recommendation layer to choose
   one affordable positive-distinction typed program that is not the held program or an
   ID/version alias. A versioned atomic transaction now reconstructs the canonical
   prefix and admits exactly that selected continuation under a new ContextPacket. This
   is a new experiment, never an automatic redispatch; the old hold is not erased.
   Mixed or contradictory holds remain blocked. A later supersession receipt is
   deferred until an exact independently verified successor identity actually exists.
6. **Done negative shadow policy round — reliability before mean score:** V4's exact
   terminal-completion search beats all controls in aggregate on its exposed bank but
   loses eight changed-action nuisance-orbit members to terminal-greedy, so V4 is not
   selectable. V5's public-only tail-risk objective ties V4 at `101/160`, has an even
   `8/8` changed-comparison split against V4, and still loses two changed-action orbit
   members to terminal-greedy. Its one-best-child memoization also violates optimal
   substructure under ancestor `max` objectives, so V5 is killed and deleted; no
   holdout is opened. Exact-score ties
   are now invariant to experiment/hypothesis/partition labels; indistinguishable causal
   representations yield a no-assignment `TIE_REQUIRES_DIVERSITY` state.
7. **Required evidence — source-disjoint offline capability study:** freeze the
   production coordinator baseline and the simplest surviving cognitive arm under a
   paired complete multi-axis budget, fresh holdout, hard/long-chain subgroups,
   confidence intervals, fault injection and leave-one-feature-out ablations. Reuse the
   existing evaluator plane; synthetic scores cannot substitute for this study.
8. **Only after a passing study — separate canary decision:** an operator and
   worktree-bound release receipt decide whether a proven recommender may influence
   admission. A study reducer cannot enable production.
9. **One optional evolution candidate at a time:** memory, heterogeneous router,
   diversity portfolio, dynamic DAG, workflow optimizer or protocol adapter enters
   shadow independently and is deleted when its paired interval or ablation fails.

Unrelated process-mechanics expansion, framework accumulation, and added agent count
are not on the active route.

## Recorded product backlog and disposition

The previous product backlog is retained here with its current disposition.
The public near-term list is in the README Roadmap / TODO section.

1. **Candidate, requires product-priority confirmation — Pentest full engagement.**
   `mode="pentest"` shares the swarm,
   blackboard, and provenance gate with CTF, but the Explore / Review / report
   paths and scope handling are still CTF-shaped. Bring them to a full
   engagement flow: goal/scope-aware exploration and review, a findings report
   (attack path + per-finding impact/repro/evidence/remediation + severity), a
   scope authorization guard, and structured engagement artifacts derived from
   the existing event graph.
2. **Done — Operator intervention and observability control plane.** Typed
   operator commands, scoped delivery, pause/resume/stop, worker control and
   auditable effect state are implemented. Further UI acceptance belongs to
   ordinary product verification rather than a swarm-core redesign.
3. **Done — `swarm.py` and `shared_graph.py` responsibility split.** Further
   module cleanup should happen only alongside an approved feature or a
   reproduced problem.

Authentication and nine-engine support are implemented. The remaining public
feature expansion, including TUI integration and generic CTF-platform automation,
is currently deferred; see the README Roadmap / TODO section for the public list.

## Invariants (never traded away)

- **Provenance is sacred.** A flag/finding is valid only if it appears in real
  stdout/stderr/artifact output. `_flag_ok` / the provenance gate is never
  weakened to make a test or eval pass.
- **Flag acceptance stays a hardcoded, separate gate** — never a pluggable
  verifier.
- **The evidence graph is append-only** — event-sourced, never overwritten in
  place.
- **Don't touch the substrate** unless explicitly asked: the event spine,
  provenance gate, first-valid-flag race, cost ledger, and shared evidence graph.
- **Capability eval is offline** (deny WebSearch/WebFetch) so a solve-rate run
  can't be contaminated by a challenge writeup; a real competition keeps the web
  on.

## Explicitly not planned

- A pluggable / configurable flag verifier (the gate stays hardcoded).
- microVM/Firecracker-style sandboxing — a threat-model concern, not a solve-rate
  lever; documented as a known limitation of the trusted-environment model.
- A second benchmark harness as a headline goal — revisit only as a
  generalization check, not a near-term deliverable.
