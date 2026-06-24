"""Optional `.env` loading for local convenience.

The code reads every secret/config from `os.environ` (never from a file) — see
`muteki/core/llm.py`. This module adds a *local-only* convenience: if a `.env`
sits at the repo root, load it into the environment at process startup.

Discipline preserved:
- `.gitignore` blocks `.env` / `.env.*`, so a real key still never reaches git.
- Real environment variables WIN: `load_env()` never overrides a var already set
  in the shell (`override=False`), so `MUTEKI_DEEPSEEK_API_KEY=... uv run ...`
  keeps working exactly as before.
- It is opt-in at the entrypoints only (eval scripts + web server). Library code
  and the kernel do NOT call this — they stay pure `os.environ` consumers.

Idempotent: safe to call from multiple entrypoints; only the first call loads.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Optional

# repo root = three levels up from this file (muteki/core/dotenv_boot.py)
_REPO_ROOT = Path(__file__).resolve().parents[2]

_loaded = False


def load_env(path: Optional[Path] = None) -> bool:
    """Load `.env` (repo root by default) into os.environ, once.

    Returns True if a file was found and loaded, False otherwise (missing file,
    already loaded, or python-dotenv not installed). Never raises — a broken or
    absent `.env` must not stop a run from starting.
    """
    global _loaded
    if _loaded:
        return False
    _loaded = True

    env_path = Path(path) if path is not None else _REPO_ROOT / ".env"
    if not env_path.is_file():
        return False

    try:
        from dotenv import load_dotenv
    except ImportError:
        return False

    # override=False: a var already exported in the shell wins over the file.
    return load_dotenv(dotenv_path=env_path, override=False)
