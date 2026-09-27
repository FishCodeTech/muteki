"""Login Codex CLI must re-emit config.toml provider under --ignore-user-config."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from muteki.solver.cli_engines.argv import apply_runtime_argv
from muteki.solver.cli_engines.codex_provider import (
    codex_provider_spawn_args,
    flags_from_codex_home_config,
)


_ARK_TOML = "\n".join(
    [
        'model = "deepseek-v4.1-flash"',
        'model_provider = "ark"',
        "",
        "[model_providers.ark]",
        'name = "Volcengine Ark"',
        'base_url = "https://ark.cn-beijing.volces.com/api/plan/v3"',
        'wire_api = "responses"',
        "requires_openai_auth = true",
        "",
    ]
)


class CodexLoginProviderArgvTests(unittest.TestCase):
    def test_flags_from_config_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "config.toml").write_text(_ARK_TOML, encoding="utf-8")
            flags = flags_from_codex_home_config(home)
        self.assertEqual(flags[0:2], ["-c", "model_provider=ark"])
        self.assertIn(
            "model_providers.ark.base_url=https://ark.cn-beijing.volces.com/api/plan/v3",
            flags,
        )

    def test_apply_runtime_argv_injects_after_exec(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "config.toml").write_text(_ARK_TOML, encoding="utf-8")
            driver = SimpleNamespace(name="codex", env_extra=lambda: {})
            argv = [
                "codex",
                "exec",
                "--ignore-user-config",
                "--json",
                "--",
                "hi",
            ]
            out = apply_runtime_argv(
                argv,
                driver=driver,  # type: ignore[arg-type]
                env={"CODEX_HOME": str(home), "OPENAI_BASE_URL": ""},
            )
        # Subcommand -c must follow exec; global -c before exec is ignored
        # when --ignore-user-config is set (metadata path).
        self.assertLess(out.index("exec"), out.index("model_provider=ark"))
        self.assertLess(out.index("model_provider=ark"), out.index("--ignore-user-config"))
        self.assertIn("--ignore-user-config", out)
        # Provider block is present even though user config is ignored.
        joined = " ".join(out)
        self.assertIn("model_providers.ark.base_url=", joined)
        self.assertNotIn("api.openai.com", joined)

    def test_inject_helper_places_flags_after_verb(self) -> None:
        from muteki.solver.cli_engines.codex_provider import (
            inject_codex_provider_flags,
        )
        argv = ["codex", "exec", "--ignore-user-config", "--", "hi"]
        flags = ["-c", "model_provider=ark", "-c", "model_providers.ark.base_url=https://x"]
        out = inject_codex_provider_flags(argv, flags)
        self.assertEqual(
            out,
            [
                "codex",
                "exec",
                "-c", "model_provider=ark",
                "-c", "model_providers.ark.base_url=https://x",
                "--ignore-user-config",
                "--",
                "hi",
            ],
        )

    def test_existing_provider_flags_not_duplicated(self) -> None:
        driver = SimpleNamespace(name="codex", env_extra=lambda: {})
        argv = [
            "codex",
            "-c", "model_provider=muteki",
            "-c", "model_providers.muteki.base_url=https://x/v1",
            "exec",
            "--",
            "hi",
        ]
        out = apply_runtime_argv(
            argv,
            driver=driver,  # type: ignore[arg-type]
            env={"OPENAI_BASE_URL": "https://y/v1"},
        )
        self.assertEqual(out.count("model_provider=muteki"), 1)
        self.assertNotIn("model_provider=ark", out)

    def test_spawn_args_prefer_base_url(self) -> None:
        flags = codex_provider_spawn_args({
            "OPENAI_BASE_URL": "https://x/v1",
            "CODEX_HOME": "/nonexistent",
        })
        self.assertIn("model_provider=muteki", flags)


if __name__ == "__main__":
    unittest.main()
