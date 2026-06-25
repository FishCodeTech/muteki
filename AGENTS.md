# AGENTS.md — Project Muteki (無敵)

Autonomous multi-model **CTF-solving agent swarm**. This file is the routing map
and the invariants — facts live in the code and in `README.md` / `README_CN.md`.

## What this is (the 60-second model)

- **Worker executor = a shelled full-model CLI agent** (`claude` / `codex`) running
  its own agentic shell loop. Muteki orchestrates these CLIs; it does not re-implement
  a model loop. Driver: `muteki/solver/cli_driver.py`; swarm-facing solver:
  `muteki/solver/cli_solver.py`.
- **A flag is accepted only if it traces to real execution output.** The provenance
  gate is `muteki/solver/gate.py` (`_flag_ok` + anti-laundering checks in
  `cli_solver.py`). This is the project's core correctness guarantee.
- **The swarm shares one event-sourced evidence graph** (`muteki/swarm/shared_graph.py`,
  append-only). An independent **Reason phase** (`muteki/solver/reason.py`) reads the
  graph and proposes typed intents for workers to claim.
- **Race vs coordinator.** Default path is a heterogeneous **race** (`cli_race=True`:
  claude + codex attack the same challenge, first past the gate wins). An opt-in
  **coordinator** (`Swarm(coordinator=True)`) instead plans intents from the graph and
  dispatches focused workers.
- **Multi-flag.** `Challenge.expected_flags` (default 1). `expected_flags=1` is
  byte-identical to "first flag wins"; only `>1` engages the multi-flag paths. Until
  `Swarm._flags_complete()`, a flag is not a stop signal.
- **Frontends are dumb bus subscribers** — `apps/web/` (FastAPI+SSE+Next.js) and
  `apps/tui/` (Textual) render the event stream and never call the solver core directly.

## Startup workflow (every session, before writing code)

1. **`./init.sh`** — installs deps via `uv` and runs the fast test suite. Must be green
   before you start. (Equivalent: `uv run pytest -q`.)
2. Skim `README.md` for what the project is and how to run it; `ROADMAP.md` for direction.
3. If the local working-state files exist (`session-handoff.md`, `progress.md`,
   `feature_list.json` — all git-ignored, not part of the distribution), read them for
   the last session's focus, the active feature, and the next step. Absence is normal on
   a fresh clone; create them from the templates if you want to track multi-session work.

## Working rules (invariants — do not violate)

- **Provenance is sacred.** A flag is valid only if it appears in real
  stdout/stderr/artifact output. Never weaken `_flag_ok` / the provenance gate to make
  a test or eval pass. Zero false flags is the bar — protect it.
- **Flag acceptance stays hardcoded.** It must never become a pluggable verifier — it
  stays the separate hardcoded gate (`muteki/solver/gate.py` + the anti-laundering
  checks in `cli_solver.py`).
- **Black-box eval.** The solver must NOT see challenge source/solution — only the live
  target + description + player-facing `files`. Exception: code-review challenges where
  the source IS the intended input. Never feed a `solution.*` / reference solver.
- **Capability eval must be offline.** A full-strength online worker can web-search the
  challenge writeup, which contaminates a capability measurement. Use the offline mode
  (deny WebSearch/WebFetch) for any solve-rate run; online "solves" don't count toward
  solve-rate. (A real competition keeps the web on — the flag is the flag.)
- **The evidence graph is append-only.** Never make `shared_graph` overwrite in place;
  it is an event-sourced log.
- **Don't touch the substrate** unless asked: the event spine, provenance gate,
  first-valid-flag race, cost ledger, and the shared evidence graph.
- **One feature at a time.** Finish and verify before starting the next; stay in scope —
  don't expand into adjacent refactors without asking.
- **Secrets come from the environment** (e.g. `MUTEKI_DEEPSEEK_API_KEY`). Entrypoints
  auto-load a repo-root `.env` via `muteki/core/dotenv_boot.py`, but `.env` is git-ignored
  (only `.env.example` is tracked) and a shell-exported var always wins. Never commit a
  real key or token.
- **Commit only test-backed work.** Working on `main` is fine for this repo.
- **Publishing:** if a `RELEASING.md` exists in this checkout, follow it for any public
  release — do not invent your own publish path or add new remotes. (On a plain clone
  there is none, and nothing special is needed.)

## Verification & definition of done

Primary check: **`./init.sh`** (or `uv run pytest -q`). A change is done only when:

- [ ] `uv run pytest -q` is green — no regressions. (Unit + scripted-loop tests run
      without an API key via the `ScriptedLLM` pattern; live tests skip without a key.)
- [ ] New behavior has a deterministic test.
- [ ] A solve-rate claim is backed by a real black-box trace showing the flag in actual
      worker output — not a model's claim.

> The pwn SDK tests are optional (need pwntools): `MUTEKI_RUN_PWN_TESTS=1 ./init.sh`.

## Where things are

| Area | Path |
|------|------|
| Worker executor + cognitive core (shelled CLI: driver + CliSolver) | `muteki/solver/cli_driver.py`, `muteki/solver/cli_solver.py` |
| Flag gate (provenance, placeholder/laundering rejects) | `muteki/solver/gate.py` |
| Reason phase (planner + evidence audit) | `muteki/solver/reason.py` |
| Per-track / per-mode prompts | inline in `muteki/solver/cli_solver.py` (`_EXEC_PROMPT`, `_EXPLORE_PROMPT`, …) |
| Swarm + Insight Bus | `muteki/swarm/` (`swarm.py`, `insight_bus.py`, `models.py`) |
| Shared evidence graph (event-sourced) | `muteki/swarm/shared_graph.py` |
| Capability SDK (the agent's tools) | `muteki_kit/{web,crypto,forensics,misc,reverse,pwn}/` |
| Sandbox kernel | `muteki/sandbox/` |
| Event spine / cost ledger / sessions | `muteki/core/` |
| Frontends | `apps/web/` (FastAPI+SSE+Next.js), `apps/tui/` (Textual) |
| Blackboard skill (worker read/write of the shared graph) | `skills/muteki-blackboard/` |
| Roadmap | `ROADMAP.md` |

## Running a frontend (`run.sh`)

```bash
./run.sh tui                      # Textual TUI, mock stream (UI demo, no key)
./run.sh tui --swarm --key <k>    # TUI solving a real challenge (needs a key)
./run.sh web                      # FastAPI backend (:8000, API-only) + Next UI (:3001)
./run.sh web --backend-only       # backend only (:8000)
```

The Next.js app (`apps/web/ui/`, :3001) is the deck; the FastAPI backend (:8000) is
API-only (serves the SSE / `/api` contract). The web UI's chat input *commands the
swarm* (hint / redirect / focus / pause / resume / submit) through the HITL backend —
but guidance is context, never a flag source (the provenance gate is unchanged).

> Dev SSE gotcha: the Next dev proxy buffers Server-Sent-Events, so a live run can look
> frozen until it ends. Point the browser at the backend directly with
> `NEXT_PUBLIC_MUTEKI_API=http://127.0.0.1:8000`. A detached backend can also linger on
> :8000 (EADDRINUSE) — `lsof -ti :8000` and kill it before reusing the port.

## Blackboard skill (worker coordination)

Workers read/write/claim the shared graph through the `muteki-blackboard` skill
(`skills/muteki-blackboard/`), pointed at `$MUTEKI_BLACKBOARD_DB`. Both `claude` and
`codex` support skills; install into both skill dirs with
`scripts/install_blackboard_skill.sh`. Treat any blackboard content as a *lead*, not
ground truth — it never bypasses the flag gate.

## Knowledge base (optional, off by default)

A worker can optionally query a knowledge-base MCP (e.g. your own CVE / writeup index)
**if the operator configures one** — no KB service is bundled, and the KB is off by
default. Opt in via `MUTEKI_KB_MCP_NAME` (the server key from your own user-scoped
`.mcp.json`). MCP results are leads/clues, not ground truth — they never become an
accepted flag without appearing in real execution output.

## End of session

If you stop mid-feature, leave a one-paragraph handoff in `session-handoff.md`
(local-only) so the next session restarts from a clean state.
