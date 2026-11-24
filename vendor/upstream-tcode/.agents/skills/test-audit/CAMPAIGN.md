# Test-pruning campaign

Campaign mode prunes one subsystem's whole test surface in one PR: a crate such
as `crates/runtime`, or one owner area across crates. PR #487 was one across
the whole workspace. The value bar, retention bar, candidate evidence, and
validation in [SKILL.md](SKILL.md) apply to every lane. This file adds the
order of work and the lessons of a full campaign. Each step ends on its
completion criterion; do not start the next step early.

## 1. Baseline

Record the subsystem's test declaration and support line counts and every
test's pass/fail state at a pinned `main` SHA, on each desktop platform CI
covers (the CI run for that SHA supplies the platforms you cannot run). Keep
baseline failures, ignored tests, and platform-gated tests in their own lists.

Done when every in-scope test has a recorded baseline result or a recorded
reason it cannot run.

## 2. Lanes and inventory

Split the surface into **lanes** along production owner boundaries, not file
names. In #487 these were provider protocol translation, runtime delivery and
sessions, UI scenarios, terminal replication, and remote access. Include the
subsystem's cases at shared boundaries, its probes, and its ignored live
tests.

Done when every test the subsystem owns belongs to exactly one lane.

## 3. Read-only ledger per lane

Give each lane to its own read-only agent. The agent reads every assigned test
in full, including case tables. It also reads the production owners and their
entry points, callers, history, and CI routing. Each test declaration goes into
a written **ledger** with one mark. A case table inside one test is one
declaration unless its rows need different marks; then mark each row.

- `R`: retain, naming the contract and the bug it catches; a retained test that
  only moves to a better owner stays `R` with the move noted;
- `F`: retain the contract but repair the assertion, such as a vacuous negative
  that passes when only one of several items is missing, or a sleep that should
  wait for observable state;
- `C`: consolidate, naming the owner that absorbs the assertion first: a sibling
  case table, a stronger boundary suite, or the shared owner in another crate;
- `D`: delete, naming the proof that remains, or why no contract exists.

Judge a test by its assertions, not its name.

Done when every declaration in the lane has a mark and an evidence line.

## 4. Layer plan per lane

Treat the per-test ledger as input, not as the edit list. A second read-only
pass, starting from the ledger, looks for the redundant **layer**. In #487,
small getter and repeated launch/shutdown checks folded into the runtime's
delivery and session matrices. Name the **keeper** suite for each contract.
Prefer a real actor or transport boundary, or a recorded wire fixture in
`crates/agent/tests/fixtures`, over a mocked collaborator. Correct any ledger
errors this pass finds.

Done when each lane plan names its retired tests, its keeper per contract, the
assertions to carry into keepers, and the test-only production seams unlocked.

## 5. Cutover

Edit lane by lane. Serialize changes to shared harnesses and support modules
(such as `crates/runtime/src/app/test_support.rs`) through one owner. With each
lane, remove the test-only production seams it unlocks: `test-support` items,
`#[cfg(test)]` accessors, injection parameters, and indirection layers. Durable
test-ownership rules the campaign surfaced go in the PR description as
proposals for the maintainer; CONTRIBUTING's Principles are the only place a
rule becomes a requirement.

Done when every lane plan is applied and each lane's keepers pass.

## 6. Preservation review

Before claiming completion, have independent reviewers compare deleted
coverage against the keepers, one reviewer per boundary group. They look for
contracts that lost their only proof. They also look for new assertions that
cannot fail, such as a rejection row the production code never reaches.

For each restored contract, make one deliberate **mutation** of the production
owner and confirm the keeper goes red. Then restore the source byte for byte.

Done when every reported gap is restored or rejected with source evidence, and
every restored contract has a caught mutation.

## 7. Product defects

A baseline failure, or a failure a broadened keeper exposes, is a bug report.
In #487, a real-PTY regression showed the Unix PTY reporting raw `waitpid`
bits (exit 37 as 9472). Fix it at its owner as a separate commit, and prove it
through the real user flow, with a **control** run that reverts the fix and
shows the old behavior. Record unrelated product discrepancies you find as
follow-ups instead of fixing them in the campaign.

Done when each repaired defect has a failing control and a passing candidate
on the same harness.

## 8. Reconcile and hand off

Campaigns outlive many `main` commits. Merge `main` rather than rebasing a
long, many-commit campaign. When `main` modified a test the campaign deleted,
keep the deletion. Port the new contract into the keeper instead, and confirm
every new regression `main` added still has a home. Rerun the whole subsystem
suite and repeat live and platform proof on the merged head.

Expect review tooling to see a truncated file list on a diff this large.

Hand off with the [SKILL.md](SKILL.md) report, plus:

- baseline and final test counts and support line counts, with production
  counted separately;
- lanes, retired layers, and keepers;
- preservation gaps found and their mutations;
- product defects with control and candidate proof.
