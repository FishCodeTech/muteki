---
name: verify
description: Project-local pre-PR verification skill for muteki-rust. Run immediately after finishing feature implementation and before committing/pushing/opening a PR: runs diff-routed custom Lints, Rust & Frontend property tests, Web E2E via kimi-webbridge (Playwright forbidden), and Desktop E2E via cua-driver while strictly respecting data isolation, state inheritance, and production upgrade safety.
---

# Muteki Pre-PR Verification Workflow

Execute this workflow **after completing an implementation and before `git push` or opening a PR**.
Never substitute "build succeeded" or "model claimed done" for real evidence.

---

## Step 1: Environment Doctor & Automated Hard Gates

1. Inspect workspace state, port ownership, `.state` symlink target, and tool readiness:

```sh
node scripts/verify/run.mjs --doctor
```

2. Run diff-routed Lint + Property Tests (and print matched E2E tracks & checklists):

```sh
node scripts/verify/run.mjs
```

- Automated tracks cover:
  - Custom Lint (`node scripts/lint_rules.mjs`, `npm --prefix apps/frontend run lint`, `npm --prefix apps/frontend run typecheck`, `python3 scripts/generate_frontend_contracts.py --check`, `cargo clippy --locked -p muteki-server -- -A clippy::all -D clippy::disallowed_methods`)
  - Frontend Property Tests (`npm --prefix apps/frontend run props` via `fast-check`)
  - Rust Property Tests (`node scripts/cargo.mjs run --profile local -p muteki-server --features verify-props --bin muteki-verify-props` via `proptest`)
- If an automated check fails, fix the root cause (up to **3 repair rounds** max — see Step 6).

---

## Step 2: Build, Process Ownership, Data Isolation & Inheritance Rules

Strictly follow [docs/development.md](../../../docs/development.md) and [docs/self-development.md](../../../docs/self-development.md):

1. **Check `.state` before reusing or assuming isolation**:
   - Inspect `ls -la .state` (`--doctor` reports this). `.state` may be a symlink to existing dev data, and switching git branches does NOT switch `.state`.
   - Use **explicit `MUTEKI_STATE_DIR=/tmp/muteki-verify-<id>`** when you need clean-room isolated verification (explicit `MUTEKI_STATE_DIR` never triggers legacy auto-migration).
   - Use **default `.state`** only when intentionally testing data inheritance / existing dev state, and never mutate `~/Library/Application Support/Muteki/data`.
2. **Check port `8100` / `3101` / `3103` ownership before reusing**:
   - Confirm whether an existing `8100` backend belongs to THIS checkout and expected state dir. Never kill `/Applications/Muteki.app` or another workspace's server.
   - For isolated smoke/E2E runs, allocate ephemeral free ports (`MUTEKI_API_PORT`, `MUTEKI_UI_PORT`, `MUTEKI_BACKEND_PORT`, `MUTEKI_UI_ORIGIN`).
3. **Production App protection**:
   - **Never replace `/Applications/Muteki.app` during routine PR verification.**
   - Only perform local replacement when the user explicitly requests upgrading the installed App, following `docs/development.md` (`node scripts/install-macos.mjs --build --open` after quitting the installed app, or detached updater waiting for idle + verifying hash, codesign, readiness, and data directory identity).

---

## Step 3: Web E2E via `kimi-webbridge` (Playwright Strictly Forbidden)

When diff triggers `web`:

- **Playwright is strictly banned** (`playwright`, `@playwright/*`, `npx playwright`). Never install, import, or run it.
- Ensure Web dev server is serving current builds (`node scripts/dev.mjs --web`, `node scripts/dev.mjs --hot --web`, or isolated `muteki-server` + Next on ephemeral ports & temp `MUTEKI_STATE_DIR`).
- Check `kimi-webbridge` daemon (`http://127.0.0.1:10086`):
  - If unreachable, start it with `~/.kimi-webbridge/bin/kimi-webbridge start`.
  - If daemon returns `extension_not_connected`, **stop and report that exact block to the user** — never fake Web E2E or fall back to Playwright.
- Execute real user-flow verification in **one session name**:
  1. `navigate` (`newTab: true`, clear `group_title`) to the target workbench URL (`/`, `/chat`, `/ctf`, `/pentest`, `/src`, `/settings`, or changed route).
  2. `network` (`cmd: "start"`) before critical mutations; inspect `list` + `detail` (e.g., `/api/health`, `/api/readiness`, `/api/domain-modules`, chat/attachment payloads) before stopping capture.
  3. `snapshot` (`format: "aria"`) and drive real controls via `click` / `fill` using `@e` refs.
  4. Verify zero uncaught page/console/hydration errors via `evaluate` (wrapped in an IIFE, compact `JSON.stringify`).
  5. Verify chat attachment separation (if chat/attachment touched), narrow viewport behavior, and keyboard interaction.
  6. Capture `screenshot` and **visually inspect the saved image** with `read_file`.

---

## Step 4: Desktop E2E via `cua-driver`

When diff triggers `desktop`:

1. Build workspace (`local` profile via `node scripts/cargo.mjs build --workspace --profile local` and Next build if needed).
2. Start dev desktop (`node scripts/dev.mjs` or isolated `.cache/target/local/muteki-desktop` with explicit `MUTEKI_STATE_DIR`, `MUTEKI_BACKEND_PORT`, `MUTEKI_UI_ORIGIN`).
3. Target **ONLY the spawned dev PID**:
   - Call `list_apps` / `list_windows` via `cua-driver` and verify exact `pid` + `window_id` belong to the dev binary, NEVER `/Applications/Muteki.app`.
   - Start from targeted `get_window_state({pid, window_id, query})`.
   - Drive real controls via `run_steps({steps, observe: {}})` with snapshot-bound `element_token` values.
   - Verify expected UI/state changes via `verify_state` or targeted `get_window_state`.
   - Close your `cua-driver` session (`end_session`) and terminate ONLY the processes started by this verification run.

---

## Step 5: Runner & Model Runtime Tracks (When Triggered by Diff)

- **`runner`**: Run the matched `scripts/verify_runner_*.py` scripts in isolated temp state (and `scripts/verify_worker_image.sh` if `docker/worker/**` changed). Never stop unrelated containers.
- **`model_runtime`**: Verify real configured CLI/model execution, tool receipts, subagent/AGY native stream `fullyIdle`, cancel, and exit cleanup. Never substitute synthetic replies.

---

## Step 6: 3-Strike Escalation & Anti-Cheating Rules

- If the same check or E2E step fails **3 consecutive repair rounds**, stop and hand off to a human with full logs, minimal repro, and hypothesis.
- Never add `#[ignore]`, `.skip()`, `.only()`, bare `sleep >= 5s`, or weaken `clippy.toml` / `.eslintrc.json` / `scripts/lint_rules.mjs` to force green.
- If a check is flaky and root cause cannot be found after investigation, escalate or remove it with documented justification — never retry until luck passes.

---

## Step 7: Record Evidence & Generate PR Stamp

After all required tracks pass (or if an explicit skip/block occurred):

```sh
node scripts/verify/run.mjs \
  --record-e2e "web=pass:kimi-webbridge verified /chat,/src,/settings and /api/readiness" \
  --record-e2e "desktop=pass:cua-driver verified dev PID window lifecycle" \
  --stamp
```

- Paste the generated `Verification Stamp` (including the `<!-- muteki-verify-stamp {...} -->` comment) into the PR description **after your final commit** so `head_sha` matches the PR `HEAD`.
- Before pushing, run `node scripts/verify/pr_gate.mjs --local-push` (automatically invoked by `.githooks/pre-push`).
