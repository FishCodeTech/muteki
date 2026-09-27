"""Conversation Codex app-server must bind custom endpoints via -c flags."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.external_agents.codex import CodexAppServerAdapter


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


class ProviderSpawnArgsTests(unittest.TestCase):
    def test_empty_without_base_url_or_home(self) -> None:
        self.assertEqual(CodexAppServerAdapter._provider_spawn_args({}), [])
        self.assertEqual(
            CodexAppServerAdapter._provider_spawn_args({"OPENAI_API_KEY": "x"}),
            [],
        )

    def test_flags_match_cli_shape_for_base_url(self) -> None:
        args = CodexAppServerAdapter._provider_spawn_args({
            "OPENAI_BASE_URL": "https://ark.example/api/plan/v3",
            "OPENAI_API_KEY": "ark-x",
        })
        self.assertEqual(
            args,
            [
                "-c", "model_provider=muteki",
                "-c", "model_providers.muteki.name=muteki",
                "-c", "model_providers.muteki.base_url=https://ark.example/api/plan/v3",
                "-c", "model_providers.muteki.wire_api=responses",
                "-c", "model_providers.muteki.env_key=OPENAI_API_KEY",
            ],
        )

    def test_wire_api_override(self) -> None:
        args = CodexAppServerAdapter._provider_spawn_args({
            "OPENAI_BASE_URL": "https://example/v1",
            "MUTEKI_CODEX_WIRE_API": "chat",
        })
        self.assertIn("model_providers.muteki.wire_api=chat", args)

    def test_login_account_reads_config_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "config.toml").write_text(_ARK_TOML, encoding="utf-8")
            args = CodexAppServerAdapter._provider_spawn_args({
                "CODEX_HOME": str(home),
                "OPENAI_BASE_URL": "",
                "OPENAI_API_KEY": "",
            })
        self.assertEqual(args[0:2], ["-c", "model_provider=ark"])
        self.assertIn("model_providers.ark.base_url=https://ark.cn-beijing.volces.com/api/plan/v3", args)
        self.assertIn("model_providers.ark.wire_api=responses", args)
        self.assertIn("model_providers.ark.requires_openai_auth=true", args)
        self.assertNotIn("model_provider=muteki", args)
        self.assertNotIn("api.openai.com", " ".join(args))

    def test_base_url_wins_over_config_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "config.toml").write_text(_ARK_TOML, encoding="utf-8")
            args = CodexAppServerAdapter._provider_spawn_args({
                "CODEX_HOME": str(home),
                "OPENAI_BASE_URL": "https://custom.example/v1",
            })
        self.assertIn("model_provider=muteki", args)
        self.assertIn("model_providers.muteki.base_url=https://custom.example/v1", args)
        self.assertNotIn("model_provider=ark", args)

    def test_openai_default_config_emits_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            (home / "config.toml").write_text(
                'model_provider = "openai"\n', encoding="utf-8"
            )
            args = CodexAppServerAdapter._provider_spawn_args({
                "CODEX_HOME": str(home),
            })
        self.assertEqual(args, [])


if __name__ == "__main__":
    unittest.main()
