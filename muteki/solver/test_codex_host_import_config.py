"""Host Codex import must copy config.toml (custom provider binding)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from muteki.solver.credential_accounts import CredentialAccountStore


class ImportHostCodexConfigTests(unittest.TestCase):
    def test_import_copies_host_config_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            host = root / "host-codex"
            host.mkdir()
            (host / "auth.json").write_text(
                json.dumps({"OPENAI_API_KEY": "ark-test-key"}), encoding="utf-8"
            )
            (host / "config.toml").write_text(
                "\n".join(
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
                ),
                encoding="utf-8",
            )
            store = CredentialAccountStore(root / "accounts")
            with mock.patch.object(Path, "home", return_value=root):
                # Path.home()/.codex → need home/.codex
                pass
            fake_home = root / "home"
            fake_home.mkdir()
            (fake_home / ".codex").symlink_to(host)
            with mock.patch(
                "muteki.solver.credential_accounts.Path.home",
                return_value=fake_home,
            ):
                store.import_host_codex_auth("codex-account")

            dest = root / "accounts" / "codex-account" / "codex-home"
            self.assertTrue((dest / "auth.json").is_file())
            self.assertTrue((dest / "config.toml").is_file())
            text = (dest / "config.toml").read_text(encoding="utf-8")
            self.assertIn("model_provider = \"ark\"", text)
            self.assertIn("ark.cn-beijing.volces.com", text)
            self.assertNotIn("ark-test-key", text)

    def test_import_without_host_config_still_writes_auth(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake_home = root / "home"
            codex = fake_home / ".codex"
            codex.mkdir(parents=True)
            (codex / "auth.json").write_text(
                json.dumps({"OPENAI_API_KEY": "sk-test"}), encoding="utf-8"
            )
            store = CredentialAccountStore(root / "accounts")
            with mock.patch(
                "muteki.solver.credential_accounts.Path.home",
                return_value=fake_home,
            ):
                store.import_host_codex_auth("codex-account")
            dest = root / "accounts" / "codex-account" / "codex-home"
            self.assertTrue((dest / "auth.json").is_file())
            self.assertFalse((dest / "config.toml").exists())


if __name__ == "__main__":
    unittest.main()
