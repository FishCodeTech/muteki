# Project Muteki — Iteration Roadmap

> **Current status override (2026-06-16 CST):** this roadmap is retained as
> historical planning context. It predates the CLI-only executor, multi-flag work,
> current web deck, and code-review v7 remediation queue. Use `progress.md`,
> `session-handoff.md`, `feature_list.json`, and `docs/CODE_REVIEW_2026-06-15.md`
> for the active next step.

> Synthesized 2026-05-30 from a 6-dimension parallel audit (gap-vs-design, missing-tracks,
> solve-rate levers, code-quality, eval-rigor, frontend/platform) + a synthesis pass.
> Every structural claim below was re-verified against the code before adoption.

## Where we are

P0 (event bus / cost ledger / session store), P1 (code-driven solver + persistent
subprocess kernel + provenance flag gate), P2 (FastAPI SSE/WS backend + Next.js/assistant-ui
deck + Textual TUI), P4 (swarm + Insight Bus, first-valid-flag), P6 (NYU black-box eval +
self-learning distill/retrieve) are **built and verified**. Real NYU result: **1/3 web solved
black-box, provenance gate held (no false flags)**. 114 tests green.

P3 muteki_kit (the code-driven kernel's capability SDK) was **removed** — that
kernel is retired; the CLI-agent swarm uses its own in-container tools. The
crypto/reverse build-out below (I2/I3) is obsolete and kept only as a record.
P5 (L0 coordinator / CTFBridge / auto-submit) is **user-skipped**.

## The dominant lever (exact data)

NYU CTF Bench `test_dataset.json` = 200 challenges:

| Track | Count | % of bench | SDK |
|-------|------:|-----------:|-----|
| crypto | 52 | 26.0% | ❌ |
| rev | 51 | 25.5% | ❌ |
| pwn | 39 | 19.5% | ❌ |
| misc | 24 | 12.0% | ❌ |
| **web** | **19** | **9.5%** | ✅ |
| forensics | 15 | 7.5% | ❌ |

**The agent can currently attempt only 9.5% of the benchmark.** No amount of loop tuning
moves points-per-dollar-hour on challenges it cannot start. crypto + rev alone = 51.5%.

## Verified structural gaps (not polish — load-bearing)

1. **Hypothesis machinery is dormant.** `SolveGraph` has `add_hypothesis/set_status/
   active_hypotheses/mark_dead_end` (solve_graph.py:85-118) but `muteki/solver/` has **zero**
   call sites — the §6.1 hypothesis-driven bounded search is aspirational; the graph only
   accumulates flat evidence.
2. **No triage→track routing.** Solver hardcodes `from ...prompts.web import SYSTEM_PROMPT`
   (solver.py:32). `triage` is imported into the kernel but never selects prompt/SDK by
   category. Even when crypto/rev SDKs exist, nothing would dispatch to them.
3. **HITL is cosmetic.** `run.hitl` queue is enqueued (run_manager.py) but **no consumer** —
   the solver drains only the *insight* inbox, never HITL. Human hints go into the void.
4. **No context compaction / CONTEXT_STATE in the real solver.** Only the mock emits it; the
   real loop never monitors token pressure or compacts (§6.3).
5. **Loop guard is crude** — 85%-similarity-over-3-turns on raw code; fires false-positives on
   legitimate same-endpoint-different-technique retries (the exact no-pass-needed failure mode).
6. **Eval fixed at 60s** (eval.py:96) — systematically fails crypto/rev where factoring/angr
   need minutes.
7. **`InsightBus.history` unbounded** (insight_bus.py:53); terminal WS replays from seq 0.
8. **Rejected flags not logged to an audit artifact** (solver.py:237) — silently noted only.

---

## Iterations (ordered by points-per-dollar-hour × unblocking value)

### I1 — Web loop sharpening + hypothesis activation  *(medium)*
Close the 1/3 gap on the track that already works, before widening.
- Expand web prompt HYPOTHESIZE into an explicit **attack-class taxonomy** (SQLi subclasses,
  NoSQL operators, auth bypass, SSTI, traversal) + "if a class fails, switch CLASS not payload".
- Wire `graph.add_hypothesis` / `set_status(REFUTED)` / `dead_ends` into the solver loop; emit
  `SOLVE_GRAPH_DELTA` for proposals + status.
- Refactor loop guard to key on `(target, technique-class)` not raw 85% code text.
- Raise web `max_steps` 12→20; add a peek-usage hint to the web prompt.
- **Accept:** ≥2/3 on the 3-web NYU subset (no provenance regressions), OR unit tests proving
  hypotheses populate, a REFUTED hypothesis lands in `dead_ends`/`to_summary()`, and the guard
  passes sqli-union→sqli-blind but blocks a true triple-repeat.

### I2 — Crypto track SDK (RSA-first) + per-track eval subset  *(OBSOLETE — muteki_kit removed; CLI agents use their own crypto tools)*
Crypto = 26% of the bench, highest automation ceiling, lightweight deps (gmpy2/sympy).
- `muteki_kit/crypto/{rsa,classical,symmetric}.py`, typed Pydantic results + artifact peek.
- `RSABreaker(n,e,c).auto()`: small-e, common-modulus, Wiener, factordb, RsaCtfTool fallback.
- `requirements-crypto.txt` + `_check_deps()` probe; `prompts/crypto.py`.
- **Accept:** `test_kit_crypto.py` recovers plaintext on ≥3 classic vuln param sets (pure-math
  CI); crypto NYU subset solves ≥1 real challenge black-box, provenance intact.

### I3 — Reverse track SDK (decompile + disasm + best-effort symexec)  *(OBSOLETE — muteki_kit removed; CLI agents use their own RE tools)*
Rev = 25.5% of the bench. Heavier deps (Ghidra/pyghidra/angr) → ranked after crypto.
- `muteki_kit/reverse/{decompile,disasm,symexec}.py`; **always** route pseudocode through
  `save_artifact`+peek (never inline a 10KB decompilation → hallucination risk).
- `requirements-reverse.txt` + deps probe; `prompts/reverse.py` (decompile→peek-by-function).
- **Accept:** disasm a committed ELF + artifact roundtrip in CI; rev NYU subset solves ≥1 real
  crackme black-box with decompilation never inlined into message history.

### I4 — Forensics + misc SDK + triage→track routing  *(large; capstone that activates dispatch)*
forensics 7.5% + misc 12%; lower frequency but high-automation easy points. The triage→track
routing glue (currently absent) belongs here and **retroactively activates I2/I3 dispatch**.
- forensics: stego/LSB, PCAP reconstruction, binwalk carving, exif. misc: QR, esolang, spectrogram.
- `TRACK_SDK_MAP` + `get_sdk_for_track(category)` + category→prompt selector in `solver.run()`.
- **Accept:** all four SDKs importable; routing unit-tested (crypto/rev/forensics triage loads
  the matching prompt, not web); ≥1 stego/PCAP and ≥1 QR/encoding challenge solved black-box.

### I5 — Eval CI gate + ablation + per-track timeout policy  *(large; locks in the gains)*
By I5 all six tracks are attemptable; make it regression-proof.
- Two-tier eval: FAST mock-solver gate (every PR — proves provenance gate rejects hallucinated
  flags, no flag leakage, grading tolerance) + SLOW weekly black-box subset with a stored
  baseline that alerts on solve-drop/cost-spike.
- `AblationConfig` (swarm/insight/loop-guard on/off) → finally answer "is the swarm worth it?".
- `ChallengeTimeoutPolicy` per track (crypto.rsa 180s, reverse 300s, web 60s).
- Log rejected/hallucinated flags to an audit artifact.
- **Accept:** FAST gate green in CI and fails on a deliberately-broken-provenance branch;
  baseline file + delta check exist; ablation produces a single-solver-vs-swarm number.

---

## Quick wins (do immediately, before/within I1)
- Attack-class taxonomy prompt edit (cheapest solve-rate bump; ship before rest of I1).
- Multi-step-flag `print()` example in the web prompt discipline section.
- On kernel timeout, append directed next-step guidance to the condensed Result.
- Cap `InsightBus.history` to ~1000; bound terminal WS replay to a recent window.

## Deferred (explicitly NOT next — and why)
- **P5 coordinator / CTFBridge / auto-submit** — pure orchestration overhead until >1 track
  solves. Pull forward only when a live CTFd/rCTF contest is concretely scheduled.
- **microVM/Firecracker sandbox** — threat-model concern, not a solve-rate lever; local
  subprocess is fine for the trusted NYU env. Document as a known limitation.
- **EIG/entropy hypothesis reranking** — over-engineering vs a good static+RAG taxonomy (I1).
- **Cybench second benchmark** — add only after all six NYU tracks solve (generalization check).
- **Frontend polish** (full Next build, xterm PTY, replay UI) — doesn't move solve-rate.
- **HITL consumption wiring** — genuinely half-built, but a force-multiplier on working tracks,
  not a track-unlocker; fold into a later iteration once multi-track creates demand for steering.

## Open call to revisit
The synthesizer ranked forensics (15) above pwn (39) on automation-ceiling grounds (pwn binary
exploitation is fragile to automate). By raw bench frequency pwn is 2.6× forensics — if pwn
automation proves tractable (pwntools + a ret2libc/ret2win template), it may deserve to jump
ahead of I4. Decide after I2/I3 land and we see how the typed-result pattern holds for binaries.
