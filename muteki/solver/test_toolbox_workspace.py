"""MNT-09.06/07/08: toolbox probe-or-project + workspace contract wording."""
from __future__ import annotations

import tempfile
import subprocess
import unittest
from pathlib import Path
from unittest.mock import patch

from muteki.solver.cli_board_context import _toolbox_block, _workspace_protocol_block
from muteki.solver.cli_prompts import _CTF_WORKER_SYSTEM
from muteki.solver.workspace import (
    clear_toolbox_probe_cache,
    probe_image_paths,
    container_toolbox_candidates,
    stage_worker_toolbox,
)
from muteki.solver.worker_skills import (
    ROLE_CONTRACT_MARKER,
    role_contract_document,
    stage_role_contract,
)


WORKSPACE_DOC_MARKER = "<!-- muteki-workspace-doc:"


class ToolboxProjectionTests(unittest.TestCase):
    def setUp(self) -> None:
        clear_toolbox_probe_cache()

    def test_container_candidates_cover_full_only_paths(self) -> None:
        paths = container_toolbox_candidates()
        self.assertIn("chisel", paths)
        self.assertIn("wordlists", paths)
        self.assertTrue(str(paths["chisel"]).startswith("/"))

    def test_slim_present_paths_project_nothing_full_only(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wd = Path(tmp) / "worker"
            wd.mkdir()
            manifest = stage_worker_toolbox(
                wd, container=True, image="muteki-worker-slim:test", present_paths=[],
            )
            self.assertEqual(manifest, {})
            toolbox = wd / "toolbox"
            self.assertTrue(toolbox.is_dir())
            self.assertFalse((toolbox / "MANIFEST.json").exists())
            self.assertFalse((toolbox / "wordlists").exists())
            self.assertFalse((toolbox / "bin" / "chisel").exists())

    def test_full_present_paths_link_only_confirmed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wd = Path(tmp) / "worker"
            wd.mkdir()
            present = [
                "/usr/bin/tmux",
                "/usr/bin/chisel",
                "/usr/share/seclists",
                "/home/kali/pocs",
            ]
            manifest = stage_worker_toolbox(
                wd, container=True, present_paths=present,
            )
            self.assertEqual(manifest, {})
            self.assertTrue((wd / "toolbox" / "bin" / "tmux").is_symlink())
            self.assertTrue((wd / "toolbox" / "bin" / "chisel").is_symlink())
            self.assertTrue((wd / "toolbox" / "wordlists").is_symlink())
            self.assertFalse((wd / "toolbox" / "knowledge").exists())
            self.assertFalse((wd / "toolbox" / "bin" / "proxychains4").exists())
            self.assertFalse((wd / "toolbox" / "MANIFEST.json").exists())

    def test_restage_removes_stale_full_only_links(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wd = Path(tmp) / "worker"
            wd.mkdir()
            stage_worker_toolbox(
                wd,
                container=True,
                present_paths=["/usr/bin/chisel", "/usr/share/seclists"],
            )
            self.assertTrue((wd / "toolbox" / "bin" / "chisel").is_symlink())
            stage_worker_toolbox(
                wd, container=True, present_paths=["/usr/bin/tmux"],
            )
            self.assertFalse((wd / "toolbox" / "bin" / "chisel").exists())
            self.assertFalse((wd / "toolbox" / "wordlists").exists())
            self.assertTrue((wd / "toolbox" / "bin" / "tmux").is_symlink())

    def test_local_mode_only_links_existing_host_paths(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tools = root / "ctf-tools"
            (tools / "wordlists").mkdir(parents=True)
            (tools / "bin").mkdir(parents=True)
            wd = root / "worker"
            wd.mkdir()
            with patch.dict("os.environ", {"MUTEKI_MAC_WORKER_ROOT": str(tools)}):
                manifest = stage_worker_toolbox(wd, container=False)
            self.assertEqual(manifest, {})
            self.assertTrue((wd / "toolbox" / "wordlists").is_symlink())
            self.assertFalse((wd / "toolbox" / "pocs").exists())

    def test_probe_retains_matches_when_last_path_is_missing(self) -> None:
        real_run = subprocess.run
        with tempfile.TemporaryDirectory() as tmp:
            exists = Path(tmp) / "a-existing"
            exists.write_text("ok")
            missing = Path(tmp) / "z-missing"
            def run(argv, **kwargs):
                if argv[:3] == ["docker", "image", "inspect"]:
                    return subprocess.CompletedProcess(argv, 0, "sha256:probe", "")
                index = argv.index("-c")
                return real_run(["/bin/sh", *argv[index:]], **kwargs)
            with patch("muteki.solver.workspace.subprocess.run", side_effect=run):
                self.assertEqual(probe_image_paths("test", [exists, missing]), {str(exists)})

    def test_local_bin_and_operator_files_survive_restage(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            tools = root / "tools"
            (tools / "bin").mkdir(parents=True)
            original = tools / "bin" / "tmux"
            original.write_text("operator binary")
            wd = root / "worker"
            with patch.dict("os.environ", {"MUTEKI_MAC_WORKER_ROOT": str(tools)}):
                stage_worker_toolbox(wd)
                stage_worker_toolbox(wd)
            self.assertEqual(original.read_text(), "operator binary")
            stage_worker_toolbox(wd, container=True, present_paths=[])
            self.assertEqual(original.read_text(), "operator binary")
            custom = wd / "toolbox" / "bin"
            custom.mkdir(exist_ok=True)
            (custom / "notes.txt").write_text("keep")
            stage_worker_toolbox(wd, container=True, present_paths=[])
            self.assertEqual((custom / "notes.txt").read_text(), "keep")

    def test_no_image_and_no_inject_projects_nothing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            wd = Path(tmp) / "worker"
            wd.mkdir()
            with patch.dict("os.environ", {"MUTEKI_WORKER_IMAGE": ""}, clear=False):
                # Force empty image by passing image=""
                manifest = stage_worker_toolbox(wd, container=True, image="")
            self.assertEqual(manifest, {})


class WorkspaceContractWordingTests(unittest.TestCase):
    def test_workspace_protocol_private_cwd_and_shared(self) -> None:
        text = _workspace_protocol_block()
        self.assertIn("private cwd", text)
        self.assertIn("./shared/", text)
        self.assertNotIn("available to every Worker in this run. Put reusable", text)

    def test_role_contract_already_private_cwd(self) -> None:
        self.assertIn("不要假设后续 Worker 能读取这里的文件", _CTF_WORKER_SYSTEM)
        self.assertIn("shared/", _CTF_WORKER_SYSTEM)

    def test_role_contract_marker_stable_across_engines(self) -> None:
        doc = role_contract_document()
        self.assertTrue(doc.startswith(ROLE_CONTRACT_MARKER))
        with tempfile.TemporaryDirectory() as tmp:
            for engine in ("codex", "cursor", "grok", "kimi", "opencode"):
                staged = stage_role_contract(tmp, engine=engine)
                self.assertEqual(staged.channel, "file", engine)
                text = Path(staged.path).read_text(encoding="utf-8")
                self.assertIn("shared/", text)
                self.assertIn("不要假设后续 Worker 能读取这里的文件", text)

    def test_toolbox_block_lists_confirmed_entries_only(self) -> None:
        class _H:
            _toolbox_manifest = {
                "entries": [
                    {
                        "logical_name": "tmux",
                        "path": "./toolbox/bin/tmux",
                        "category": "host-command",
                    },
                    {
                        "logical_name": "wordlists",
                        "path": "./toolbox/wordlists",
                        "category": "data-dir",
                    },
                ]
            }

        text = _toolbox_block(_H())
        self.assertIn("tmux", text)
        self.assertIn("wordlists", text)
        self.assertIn("confirmed present", text)

    def test_baked_agents_docs_have_marker_and_private_cwd(self) -> None:
        repo = Path(__file__).resolve().parents[2]
        for rel in (
            "docker/worker/AGENTS.md",
            "docker/worker-slim/AGENTS.md",
        ):
            text = (repo / rel).read_text(encoding="utf-8")
            self.assertTrue(text.startswith(WORKSPACE_DOC_MARKER), rel)
            self.assertIn("私有 cwd", text)
            self.assertIn("shared/", text)
            self.assertNotIn("并与同一运行中的协作", text)


if __name__ == "__main__":
    unittest.main()
