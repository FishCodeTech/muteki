"""The provenance + format flag-acceptance gate (muteki.solver.gate).

This replaces the old test_verifier_gate.py: the standalone gate.flag_ok is now
the ONE acceptance check every executor shares. A flag counts only if it (a)
matches the challenge flag format AND (b) is traceable to real execution output —
verbatim in raw_output, or in an artifact referenced by it. A hallucinated flag
cannot be laundered through prose or a side channel.
"""

from __future__ import annotations

from muteki.solver import gate
from muteki.solver.result import ArtifactStore

FMT = r"flag\{[^}]{1,200}\}"


# ── referenced_artifacts ─────────────────────────────────────────────────────
def test_referenced_artifacts_extracts_ids():
    txt = "see artifact_deadbeef12 and artifact 0011223344 for details"
    ids = gate.referenced_artifacts(txt)
    assert "deadbeef12" in ids
    assert "0011223344" in ids


def test_referenced_artifacts_ignores_short_or_nonhex():
    # needs 8+ hex chars; 'artifactzz' / short ids must not match
    assert gate.referenced_artifacts("artifact_zz no artifact_abc") == []


# ── format gate ──────────────────────────────────────────────────────────────
def test_wrong_format_rejected(tmp_path):
    art = ArtifactStore(root=str(tmp_path / "a"))
    # right value, wrong shape → never accepted, even if present in output
    assert gate.flag_ok("CTF{x}", "the answer is CTF{x}", flag_format=FMT, artifacts=art) is False


# ── verbatim provenance ──────────────────────────────────────────────────────
def test_verbatim_in_output_accepted(tmp_path):
    art = ArtifactStore(root=str(tmp_path / "a"))
    out = "exploit succeeded, server returned flag{pwn3d_it} in the body"
    assert gate.flag_ok("flag{pwn3d_it}", out, flag_format=FMT, artifacts=art) is True


def test_format_match_but_not_in_output_rejected(tmp_path):
    # well-formed flag the model just CLAIMS — not traceable to any output.
    art = ArtifactStore(root=str(tmp_path / "a"))
    assert gate.flag_ok("flag{hallucinated}", "I think the flag is the one above",
                        flag_format=FMT, artifacts=art) is False


# ── artifact provenance ──────────────────────────────────────────────────────
def test_flag_in_referenced_artifact_accepted(tmp_path):
    art = ArtifactStore(root=str(tmp_path / "a"))
    aid = art.put("decoded payload:\nflag{from_artifact}\nEOF")
    out = f"decoded the blob, full dump saved as artifact_{aid}"
    assert gate.flag_ok("flag{from_artifact}", out, flag_format=FMT, artifacts=art) is True


def test_flag_in_unreferenced_artifact_rejected(tmp_path):
    # the artifact exists and contains the flag, but output never references it →
    # no provenance link, so reject (can't grep every artifact for any string).
    art = ArtifactStore(root=str(tmp_path / "a"))
    art.put("flag{from_artifact}")  # aid never mentioned in output
    assert gate.flag_ok("flag{from_artifact}", "ran the exploit, see logs",
                        flag_format=FMT, artifacts=art) is False


def test_referenced_artifact_without_flag_rejected(tmp_path):
    art = ArtifactStore(root=str(tmp_path / "a"))
    aid = art.put("totally unrelated disassembly output xyz")
    out = f"see artifact_{aid}"
    assert gate.flag_ok("flag{not_here}", out, flag_format=FMT, artifacts=art) is False


def test_missing_artifact_id_rejected(tmp_path):
    # output references an id that was never stored → read_text returns None.
    art = ArtifactStore(root=str(tmp_path / "a"))
    out = "see artifact_deadbeef00"  # never put()
    assert gate.flag_ok("flag{nope}", out, flag_format=FMT, artifacts=art) is False


def test_none_artifacts_store_only_verbatim(tmp_path):
    # with no store, only verbatim-in-output can pass; artifact path is skipped safely.
    assert gate.flag_ok("flag{ok}", "flag{ok}", flag_format=FMT, artifacts=None) is True
    assert gate.flag_ok("flag{ok}", "see artifact_deadbeef00", flag_format=FMT, artifacts=None) is False


# ── placeholder rejection (the run-1619 / run-0405 false-positive class) ──────
# A worker that did NOT solve still got marked solved because it mentioned a
# template like flag{...} or {uuid} in its prose: a loose flag_format matched it,
# and the self-referential "appears in output" check trivially passed. The gate
# must reject these templates outright.

# loose format that matches both flag{...} and {uuid} (mirrors the real challenge)
LOOSE = r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}"


def test_is_placeholder_flag_detects_templates():
    for p in ["flag{...}", "flag{…}", "{uuid}", "<flag>", "<the flag>", "<...>",
              "flag{FLAG}", "flag{flag}", "flag{your_flag_here}", "flag{redacted}",
              "flag{xxxx}", "flag{TODO}", "flag{____}", "flag{....}", "flag{}",
              "flag{ }", "flag{placeholder}", "flag{16fc0d69-...}",
              "flag{4b9581e7…}"]:
        assert gate.is_placeholder_flag(p) is True, p


def test_is_placeholder_flag_passes_real_flags():
    # words that COULD be genuine flag content must NOT be rejected — only
    # unambiguous templates are. flag{real}/flag{example} stay valid.
    for f in ["dalctf{7h4t_w45_3xp3n5Iv3_4f}", "csawctf{d0nt_smuggl3_Fla6s_!}",
              "flag{cheaper_prices_in_NYC_PLZ}", "HTB{1f9b0765ee03e69aa1822fa7f0027d35}",
              "flag{a}", "flag{real}", "flag{example_solved}", "flag{x}",
              "ctfhub{648eeeb042153d9c2a1f4721}"]:
        assert gate.is_placeholder_flag(f) is False, f


def test_gate_rejects_flag_dotdotdot_from_prose_run_1619():
    # the exact run-1619 shape: the placeholder appears verbatim in the worker's
    # prose, so the old self-referential check would have accepted it.
    prose = ("The forged-admin /admin HTML did not contain flag{...}. "
             "I'm scanning the returned pages for flag{...}.")
    assert "flag{...}" in prose                                   # would satisfy old check
    assert gate.flag_ok("flag{...}", prose, flag_format=LOOSE, artifacts=None) is False


def test_gate_rejects_truncated_flag_summary_from_prose():
    """run-61718: a coordinator summary wrote `flag{16fc0d69-...}` as shorthand.
    The old placeholder rule rejected only a pure `flag{...}` body, so this
    truncated summary was accepted as a fourth distinct flag."""
    prose = "already have flag{16fc0d69-...}; keep hunting the remaining host"
    assert gate.flag_ok("flag{16fc0d69-...}", prose,
                        flag_format=LOOSE, artifacts=None) is False


def test_gate_rejects_uuid_placeholder_run_0405():
    out = "the team board says we found {uuid} earlier"
    assert gate.flag_ok("{uuid}", out, flag_format=LOOSE, artifacts=None) is False


def test_gate_still_accepts_real_flag_in_output():
    # the fix must not regress real solves.
    raw = "FOUND_FLAG=dalctf{7h4t_w45_3xp3n5Iv3_4f}\nrecovered from /order/7 Set-Cookie"
    assert gate.flag_ok("dalctf{7h4t_w45_3xp3n5Iv3_4f}", raw,
                        flag_format=LOOSE, artifacts=None) is True


# ── bare-brace code-template placeholders (run-3613: {name}) ──────────────────
# Web workers quote URL/code templates like {name}, {id}, {comp} from prose. These
# match a loose flag_format (0-char prefix allowed) and self-satisfy provenance.
# Every REAL flag in history carried a prefix (dalctf{/HTB{/flag{…}); the only
# prefix-less {...} ever accepted were all false positives — so bare braces are
# placeholders unless the body already looks like a recovered flag.

def test_bare_brace_templates_are_placeholders():
    for p in ["{name}", "{uuid}", "{id}", "{comp}", "{path}", "{country}",
              "{1,2,66,67,68}", "{username}", "{img}", "{token}",
              "{out3[i:j].decode()}"]:
        assert gate.is_placeholder_flag(p) is True, p


def test_prefixed_real_flags_not_dropped_by_bare_rule():
    # the bare-brace rule must not touch flags that carry a prefix.
    for f in ["dalctf{h3lv3t3}", "flag{name}", "HTB{1f9b0765ee03e69aa1822fa7f0027d35}",
              "csawctf{d0nt_smuggl3_Fla6s_!}", "DalCTF{533m1ng1y_r4nd0m1y_g3n3r473d_num63rs}"]:
        assert gate.is_placeholder_flag(f) is False, f


def test_gate_rejects_name_template_run_3613():
    # run-3613: claude described main.js (`icon={name}.ico`) and the blind scan
    # grabbed the URL template {name} as the flag.
    prose = ("main.js writes the 'icon' cookie client-side as icon={name}.ico but "
             "its read path validates against a hardcoded ICON_NAMES whitelist")
    assert "{name}" in prose
    assert gate.flag_ok("{name}", prose, flag_format=LOOSE, artifacts=None) is False


def test_gate_rejects_number_set_template():
    # run-3613-class: {1,2,66,67,68} was a port/id range quoted in prose.
    out = "the candidate ids were {1,2,66,67,68}"
    assert gate.flag_ok("{1,2,66,67,68}", out, flag_format=LOOSE, artifacts=None) is False


def test_gate_rejects_bare_brace_code_expression_from_found_flag_source():
    # run-0835: a worker pasted Python source containing an f-string marker and
    # the extractor grabbed the template expression rather than command output.
    out = 'print(f"FOUND_FLAG={out3[i:j].decode()}")'
    assert gate.flag_ok("{out3[i:j].decode()}", out,
                        flag_format=LOOSE, artifacts=None) is False


# ── bare-brace comma-set code literals (run-1763) ────────────────────────────
# A bare {...} whose body is a comma-separated set/list is a code literal the worker
# quoted (Python set, BLOCKED_HOSTS), not a flag — even if it has letters+digits.
def test_bare_brace_comma_sets_are_placeholders():
    for p in ["{1,2,66,67,68}", "{127.0.0.1, localhost, 0.0.0.0, ::1}",
              "{a, b, c}", "{GET, POST, PUT}"]:
        assert gate.is_placeholder_flag(p) is True, p


def test_prefixed_flag_with_comma_still_accepted():
    # the comma rule is scoped to BARE braces — a prefixed flag is never touched.
    for f in ["flag{a,b,c}", "dalctf{list,of,things}"]:
        assert gate.is_placeholder_flag(f) is False, f


def test_gate_rejects_blocked_hosts_set_run_1763():
    out = "BLOCKED_HOSTS={127.0.0.1, localhost, 0.0.0.0, ::1} in the is_blocked() decoy"
    assert gate.flag_ok("{127.0.0.1, localhost, 0.0.0.0, ::1}", out,
                        flag_format=LOOSE, artifacts=None) is False


# ── token mode: bare-token flags (run-10070 Bandit-style ladder) ─────────────
# A challenge whose "flag" is a bare secret (W3lc0m3T0Gh0st), not flag{...}. The
# operator sets flag_format="token". The brace match is swapped for a strength
# floor; provenance + placeholder checks stay, so the moat holds.
TOKEN = gate.TOKEN_FLAG_FORMAT


def test_token_mode_accepts_strong_token_in_output():
    out = "ghost1 login succeeds, password is W3lc0m3T0Gh0st, whoami=ghost1"
    assert gate.flag_ok("W3lc0m3T0Gh0st", out, flag_format=TOKEN, artifacts=None) is True


def test_token_mode_rejects_token_not_in_output():
    # provenance moat unchanged: a token the worker didn't actually recover is out.
    out = "ghost1 login succeeds, password is W3lc0m3T0Gh0st"
    assert gate.flag_ok("D1ff3r3ntS3cr3t", out, flag_format=TOKEN, artifacts=None) is False


def test_token_mode_rejects_weak_or_common_tokens():
    out = "the password password admin ghost 12345678 abcdefgh appears here verbatim"
    for weak in ["password", "admin", "ghost", "12345678", "abcdefgh"]:
        assert gate.flag_ok(weak, out, flag_format=TOKEN, artifacts=None) is False, weak


def test_token_mode_rejects_prose_with_spaces():
    out = "the flag is here somewhere in this sentence"
    assert gate.flag_ok("the flag is here", out, flag_format=TOKEN, artifacts=None) is False


def test_token_mode_accepts_separator_token_without_digit():
    out = "recovered the secret Gr3p_F1nds_Truth from the vault"
    assert gate.flag_ok("Gr3p_F1nds_Truth", out, flag_format=TOKEN, artifacts=None) is True


def test_token_mode_via_artifact(tmp_path):
    art = ArtifactStore(root=tmp_path)
    aid = art.put("ghost4 password P3rm1ss10ns_M4tt3r")
    out = f"see artifact_{aid}"
    assert gate.flag_ok("P3rm1ss10ns_M4tt3r", out, flag_format=TOKEN, artifacts=art) is True


def test_brace_mode_unchanged_by_token_addition(tmp_path):
    # a bare token is STILL rejected under a brace format (no regression).
    out = "ghost1 password is W3lc0m3T0Gh0st"
    assert gate.flag_ok("W3lc0m3T0Gh0st", out, flag_format=LOOSE, artifacts=None) is False
    assert gate.flag_ok("flag{real_win}", "got flag{real_win}", flag_format=LOOSE,
                        artifacts=None) is True


def test_looks_like_real_token_unit():
    ok = ["W3lc0m3T0Gh0st", "P3rm1ss10ns_M4tt3r", "Gr3p_F1nds_Truth", "D3c0d3_0r_D13"]
    bad = ["password", "admin", "ghost1", "short", "abcdefgh", "12345678", "a b c d"]
    for t in ok:
        assert gate._looks_like_real_token(t) is True, t
    for t in bad:
        assert gate._looks_like_real_token(t) is False, t


def test_token_mode_rejects_shell_regex_metachar_patterns():
    """run-11550 regression: a worker grepping its own process list / files for
    `FOUND_FLAG=bl_...` puts that grep PATTERN (with regex/shell metachars) into the
    command text. The marker scanner caught it and the strength floor (alpha+digit+
    sep) otherwise passed it → a false flag `bl_|VERIFIED_FACT=.*L4|...` got
    registered. A real bare-token flag is an opaque secret and NEVER contains pipes,
    globs, quantifiers, redirects, or command separators — reject them."""
    fakes = [
        "bl_|VERIFIED_FACT=.*L4|last-assistant-message",  # the actual run-11550 leak
        "bl_abc|def",            # pipe
        "FOUND_FLAG=bl_.*",      # glob/regex
        "bl_abc;rm",             # command separator
        "$(whoami)_token",       # command substitution
        "bl_abc&background",     # background
        "tok>redirect",          # redirect
        "tok[a-z]+",             # char class
    ]
    for f in fakes:
        assert gate._looks_like_real_token(f) is False, f
        assert gate.flag_ok(f, "grep " + f, flag_format=gate.TOKEN_FLAG_FORMAT,
                            artifacts=None) is False, f
    # real bl_-style flags (pure hex secret) still pass
    for real in ["bl_02ee57c1b60a8f14ca9228de95dbb1d1", "bl_eaa6a7c0438aff52e30a40f4ce171ffe"]:
        assert gate._looks_like_real_token(real) is True, real


def test_token_mode_rejects_rockyou_words_and_sentences(tmp_path):
    # review (gate-moat): a bare RockYou-class word or a quoted sentence the worker
    # echoed must NOT be accepted in token mode (the strength floor catches them).
    rockyou = ["password", "iloveyou", "princess", "sunshine", "football", "welcome",
               "qwerty", "monkey", "dragon"]
    out = " ".join(rockyou) + " the flag is the admin password somewhere here"
    for w in rockyou:
        assert gate.flag_ok(w, out, flag_format=gate.TOKEN_FLAG_FORMAT,
                            artifacts=None) is False, w
    # a whole quoted sentence is rejected (no-whitespace floor)
    assert gate.flag_ok("the flag is the admin password", out,
                        flag_format=gate.TOKEN_FLAG_FORMAT, artifacts=None) is False

