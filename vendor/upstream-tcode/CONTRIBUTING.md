# Contributing to Tcode

Thanks for taking the time. Tcode is a small project — bug reports, UI polish and
provider work are all welcome. By participating you agree to the
[Code of Conduct](CODE_OF_CONDUCT.md).

This file has two parts. **Principles** is the one text the maintainer has
personally reviewed; every other document, comment and issue in this repository
is derived from it. **Process** is how to build, check and submit work.

## Principles

1. **Tcode is a native GUI proxy for TUI coding agents.** When a native harness
   already has a capability, Tcode surfaces it as it is instead of building its
   own equivalent. Tcode never hides, alters or replaces what the model would
   see or do through the native CLI. Anything Tcode injects into a model — MCP
   tools, prompts, transcripts — is visible to the user. Capabilities a harness
   does not have (for example rewind outside Claude Code) are not synthesized.

2. **Host and client.** One Tcode host owns all state: providers, threads,
   terminals, projects. Every client — the desktop window, a phone, a tablet, a
   browser tab — is a screen for that host and speaks the same protocol whether
   it runs on the same machine or across a network. Layers depend strictly
   downward: `app → ui/runtime/services`, `ui → runtime/core`,
   `runtime → services/core` and lower adapters, `services → core`.

3. **Six native providers, then ACP.** Claude Code, Codex, pi, OpenCode, Cursor
   and Grok are maintained natively, permanently, over each CLI's own protocol.
   No seventh native integration is planned; every other agent connects over
   ACP, and ACP registry entries that duplicate a native integration are hidden.

4. **One canonical event stream.** Every provider normalizes into one
   `AgentEvent` stream and accepts one `SessionCommand` set
   (`crates/agent/src/lib.rs`). The stream is the union of what all providers
   can emit: an event only one provider produces still enters the union and is
   handled uniformly, so a provider that never emits it simply appears not to
   have it. The UI never branches on the provider kind.

5. **Remote access.** iroh, end to end encrypted, is the only transport for
   native clients; the browser client is a direct-access HTTP entry on the
   machine and does not use iroh. There is no automatic discovery: a device's
   first contact with a machine, or with a group of machines, is always an
   invitation, scanned or pasted; a machine admits only devices it paired or
   that a machine in its group admitted. An official service, **Traverse**,
   provides lookup and relay fallback: no accounts, on by default,
   self-hostable, sees only ciphertext, and never required — on a LAN, direct
   connections give the full product without it. Abuse is limited without
   accounts: the relay forwards by destination id and the machine rejects
   every peer it has not admitted; lookup and relay bandwidth are rate-limited
   per node and per IP; an invitation secret dies after five failures on the
   machine, which is the only place it is ever checked — the service never
   sees it; and no SLA is promised. Current status
   and plan: [#376](https://github.com/Tryanks/tcode/issues/376).

6. **gpui-base is the infrastructure; no house UI principles.** Tcode follows
   the recommendations and defaults of gpui-base and gpui-kit, and keeps no
   separate design contract in sync with the code. Everything gpui-base
   provides on top of GPUI — selection, scrolling, positioning, dismissal,
   focus, theme tokens, motion, virtual lists, form controls — is used as it
   is, defaults included, before anything of the kind is written in Tcode.
   Tcode's own components are styled compositions of those primitives; they
   never re-derive a primitive's rules locally. A primitive is reimplemented,
   or a default overridden, only where gpui-base demonstrably cannot meet the
   need; the site names that gap, and it is removed once gpui-base closes it.

7. **Code and tests are the only source of truth.** This section is the only
   maintainer-authored text; comments, issues and other documents are derived
   from it and are corrected when they disagree with it or with the code. A
   comment is kept only when it states a constraint the code cannot express or
   the reason an obvious alternative was rejected; narrated steps, history and
   "mirrors X" notes are deleted when touched. A constraint drafted by an AI is
   not a requirement until it appears here. A test, whatever its form, is
   truth only for a contract real use needs, and only while it is the test
   that proves it by driving the code the way the running app does. The suite
   is larger than its contracts: a test met while working on its area is
   judged on those terms, not left until it fails, and a failing one is first
   a question about the test and only then a verdict on the code. A test
   that proves no such contract is deleted, with whatever code existed only
   to satisfy it; a test is never edited into passing.

8. **Adopt upstream early; fix what is Tcode's, carry what is upstream's.**
   Prefer an upstream's experimental feature over waiting for it to mature,
   and never fork a dependency. A problem whose cause lies within Tcode — its
   code, its tests, or the way it uses a dependency — is fixed in Tcode where
   it is found, as part of the work at hand; it is not carried, reported or
   left for later. Only a problem whose cause lies in the upstream itself is
   carried and reported. Such a problem may be investigated freely — reading
   its source, building it, patching a local copy to confirm a cause — and is
   judged by its effect on Tcode's real use. Findings are reported to the
   maintainer or as an issue in this repository; diagnostic patches stay local
   and are discarded, and Tcode carries the bug until upstream changes on its
   own. Acting on an upstream — an issue, pull request, comment or patch there
   — is never a recommendation or an option to offer: it happens only when the
   maintainer explicitly asks for it, with the maintainer taking part
   throughout.

9. **The protocol version follows releases, not changes.** `PROTOCOL_VERSION`
   (`crates/protocol/src/lib.rs`) is bumped once per release whose wire
   differs from the previous release's, when that release is cut, never in
   the pull request that changes the wire. Between releases, `main` may
   change the wire freely under the same number: every client and host are
   built from one tree, and compatibility is promised between releases, not
   between commits. A wire change notes itself in the version-history comment
   above the constant; cutting a release that has such notes bumps the number
   once and folds the notes into one line for that version.

10. **An intermittently failing test is historical debt.** A test that both
    passes and fails on the same code is deleted as soon as it is met,
    wherever that is; it is not fixed, retried, quarantined or ignored. The
    deletion follows the other principles: whatever code existed only to
    satisfy that test goes with it, and the pull request names the test and
    where its contract is still covered, or that none is.

## Process

### Reporting bugs and asking for features

Open an [issue](https://github.com/Tryanks/tcode/issues). For a bug, include
your OS, which agent (Claude Code, Codex, pi, OpenCode, Cursor, Grok, or which
ACP agent), what you did, what happened and what you expected. A screenshot
beats a paragraph for anything visual. If you are unsure whether something is a
bug or intended, open an issue anyway — the answer is worth writing down either
way.

### Building

You need a recent Rust toolchain (edition 2024). The first build compiles GPUI
from source and takes 10–20 minutes; later builds are fast.

```sh
git clone https://github.com/Tryanks/tcode
cd tcode
cargo run
```

If `mbx` is installed, run cargo through it (`mbx build`, `mbx nextest run`, ...) so compiled work
is shared across worktrees; the [`mbx` skill](.agents/skills/mbx/SKILL.md)
has the detection and sandbox details.

Platform prerequisites:

- **macOS** — Xcode command-line tools.
- **Windows** — MSVC toolchain.
- **Linux** — a Vulkan driver plus the usual GPUI build deps (`libxkbcommon-dev`,
  `libwayland-dev`, `libxcb*`, `libssl-dev`, `libasound2-dev`, `libfontconfig-dev`)
  and WebKitGTK for the embedded preview (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`).
  The preview embeds only into an X11 window: on a Wayland session run
  `WAYLAND_DISPLAY= cargo run` to get it through XWayland.

Provider CLIs are resolved from `PATH` (`claude`, `codex`, `pi`, `opencode`,
`cursor-agent`, `grok`) and can be overridden in **Settings → Providers**.

### Before you open a pull request

CI checks formatting, Clippy and the workspace tests on macOS, Windows and
Linux, on every push, with no change-scoped shortcuts. Run the same checks
locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked   # or: cargo test --workspace --locked
```

Clippy also enforces the process boundaries in [`clippy.toml`](clippy.toml):
child processes go through the `process` helpers in `services` and `agent`,
and blocking work goes through `Host::unblock`. A new approved site carries an
`allow(clippy::disallowed_methods)` with its reason.

CI also runs `cargo machete` to catch unused dependencies, and checks iOS, Android
and Web with `RUSTFLAGS='-D warnings'`. On Linux it also builds the browser
bundle, runs Clippy on `tcode-headless --features web` (the container image's
binary) and checks the container scripts and Compose files. Use the commands and
tool version in [the workflow](.github/workflows/ci.yml) to reproduce those
checks. Fix warnings
at their source; a platform-specific lint exception needs a concrete reason at
the declaration. For a dependency imported under a different crate name, use
Cargo's `package` alias or machete's `renamed` metadata so unused imports remain
detectable. An `ignored` entry requires an explanation of the generated or
implicit use it cannot detect.

New user-facing strings must be added to **both** `locales/en.yml` and
`locales/zh-CN.yml` — a parity test enforces it.

If you changed something visual, launch the surface and look at it in both
themes and at a wide and a narrow width; compile and unit checks alone do not
establish visual correctness. `cargo run -p tcode-ui --example phone` opens the
shared shell at phone geometry without a device.

### Verifying real behaviour

**Provider-layer probes** (no GUI — print the raw canonical event trace).
Use an existing working directory and image path in these examples.

```sh
cargo run -p agent --example probe -- claude "Reply with exactly: PONG" /tmp/smoke
cargo run -p agent --example probe -- claude "Run sleep 30, then reply DONE" /tmp/smoke --interrupt-after 5
cargo run -p agent --example probe -- codex "Run sleep 30, then reply DONE" /tmp/smoke --steer "Stop and reply PONG"
cargo run -p agent --example probe -- claude \
    "What color is this image? Reply with just the color." /tmp/smoke --image /tmp/blue.png
```

`TCODE_DATA_DIR` points Tcode at a throwaway profile (its own sessions, settings
and installed ACP agents) — useful for demos, screenshots and trying a change
without touching your real threads. The data directory is otherwise `~/.tcode`.
`LEGACY_TCODE_DATA_DIR` names an older data directory that a start moves into
the data directory (by default an earlier version's platform app-data
directory, and only when `TCODE_DATA_DIR` is unset); set both to try the move
on a copy of real data.

**Launch flags**: `--open-latest` reopens the most recent thread,
`--connect <host_id>` starts attached to a paired host, `--pair <invite-url>`
pairs from the command line with the `tcode://pair?…` link a machine shows, and `--preview-smoke` / `--cu-smoke` run the
preview and computer-use smoke phases against the live desktop.

**Phone geometry without pairing**:
`TCODE_DATA_DIR=$(mktemp -d) cargo run -p tcode-ui --example phone -- --local`
attaches the phone shell to an in-process host on that profile instead of a
paired machine, for screenshots from synthetic data
([#649](https://github.com/Tryanks/tcode/issues/649)).

Native platform behaviour — macOS permission grants, input delivery, camera
pairing on a phone — is only established by exercising it on that platform.
Ignored tests that need a desktop slot or credentials say so in their reason;
run them deliberately, never report an early return as a pass.

### Problems that may be a dependency's

Work through these in order; each step ends before the next begins.

1. **Localize.** Establish whether the cause is Tcode's code, a Tcode test, or
   the dependency. A test counts only once it drives the dependency the way
   the running app does (frames, clocks, input); a test that departs from that
   is Tcode's to fix. A failing test is a lead, not a verdict. When the cause
   is Tcode's, fix it now as part of the work at hand and stop here; the
   remaining steps are only for a cause in the dependency.
2. **Weigh the impact.** Name the builds, platforms and user paths the problem
   reaches in real use, and how it shows there. Where nothing real is reached,
   say so; that ends the matter apart from the report.
3. **Confirm the cause.** Read the dependency's source, check it out, and patch
   or instrument a local copy until the cause is shown rather than inferred.
   The copy lives outside this repository and is deleted afterwards.
4. **Report.** Give the maintainer — in the session, or as an issue in this
   repository when it outlives one — the cause, the evidence and the impact
   from step 2. Principle 8 governs anything beyond this repository.
5. **Carry it.** Handle the problem inside Tcode at the owner of the affected
   behaviour, with a comment naming the dependency behaviour it works around,
   or leave it in place when step 2 found no real impact.

### When a test fails, or is met on the way

Work through these in order for a failure met while developing, in CI, or
intermittently, and for every test in the area a change touches, whether or
not it fails; the suite predates this bar and is far larger than its
contracts, and the form of a test — unit, headless UI, probe, replay, or one
not seen before — changes none of the questions. "Make it pass" is not a
step: an assertion is never loosened or rewritten to the new output, a run is
never retried into green, and no seam is added to production for the test's
sake.

1. **Reproduce it as the app would.** For a failure, run the case alone and
   inside the suite. A test that both passes and fails on the same code is
   deleted under Principle 10, and that settles it. A failure that reproduces
   every time but depends on the suite's timing, on the wall clock, or on a
   frame the platform would have drawn differently is the test driver's
   fault, however the assertion reads: make the driver behave like the running
   app (frames, clocks, input) and stop there if that settles it. The gpui-fast
   switch (#555) was this case for every one of its ten failures.
2. **Judge the assertion, not the diff.** Name the contract the test states
   and where it comes from: a behaviour a user path reaches, a recorded
   fixture, or a known regression; name the credible regression that would
   fail it; and name why no other test already owns that contract. If any
   answer is missing — it asserts an implementation detail, a number nothing
   depends on, an assumption inherited from an earlier dependency, a scenario
   no user path reaches, or a contract a stronger test already proves — the
   test is wrong whatever the code does, and the fact that it was written
   with care, or passes, or has peers of the same shape, is not an answer.
   Delete it, and with it every function, seam, fixture or dependency whose
   only reason to exist was that test. The
   [`test-audit` skill](.agents/skills/test-audit/SKILL.md) has the patterns
   and the retention bar.
3. **Only then treat the code as broken.** When the contract is real and the
   code no longer meets it, fix the code at the behaviour's owner and keep the
   test. A test is rewritten only to move it to the owning boundary or to
   drive the code the way the app does, never to agree with the code.
4. **Say which it was.** The pull request names each failed or removed
   test, the step that settled it, and the evidence. A deleted test names
   where the contract is still covered, or why there was none; a fixed driver
   names what the app does that the test did not. The number of tests kept or
   removed is evidence of nothing.

### Code layout

```
crates/core              pure domain types and folds (the event → timeline fold
                         is shared by live streams and replay)
crates/protocol          serializable client ↔ host contract, PROTOCOL_VERSION
crates/client            transport-agnostic client endpoint; ClientHost is the
                         platform-capability contract
crates/services          persistence, filesystem, process, git, import, probes
crates/runtime           session and provider lifecycle, queues, orchestration,
                         terminals, semantic events; AppState is reached only
                         through serialized protocol messages
crates/traverse          tcode-traverse owns remote access: the iroh endpoint,
                         machine and device identities, invitations and the
                         allow list, the multi-client mux, Preview tunnels, the
                         Traverse manifest, and the browser listener
crates/traverse-server   tcode-traverse, the self-hostable Traverse instance
                         (relay, pkarr store, manifest)
crates/ui                the one GPUI shell every client opens via `run_shell`;
                         owns localization (crates/ui/src/i18n.rs) and presentation
crates/app/src/main.rs   desktop binary and composition root (default `cargo run`)
crates/headless          headless host binary
crates/ios, crates/android, crates/web
                         platform bootstrap only: a ClientHost and a call into
                         `tcode_ui::run_shell`
crates/platform/*        GPUI platform backends for iOS and Android
crates/agent             provider clients (no GPUI) — claude.rs, codex.rs, pi.rs,
                         opencode.rs, cursor.rs, grok.rs, acp.rs; the ACP
                         session they share is acp_session.rs
crates/term              terminal implementation (PTY, host-side emulator)
crates/mcp-host          shared authenticated loopback host for in-process MCP servers
crates/preview-mcp       MCP server exposing the preview browser to the agent
crates/orchestrate-mcp   MCP server for orchestration tools
crates/computer-use-mcp  MCP server for desktop automation (macOS, Windows)
```

Runtime emits semantic events; UI owns their localization and presentation.

**Adding a provider** means writing one client in `crates/agent` that translates
its wire protocol into `AgentEvent`, and nothing else. If you find yourself
special-casing a provider inside `crates/ui/src`, the contract is missing
something — say so in the PR. Changing `crates/agent/src/lib.rs` requires the
full-workspace build, because every client consumes it.

Never spawn a child process with `std::process::Command::new` directly: use the
process helpers in `crates/services/src/process.rs` and
`crates/agent/src/process.rs`, which suppress the console window on Windows and
resolve binaries against `PATH`/`PATHEXT`. A guard rejects direct
`Command::new` usage.

### Review

**One owner per behaviour.** Before adding a type, state field, helper or
dependency, find its current owner and callers, and extend that owner when it
already represents the same concept. An abstraction should hide a real policy,
platform boundary or lifecycle; a forwarder or fallible signature needs a
responsibility beyond passing values through. Compute derived state where it is
consumed unless caching has a measured benefit and an explicit invalidation
path. Remove obsolete callers, conversions, fixtures and dependencies with the
code they supported. Keep documentation with the behaviour it describes and
link to the owner instead of copying values.

**Tests earn their maintenance.** Weigh a test before writing it: name the
observable contract or realistic failure it would catch, and check that no
existing test already catches it; extend an existing case or fixture before
adding a near-duplicate. Expected values derive from that contract, a recorded
fixture or a known regression — not from the implementation. A serialization
round trip alone cannot establish wire compatibility: assert literal messages
or older persisted inputs. Use the smallest production entry point that
exercises the behaviour, never a seam only the test uses; assertions exercise
production logic, not an algorithm recreated in the test. Library and upstream
behaviour, constant/getter wiring and repeated happy paths need a
project-specific reason to be tested. When removing or merging tests, say which
coverage was redundant and where any remaining contract is still covered. For a
bug fix, show the regression test fails without the fix when practical. Test
counts and deleted line counts are not quality targets. When a test's value is
in doubt, or when auditing or pruning tests, follow the
[`test-audit` skill](.agents/skills/test-audit/SKILL.md); for a test that
fails or sits in the area of a change, follow
[When a test fails, or is met on the way](#when-a-test-fails-or-is-met-on-the-way).

**Evidence in the pull request.** Describe the changed behaviour, why added
abstractions are needed or removed ones redundant, and the contract protected
by changed tests. Report the checks actually run, including platform or
live-service gaps. The same criteria apply to human and AI contributions;
passing CI cannot decide whether an abstraction or a test is useful. Merge only
after all checks on the final commit pass, including dependency hygiene and the
mobile and Web checks.

### Release signing

The [release workflow](.github/workflows/release.yml) signs and notarizes the
macOS app and DMG when all six `MACOS_*` secrets are configured, publishes
unsigned builds when none are, and fails naming the missing ones when the set is
partial. The credentials must come from the maintainer's Apple Developer
account; the secret names, how to obtain each value and how to verify a signed
build are documented in the workflow next to the signing step. Windows builds
ship unsigned. The Android APK is a release build signed with Gradle's debug
keystore until a release key exists.
