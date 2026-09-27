"""Codex custom endpoint binding via ``-c`` flags.

Conversation app-server and CLI ``exec`` both need explicit
``model_provider`` / ``model_providers.*`` overrides when Muteki points Codex
at a non-OpenAI endpoint:

* API_KEY + BASE_URL accounts inject ``OPENAI_BASE_URL`` and use a synthetic
  ``muteki`` provider block (same shape as Worker ``_codex_config_flags``).
* Login-style accounts only set ``CODEX_HOME`` (with a host-imported
  ``config.toml`` that already declares ``model_provider`` +
  ``[model_providers.*]``). App-server normally reads that file, but CLI
  metadata uses ``--ignore-user-config`` (kb_access=False) and would otherwise
  fall back to ``api.openai.com`` while still sending the Ark key from
  ``auth.json`` — HTTP 401. Re-emitting the provider block as ``-c`` fixes
  both paths.
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Mapping


def _muteki_base_url_flags(base_url: str, *, wire_api: str = "responses") -> list[str]:
    wire = (wire_api or "responses").strip() or "responses"
    return [
        "-c", "model_provider=muteki",
        "-c", "model_providers.muteki.name=muteki",
        "-c", f"model_providers.muteki.base_url={base_url}",
        "-c", f"model_providers.muteki.wire_api={wire}",
        "-c", "model_providers.muteki.env_key=OPENAI_API_KEY",
    ]


def _toml_scalar_flag(key: str, value: object) -> str | None:
    if isinstance(value, bool):
        return f"{key}={'true' if value else 'false'}"
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return f"{key}={value}"
    text = str(value or "").strip()
    if not text:
        return None
    return f"{key}={text}"


def flags_from_codex_home_config(codex_home: str | Path) -> list[str]:
    """Emit ``-c`` overrides from ``CODEX_HOME/config.toml`` when non-OpenAI."""
    root = Path(str(codex_home or "")).expanduser()
    if not str(codex_home or "").strip() or not root.is_dir():
        return []
    path = root / "config.toml"
    if not path.is_file():
        return []
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError):
        return []
    if not isinstance(data, dict):
        return []
    provider_id = str(data.get("model_provider") or "").strip()
    if not provider_id or provider_id in {"openai"}:
        return []
    providers = data.get("model_providers")
    if not isinstance(providers, dict):
        return []
    block = providers.get(provider_id)
    if not isinstance(block, dict):
        return []
    base_url = str(block.get("base_url") or "").strip()
    if not base_url:
        return []
    name = str(block.get("name") or provider_id).strip() or provider_id
    wire_api = str(block.get("wire_api") or "responses").strip() or "responses"
    flags = [
        "-c", f"model_provider={provider_id}",
        "-c", f"model_providers.{provider_id}.name={name}",
        "-c", f"model_providers.{provider_id}.base_url={base_url}",
        "-c", f"model_providers.{provider_id}.wire_api={wire_api}",
    ]
    for field in ("env_key", "requires_openai_auth"):
        if field not in block:
            continue
        rendered = _toml_scalar_flag(
            f"model_providers.{provider_id}.{field}", block[field]
        )
        if rendered:
            flags.extend(["-c", rendered])
    return flags


def codex_provider_spawn_args(env: Mapping[str, str]) -> list[str]:
    """Spawn/exec ``-c`` flags for the active Codex credential env."""
    base_url = str(env.get("OPENAI_BASE_URL") or "").strip()
    if base_url:
        wire_api = str(env.get("MUTEKI_CODEX_WIRE_API") or "responses").strip()
        return _muteki_base_url_flags(base_url, wire_api=wire_api)
    codex_home = str(env.get("CODEX_HOME") or "").strip()
    if codex_home:
        return flags_from_codex_home_config(codex_home)
    return []


def argv_has_model_provider(argv: list[str]) -> bool:
    """True when argv already pins ``model_provider`` via a ``-c`` override."""
    for index, arg in enumerate(argv):
        text = str(arg)
        if text.startswith("model_provider="):
            return True
        if text == "-c" and index + 1 < len(argv):
            if str(argv[index + 1]).startswith("model_provider="):
                return True
    return False


def inject_codex_provider_flags(argv: list[str], flags: list[str]) -> list[str]:
    """Insert provider ``-c`` flags immediately after ``exec`` / ``app-server``.

    Codex treats ``-c`` as a *subcommand* option. Placing overrides before the
    verb makes them global; with ``--ignore-user-config`` (conversation
    metadata, kb_access=False) those global ``-c`` flags are ignored and Codex
    falls back to ``model_provider=openai`` while still reading the Ark key
    from ``auth.json`` — HTTP 401 at api.openai.com. App-server already passes
    ``-c`` after the verb; CLI must do the same.
    """
    if not flags:
        return argv
    if not argv:
        return list(flags)
    for verb in ("exec", "app-server"):
        try:
            idx = argv.index(verb)
        except ValueError:
            continue
        return [*argv[:idx + 1], *flags, *argv[idx + 1:]]
    return [argv[0], *flags, *argv[1:]]
