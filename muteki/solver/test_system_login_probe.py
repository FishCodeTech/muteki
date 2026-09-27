"""Host-login listing must not spawn engine CLIs."""
from __future__ import annotations

import unittest
from unittest.mock import patch

from muteki.solver.credential_accounts import (
    detect_system_login,
    invalidate_system_login_cache,
)


class SystemLoginProbeTests(unittest.TestCase):
    def setUp(self) -> None:
        invalidate_system_login_cache()

    def tearDown(self) -> None:
        invalidate_system_login_cache()

    def test_claude_reads_oauth_and_does_not_spawn_cli(self) -> None:
        with patch(
            "muteki.solver.cli_driver._claude_oauth",
            return_value=("tok", 0),
        ) as oauth, patch(
            "muteki.solver.cli_driver.resolve_engine_bin",
        ) as resolve_bin:
            self.assertEqual(detect_system_login("claude", fresh=True), "present")
            oauth.assert_called_once()
            resolve_bin.assert_not_called()

    def test_claude_absent_without_spawning_cli(self) -> None:
        with patch(
            "muteki.solver.cli_driver._claude_oauth",
            return_value=None,
        ), patch(
            "muteki.solver.cli_driver.resolve_engine_bin",
        ) as resolve_bin:
            self.assertEqual(detect_system_login("claude", fresh=True), "absent")
            resolve_bin.assert_not_called()

    def test_host_login_cache_skips_second_probe(self) -> None:
        with patch(
            "muteki.solver.cli_driver._claude_oauth",
            return_value=("tok", 0),
        ) as oauth:
            self.assertEqual(detect_system_login("claude", fresh=True), "present")
            self.assertEqual(detect_system_login("claude"), "present")
            self.assertEqual(oauth.call_count, 1)

    def test_fresh_bypasses_cache(self) -> None:
        with patch(
            "muteki.solver.cli_driver._claude_oauth",
            side_effect=[("tok", 0), None],
        ) as oauth:
            self.assertEqual(detect_system_login("claude", fresh=True), "present")
            self.assertEqual(detect_system_login("claude", fresh=True), "absent")
            self.assertEqual(oauth.call_count, 2)

    def test_invalidate_clears_cache(self) -> None:
        with patch(
            "muteki.solver.cli_driver._claude_oauth",
            return_value=("tok", 0),
        ) as oauth:
            self.assertEqual(detect_system_login("claude", fresh=True), "present")
            invalidate_system_login_cache("claude")
            self.assertEqual(detect_system_login("claude"), "present")
            self.assertEqual(oauth.call_count, 2)


if __name__ == "__main__":
    unittest.main()
