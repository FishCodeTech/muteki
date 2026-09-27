"""Engine binary resolution and KB MCP name. Moved from cli_driver.py."""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path


# ── engine binary resolution ─────────────────────────────────────────────────
# A worker shells `subprocess.run(["claude", ...])`, which resolves the FIRST
# `claude` on PATH. On this host (and easily on others) that can be a BROKEN
# third-party repackage — e.g. `@cometix/claude-code`, a Node "restored" build
# that crashes at parse time (`SyntaxError: Unexpected identifier`) under an
# older Node, never reaching the CLI. A worker pointed at it dies before it can
# solve, and the healthcheck just sees a non-zero exit and silently degrades the
# swarm. So we DON'T trust bare PATH order: resolve each engine to a real,
# runnable OFFICIAL binary and pin it.
#
# Precedence:
#   1. explicit override  — env MUTEKI_CLAUDE_BIN / MUTEKI_CODEX_BIN (operator wins)
#   2. known official install locations, in order
#   3. every `name` on PATH, skipping ones whose realpath looks like a known
#      bad repackage (cometix), taking the first that actually runs
#   4. bare `name` as a last resort (preserves old behavior if nothing else found)
_ENV_OVERRIDE = {
    "claude": "MUTEKI_CLAUDE_BIN",
    "codex": "MUTEKI_CODEX_BIN",
    "cursor": "MUTEKI_CURSOR_BIN",
    "pi": "MUTEKI_PI_BIN",
    "omp": "MUTEKI_OMP_BIN",
    "kimi": "MUTEKI_KIMI_BIN",
    "grok": "MUTEKI_GROK_BIN",
    "opencode": "MUTEKI_OPENCODE_BIN",
    "devin": "MUTEKI_DEVIN_BIN",
}

# The on-disk binary basename for an engine, when it differs from the engine
# `name` we use everywhere else. Cursor's headless CLI ships as `cursor-agent`
# (the bare `cursor` launcher opens the GUI / is a different tool), so a PATH
# scan for the engine "cursor" must actually look for `cursor-agent`.
_BIN_NAME = {"cursor": "cursor-agent"}

# Official / first-party install locations we trust, highest first. `~` expanded
# at resolve time. The local native installer and Homebrew cask are the two
# blessed macOS paths; /usr/local/bin covers a plain npm global on Linux.
_KNOWN_GOOD = {
    "claude": [
        "~/.local/bin/claude",
        "/opt/homebrew/bin/claude",
        "/usr/local/bin/claude",
    ],
    "codex": [
        "~/.local/bin/codex",
        "/opt/homebrew/bin/codex",
        "/usr/local/bin/codex",
    ],
    "cursor": [
        "~/.local/bin/cursor-agent",
        "/opt/homebrew/bin/cursor-agent",
        "/usr/local/bin/cursor-agent",
    ],
    "pi": [
        "~/.local/bin/pi",
        "/opt/homebrew/bin/pi",
        "/usr/local/bin/pi",
    ],
    # omp is bun-based; the omp.sh installer lands in ~/.bun/bin by default.
    "omp": [
        "~/.bun/bin/omp",
        "~/.local/bin/omp",
        "/opt/homebrew/bin/omp",
        "/usr/local/bin/omp",
    ],
    "kimi": [
        "~/.kimi-code/bin/kimi",
        "~/.local/bin/kimi",
        "/opt/homebrew/bin/kimi",
        "/usr/local/bin/kimi",
    ],
    "grok": [
        "~/.grok/bin/grok",
        "~/.local/bin/grok",
        "/opt/homebrew/bin/grok",
        "/usr/local/bin/grok",
    ],
    "opencode": [
        "~/.local/bin/opencode",
        "/opt/homebrew/bin/opencode",
        "/usr/local/bin/opencode",
    ],
    "devin": [
        "~/.local/bin/devin",
        "/opt/homebrew/bin/devin",
        "/usr/local/bin/devin",
    ],
}

# realpath substrings that mark a KNOWN-BAD repackage we must never select.
_BAD_REALPATH_MARKERS = ("@cometix", "cometix")

# Optional knowledge-base MCP. Muteki can let a worker query a KB MCP (your own
# security-intel / CVE / writeup index) as a first-class tool. There is no bundled
# KB service — set MUTEKI_KB_MCP_NAME to the server key from your .mcp.json (and
# enable kb on the run) to use one. Empty (the default) means "no KB", so the
# whole KB path is inert out of the box.
KB_MCP_NAME = os.environ.get("MUTEKI_KB_MCP_NAME", "").strip()

def _looks_bad(path: str) -> bool:
    try:
        real = os.path.realpath(path)
    except OSError:
        real = path
    low = real.lower()
    return any(m in low for m in _BAD_REALPATH_MARKERS)


def _runs_ok(path: str) -> bool:
    """Does this binary actually execute (vs crash at load like the cometix build)?
    `--version` is the cheapest probe that distinguishes a real CLI from a binary
    that dies before parsing argv."""
    try:
        r = subprocess.run([path, "--version"], capture_output=True, text=True, encoding="utf-8", errors="replace",
                           timeout=20)
        return r.returncode == 0
    except Exception:
        return False


def resolve_engine_bin(name: str) -> str:
    """Resolve an engine name to a pinned, runnable binary path (see precedence
    above). Falls back to the bare name so callers always get *something*."""
    # 1. operator override — trusted as-is (don't second-guess an explicit path)
    env = _ENV_OVERRIDE.get(name)
    if env and os.environ.get(env):
        return os.path.expanduser(os.environ[env])

    # 2. known-good install locations
    for cand in _KNOWN_GOOD.get(name, []):
        p = os.path.expanduser(cand)
        if Path(p).exists() and not _looks_bad(p) and _runs_ok(p):
            return p

    # 3. PATH scan, skipping known-bad repackages, first that runs wins. The
    #    on-disk basename may differ from the engine name (cursor → cursor-agent).
    bin_basename = _BIN_NAME.get(name, name)
    for p in _which_all(bin_basename):
        if not _looks_bad(p) and _runs_ok(p):
            return p

    # 4. last resort — bare basename (old behavior). If everything is broken we
    #    at least fail the same way we used to, not worse.
    return bin_basename


def resolve_engine_bin_source(name: str) -> str:
    """Where would resolve_engine_bin() get this engine's binary from?

    Returns one of: "env" (explicit MUTEKI_*_BIN override), "known-good" (a
    blessed install location), "path" (a PATH scan hit), or "fallback" (nothing
    found — bare name). Drives the FE's "you're on an unpinned default path,
    consider setting MUTEKI_<ENGINE>_BIN" guidance for local mode.
    """
    env = _ENV_OVERRIDE.get(name)
    if env and os.environ.get(env):
        return "env"
    for cand in _KNOWN_GOOD.get(name, []):
        p = os.path.expanduser(cand)
        if Path(p).exists() and not _looks_bad(p) and _runs_ok(p):
            return "known-good"
    bin_basename = _BIN_NAME.get(name, name)
    for p in _which_all(bin_basename):
        if not _looks_bad(p) and _runs_ok(p):
            return "path"
    return "fallback"


def _which_all(name: str) -> list[str]:
    """Every `name` found on PATH, in PATH order (shutil.which only returns one)."""
    out: list[str] = []
    seen: set[str] = set()
    for d in (os.environ.get("PATH") or "").split(os.pathsep):
        if not d:
            continue
        cand = os.path.join(d, name)
        if cand not in seen and os.path.isfile(cand) and os.access(cand, os.X_OK):
            seen.add(cand)
            out.append(cand)
    # also let shutil.which have a say (handles PATHEXT etc.) as a backstop
    w = shutil.which(name)
    if w and w not in seen:
        out.append(w)
    return out
