"""Shelled-CLI worker drivers — claude / codex as full agentic executors.

Why: the local DeepSeek code-driven kernel (one run_python tool-call per step)
lacks the execute→observe→refine depth to actually land an exploit. EXP-AB proved
a shelled `claude -p` solves challenges the code-driven swarm misses, and its flag
still passes the real provenance gate. So we delegate a focused intent to a CLI
agent that runs its OWN shell loop, and gate its output exactly as before.

Each driver is a thin per-CLI adapter: it builds argv and manages the session id.
The solver may resume that same session for bounded CTF execution/checkpoint slices
or for one post-run response; the driver itself remains stateless.
We run bare-host against the
SUBSCRIPTION CLIs (full-strength model — the reason it solves). codex is included
but may be usage-limited; the swarm degrades to claude-only when a driver's
healthcheck fails.

This module is pure (builds argv + parses output); the solver runs the subprocess.
"""

from muteki.solver.cli_engines import *  # noqa: F401,F403
from muteki.solver.cli_engines import __all__ as __all__  # noqa: F401
