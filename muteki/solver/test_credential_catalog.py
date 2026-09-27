"""Conversation/Worker credential catalog only projects Worker engines."""
from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.solver.credential_accounts import CredentialAccountStore
from muteki.solver.engine_registry import SUPPORTED_ENGINE_IDS


class CredentialCatalogWorkerEngineTests(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.store = CredentialAccountStore(self.root)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_catalog_omits_api_endpoints_and_empty_unknown_dirs(self) -> None:
        self.store.upsert_secret(
            account_id="planner-http",
            engine="api",
            secret="sk-test",
            base_url="http://127.0.0.1:11434/v1",
            target_model="llama",
        )
        self.store.upsert_secret(
            account_id="pi-custom",
            engine="api",
            secret="sk-test",
            base_url="http://127.0.0.1:11434/v1",
            target_engine="pi",
            target_model="llama",
        )
        (self.root / "orphan-empty").mkdir()

        rows = self.store.credential_catalog(engines=())
        engines = {str(row.get("engine") or "") for row in rows}
        account_ids = {str(row.get("account_id") or "") for row in rows}

        self.assertEqual(engines, {"pi"})
        self.assertLessEqual(engines, set(SUPPORTED_ENGINE_IDS))
        self.assertIn("pi-custom", account_ids)
        self.assertNotIn("planner-http", account_ids)
        self.assertNotIn("orphan-empty", account_ids)
        self.assertNotIn("api", engines)
        self.assertNotIn("unknown", engines)


if __name__ == "__main__":
    unittest.main()
