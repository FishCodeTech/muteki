---
name: test-audit
description: "Invoke whenever writing, changing, reviewing, or sweeping tests. Authoring gate for new tests plus audit workflow for low-value, implementation-coupled, or duplicative tests and the test-only production seams they demand."
---

# Test Audit

Three modes, one value bar: **Tests earn their maintenance** in the Review
section of [CONTRIBUTING.md](../../../CONTRIBUTING.md), which this skill
applies and never overrides. Authoring mode gates every new or changed test at
write time. Audit mode runs focused sweeps of tests that re-assert source,
duplicate stronger proof, couple behavior to implementation, or keep test-only
production seams alive. Continue broad audits as separate coherent follow-up
PRs; optimize for confidence, not deletion count. Campaign mode prunes one
whole subsystem's test surface (a crate, or one owner area such as
`crates/runtime/src/app`); before starting one, read [CAMPAIGN.md](CAMPAIGN.md).

## Authoring gate

Before adding any test, answer four questions; a missing answer means do not
add it yet:

1. What observable behavior, invariant, or independent contract does it protect?
2. What credible regression makes it fail?
3. Why does existing coverage not already catch that failure? Each contract has
   one primary test owner at the strongest boundary; another layer needs its
   own distinct risk, such as a transport or lifecycle failure the owner cannot
   reach. Prefer extending a case table or shared fixture over a near-duplicate
   test; consolidate duplicated setup in the same change.
4. Does it need a production seam that no production caller needs: a
   `#[cfg(any(test, feature = "test-support"))]` item, widened visibility, an
   injection parameter, an `allow(dead_code)`? If yes, move the test to the
   real boundary instead.

Then check the test against every [junk pattern](#junk-patterns); a match fails
the gate unless the [retention bar](#retention-bar) names the contract it
independently guards. A test that would break under behavior-preserving
refactoring is asserting implementation, not behavior; rewrite it at the
owning boundary before landing it.

Bug regression tests must fail on the pre-fix code for the intended reason and
pass after the owner-boundary repair. A regression test that never demonstrably
failed proves the mock, not the fix. One regression at the owner boundary
covers the bug; do not replay the same scenario at every layer it crosses.

## Junk patterns

The shared checklist for both modes: the authoring gate rejects a new test that
matches one, and audits hunt for existing tests that do.

- assertion-free coverage probes;
- self-comparisons and identity copiers;
- serialization round trips standing in for wire or storage compatibility,
  where a literal message or an older persisted input is the real contract;
- copied fixtures, inventories, manifests, or variant lists;
- exact source, import, or string greps;
- private predicate or call-shape tests duplicated at real boundaries;
- duplicate invocations of the same contract;
- provider-local replays of shared helpers, such as each `crates/agent` client
  re-testing a fold that `crates/core` owns;
- upstream behavior (GPUI, serde, iroh, the terminal parser, Markdown
  primitives) and constant or getter wiring, without a Tcode-specific reason;
- tests whose only purpose is preserving `test-support` items, `#[cfg(test)]`
  accessors, or wrappers;
- dead production code whose only callers are tests;
- expected values produced by the helper or renderer under test;
- mocks that implement the asserted behavior, or one identical mock standing in
  for different APIs;
- fixtures that supply the receipt, admission, or callback ordering the owner
  should produce, or persistence asserted against a store the path never writes;
- capability tests that restate declared flags instead of exercising the
  delivery or acknowledgement the flag promises;
- timing guesses: sleeps, fixed executor pump counts, or deadlines a hair above
  production's, where the test should wait for the observable state;
- negative controls that pass for an unrelated reason, such as a denial from a
  different guard or a rejection the production path never reaches;
- names or fixtures that promise more than the input exercises, such as a
  "retires the window" test asserting the window was not cleared.

## Value bar

In an audit, an existing test that must change for behavior-preserving source
reorganization is suspect, not automatically deletable; the authoring gate
still rejects new ones.

Before judging a candidate, read the complete test and production owner, its
entry point, callers, callees, sibling implementations, overlapping tests, CI
routing, and relevant history. CI routing here means which targets compile the
test (`cfg(target_os)` gates), whether it is `#[ignore]`d, and whether its crate
is outside the workspace (`crates/platform/*`) or outside Cargo
(`crates/web/static/*.test.mjs`). When the test claims dependency-backed
behavior, inspect the dependency source or types directly.

## Discovery

Keep discovery read-only and report evidence before editing. For broad scope,
run parallel discovery lanes when available, split along the layers in
CONTRIBUTING's code layout:

- provider translation (`crates/agent`, with its recorded wire fixtures);
- domain and contract (`crates/core`, `crates/protocol`, `crates/client`);
- host (`crates/runtime`, `crates/services`, `crates/term`);
- remote (`crates/traverse`, `crates/traverse-server`);
- UI and platform (`crates/ui`, `crates/platform/*`, `crates/web`);
- MCP servers (`crates/*-mcp`, `crates/mcp-host`);
- a cross-cutting pattern sweep.

Outside campaign mode, prefer a few high-confidence candidates over a large
speculative inventory. Hunt for the [junk patterns](#junk-patterns).

## Retention bar

Keep a test when it independently enforces a Tcode contract: the `AgentEvent`
and `SessionCommand` union, the client ↔ host protocol, a provider's wire
translation, persisted session or settings formats, security (pairing,
invitations, tool authorization, path containment), platform behavior, locale
parity, the process and layering boundaries, or a default. Also keep:

- call ordering when order is observable behavior;
- regressions with a credible failure mode;
- source inspection when it is the cheapest independent guard: it fails when
  the contract changes (the user-facing key, byte, or path) and survives an
  identifier-only refactor;
- an `#[ignore]`d or platform-gated test whose reason names the desktop slot,
  credential, or network it needs;
- a retained test that fails on the baseline: treat it as a possible product
  bug, reproduce it, and repair the owner rather than deleting it.

Static or slow is not a deletion reason. A test that resembles implementation
may still be the independent contract; prove otherwise before removing it.

## Candidate evidence

Record every field below before editing. A missing field means the candidate is
not ready for deletion:

- exact test name and location;
- what failure it can actually detect;
- non-test callers of the covered production or support seam;
- stronger remaining owner-boundary proof, or why no proof is needed;
- relevant history and the reason the test or seam exists;
- production or test-support deletion unlocked;
- risk and the focused validation command.

## Edit shape

Choose one coherent owner-boundary batch. Delete obsolete `test-support`
items, `#[cfg(test)]` accessors, wrappers, and dead production paths instead of
preserving aliases; drop a `test-support` feature once nothing gates on it.
Move retained regressions to their canonical owners. Consolidate repeated
dependency or boundary assertions into one generic contract.

Prefer net-negative production LOC. Do not add replacement tests that restate
the same implementation, and do not convert uncertain candidates into cleanup
to increase deletion counts.

## Validation

1. Run the smallest owner and sibling tests with
   `cargo nextest run -p <crate> --locked <filter>`. Run crates outside the
   workspace through their own manifest, and `node --test` for
   `crates/web/static` tests you touched.
2. For an async or timing change, repeat the targeted tests until the old flake
   would have shown, and treat a nextest LEAK warning as a failure.
3. For removed source greps or call-shape assertions, run the executable path
   that owns the real contract, such as the provider probe in CONTRIBUTING's
   **Verifying real behaviour**.
4. Run `cargo fmt --all`, then `git diff --check`.
5. Run every check in CONTRIBUTING's **Before you open a pull request**.
6. Count `#[test]`, `#[gpui::test]`, and `#[tokio::test]` declarations before
   and after, and report production LOC separately from tests and test
   support; inline `mod tests` blocks mean `git diff --numstat` alone does not
   separate them.
7. After final audit edits, get an independent review against CONTRIBUTING's
   **Review** section.

## Landing and continuation

Commit, push, open a PR, or merge only when authorized. The PR carries the
evidence CONTRIBUTING's **Evidence in the pull request** asks for, and merges
only after every check on the final commit passes. Land one coherent PR at a
time; after it merges, refresh from current `main` and rerun read-only
discovery for the next high-confidence batch.

## Handoff

Report:

- root cause and removed low-value categories;
- production owner simplifications;
- retained false positives and why they remain valuable;
- focused and full proof actually run, and the ignored, platform-gated, or
  out-of-workspace tests left unrun;
- test counts, and production versus test LOC;
- PR and merge state;
- named follow-ups.
