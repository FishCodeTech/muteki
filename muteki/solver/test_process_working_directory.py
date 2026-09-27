"""Workspace ownership checks using plain Python children, never native agents."""
from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from muteki.external_agents import opencode
from muteki.solver.cli_engines.process import run_cli, run_cli_streaming
from muteki.solver.cli_engines.types import CliResult


class _PlainDriver:
    name = "opencode"

    def env_extra(self):
        return {}

    def parse(self, stdout, stderr):
        return CliResult(text=stdout, raw_stderr=stderr)

    def parse_stream_steps(self, _line):
        return []


_READ_FIXTURE = """
import json, os
from pathlib import Path
selected = Path(os.environ.get('PWD') or os.getcwd())
print(json.dumps({
    'cwd': os.getcwd(), 'pwd': os.environ.get('PWD'),
    'fixture': (selected / 'fixture.txt').read_text(),
    'parent_only': os.environ.get('QA_PARENT_ONLY'),
}))
"""


class HostWorkingDirectoryTests(unittest.TestCase):
    def test_assigned_directory_wins_over_inherited_and_explicit_pwd(self):
        with tempfile.TemporaryDirectory(prefix="muteki-cwd-test-") as directory:
            root = Path(directory).resolve()
            selected, previous = root / "selected", root / "previous"
            selected.mkdir()
            previous.mkdir()
            (selected / "fixture.txt").write_text("selected-workspace")
            (previous / "fixture.txt").write_text("previous-workspace")
            with patch.dict(os.environ, {"PWD": str(previous), "QA_PARENT_ONLY": "parent"}):
                parent_before = dict(os.environ)
                for mode, overlay, inherit, relative in (
                    ("run", False, True, False),
                    ("run", True, True, False),
                    ("run", True, True, True),
                    ("stream", False, True, False),
                    ("stream", True, True, False),
                    ("stream", True, True, True),
                    ("stream", True, False, False),
                ):
                    with self.subTest(mode=mode, overlay=overlay, inherit=inherit, relative=relative):
                        cwd = os.path.relpath(selected) if relative else str(selected)
                        env = {"PWD": str(previous)} if overlay else None
                        argv = [sys.executable, "-c", _READ_FIXTURE]
                        if mode == "run":
                            result = run_cli(_PlainDriver(), argv, cwd=cwd, timeout=8, env=env)
                        else:
                            children = []
                            try:
                                result = run_cli_streaming(
                                    _PlainDriver(), argv, cwd=cwd, timeout=8, env=env,
                                    inherit_env=inherit, on_step=lambda _step: None,
                                    on_proc=children.append,
                                )
                            finally:
                                for proc in children:
                                    for stream in (proc.stdout, proc.stderr):
                                        if stream is not None:
                                            stream.close()
                        self.assertEqual(result.returncode, 0, result.error or result.raw_stderr)
                        observed = json.loads(result.text)
                        self.assertEqual(Path(observed["cwd"]).resolve(), selected)
                        self.assertEqual(Path(observed["pwd"]).resolve(), selected)
                        self.assertEqual(observed["fixture"], "selected-workspace")
                        self.assertEqual(observed["parent_only"], "parent" if inherit else None)
                        if env is not None:
                            self.assertEqual(env, {"PWD": str(previous)})
                        self.assertEqual(dict(os.environ), parent_before)


class OpenCodeServerWorkingDirectoryTests(unittest.IsolatedAsyncioTestCase):
    async def test_managed_server_absolute_relative_and_unspecified_cwd(self):
        class FakeHealthClient:
            def __init__(self, *_args, **_kwargs):
                pass

            async def get(self, *_args, **_kwargs):
                return {}

            async def close(self):
                pass

        with tempfile.TemporaryDirectory(prefix="muteki-opencode-cwd-test-") as directory:
            root = Path(directory).resolve()
            selected, previous = root / "selected", root / "previous"
            selected.mkdir()
            previous.mkdir()
            with patch.dict(os.environ, {"PWD": str(previous)}):
                parent_before = dict(os.environ)
                for mode, cwd in (
                    ("absolute", str(selected)),
                    ("relative", os.path.relpath(selected)),
                    ("unspecified", None),
                ):
                    with self.subTest(mode=mode):
                        expected = Path(cwd or os.getcwd()).resolve()
                        observed_file = root / f"{mode}.json"
                        child = (
                            "import os,json,time; from pathlib import Path; "
                            f"Path({str(observed_file)!r}).write_text(json.dumps("
                            "{'cwd':os.getcwd(),'pwd':os.environ.get('PWD')})); "
                            "time.sleep(60)"
                        )
                        adapter = opencode.OpenCodeServerAdapter(
                            extra_env={"PWD": str(previous)}, startup_timeout=2,
                        )
                        adapter._serve_argv = lambda _port: [sys.executable, "-c", child]
                        env = {"PWD": str(previous)}
                        with patch.object(opencode, "OpenCodeHttpClient", FakeHealthClient):
                            proc, _url = await adapter._start_server(cwd, env=env)
                            try:
                                async with asyncio.timeout(4):
                                    while True:
                                        try:
                                            observed = json.loads(observed_file.read_text())
                                            break
                                        except (FileNotFoundError, json.JSONDecodeError):
                                            await asyncio.sleep(0.01)
                                self.assertEqual(Path(observed["cwd"]).resolve(), expected)
                                self.assertEqual(Path(observed["pwd"]).resolve(), expected)
                            finally:
                                await adapter._stop_server(proc)
                            self.assertIsNotNone(proc.returncode)
                        self.assertEqual(env, {"PWD": str(previous)})
                        self.assertEqual(dict(os.environ), parent_before)


if __name__ == "__main__":
    unittest.main()
