"""File-channel delivery of the frozen CTF worker contract."""
from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.solver.cli_prompts import CTF_ROLE_CONTRACT, _CTF_WORKER_SYSTEM
from muteki.solver.context_manifest import ContextManifest
from muteki.solver.worker_skills import (
    ROLE_CONTRACT_MARKER,
    role_contract_filename,
    stage_role_contract,
)


class RoleContractTests(unittest.TestCase):
    def test_frozen_alias_is_the_same_object(self) -> None:
        self.assertIs(CTF_ROLE_CONTRACT, _CTF_WORKER_SYSTEM)
        self.assertTrue(_CTF_WORKER_SYSTEM.startswith("完成交给你的一个 step"))
        self.assertIn("submit-fact", _CTF_WORKER_SYSTEM)
        self.assertIn("commit-step", _CTF_WORKER_SYSTEM)

    def test_filename_uses_engine_name_not_class(self) -> None:
        self.assertEqual(role_contract_filename("claude"), "CLAUDE.md")
        self.assertEqual(role_contract_filename("grok"), "AGENTS.md")
        self.assertEqual(role_contract_filename("codex"), "AGENTS.md")
        self.assertEqual(role_contract_filename("pi"), "AGENTS.md")

    def test_pi_is_noop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="pi")
            self.assertEqual(staged.channel, "noop")
            self.assertFalse((Path(tmp) / "AGENTS.md").exists())
            self.assertFalse((Path(tmp) / "CLAUDE.md").exists())

    def test_omp_is_noop_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="omp")
            self.assertEqual(staged.channel, "noop")
            self.assertEqual(staged.reason, "omp_system")
            self.assertFalse((Path(tmp) / "AGENTS.md").exists())

    def test_codex_writes_agents_md(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="codex")
            dest = Path(tmp) / "AGENTS.md"
            self.assertEqual(staged.channel, "file")
            self.assertTrue(dest.is_file())
            text = dest.read_text(encoding="utf-8")
            self.assertTrue(text.startswith(ROLE_CONTRACT_MARKER))
            self.assertIn(_CTF_WORKER_SYSTEM, text)
            self.assertFalse((Path(tmp) / "CLAUDE.md").exists())

    def test_claude_writes_claude_md_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="claude")
            self.assertEqual(staged.channel, "file")
            self.assertTrue((Path(tmp) / "CLAUDE.md").is_file())
            self.assertFalse((Path(tmp) / "AGENTS.md").exists())

    def test_grok_writes_agents_not_claude(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="grok")
            self.assertEqual(staged.channel, "file")
            self.assertTrue((Path(tmp) / "AGENTS.md").is_file())
            self.assertFalse((Path(tmp) / "CLAUDE.md").exists())

    def test_collision_folds_and_does_not_overwrite(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "AGENTS.md"
            dest.write_text("operator attachment\n", encoding="utf-8")
            staged = stage_role_contract(tmp, engine="cursor")
            self.assertEqual(staged.channel, "user_fold")
            self.assertEqual(staged.reason, "collision")
            self.assertEqual(dest.read_text(encoding="utf-8"), "operator attachment\n")
            self.assertIn("submit-fact", staged.folded_text)

    def test_symlink_folds(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "other.md"
            target.write_text("keep\n", encoding="utf-8")
            dest = Path(tmp) / "AGENTS.md"
            dest.symlink_to(target)
            staged = stage_role_contract(tmp, engine="kimi")
            self.assertEqual(staged.channel, "user_fold")
            self.assertEqual(staged.reason, "symlink")
            self.assertTrue(dest.is_symlink())
            self.assertEqual(target.read_text(encoding="utf-8"), "keep\n")

    def test_own_file_can_be_restaged(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            first = stage_role_contract(tmp, engine="opencode")
            second = stage_role_contract(tmp, engine="opencode")
            self.assertEqual(first.channel, "file")
            self.assertEqual(second.channel, "file")
            self.assertTrue((Path(tmp) / "AGENTS.md").read_text(encoding="utf-8").startswith(ROLE_CONTRACT_MARKER))

    def test_claude_bare_folds_without_writing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="claude", bare=True)
            self.assertEqual(staged.channel, "user_fold")
            self.assertEqual(staged.reason, "bare")
            self.assertFalse((Path(tmp) / "CLAUDE.md").exists())

    def test_grok_gitignore_folds(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(tmp, engine="grok", gitignore_blocked=True)
            self.assertEqual(staged.channel, "user_fold")
            self.assertEqual(staged.reason, "gitignore")
            self.assertFalse((Path(tmp) / "AGENTS.md").exists())

    def test_pentest_is_noop(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            staged = stage_role_contract(
                tmp, engine="codex", challenge_mode="pentest",
            )
            self.assertEqual(staged.channel, "noop")
            self.assertFalse((Path(tmp) / "AGENTS.md").exists())

    def test_missing_cwd_folds(self) -> None:
        staged = stage_role_contract("", engine="cursor")
        self.assertEqual(staged.channel, "user_fold")
        self.assertEqual(staged.reason, "missing_cwd")
        self.assertIn("submit-fact", staged.folded_text)

    def test_manifest_records_user_fold_fallback(self) -> None:
        folded = ContextManifest.build(
            role="worker_explore",
            worker_id="w1",
            intent_id="s1",
            sections=[
                ("shared-graph", "facts: []", {}),
                ("role-contract", "folded contract", {"user_fold_fallback": True}),
            ],
        )
        self.assertTrue(folded.user_fold_fallback)
        self.assertEqual(folded.to_dict()["user_fold_fallback"], True)
        clean = ContextManifest.build(
            role="worker_explore",
            worker_id="w1",
            intent_id="s1",
            sections=[("shared-graph", "facts: []", {})],
        )
        self.assertFalse(clean.user_fold_fallback)


if __name__ == "__main__":
    unittest.main()
