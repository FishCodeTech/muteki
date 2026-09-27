"""omp CLI driver. Moved from cli_driver.py."""
from __future__ import annotations

import json
import subprocess
from pathlib import Path
from typing import Optional


from muteki.solver.cli_engines.types import (
    CliResult, LaunchContext, StreamStep, WORKER_LAUNCH,
)
from muteki.solver.cli_engines.engines.cursor import CursorDriver
from muteki.solver.cli_engines.engines.pi import PiLikeDriver

_SOLVER_DIR = Path(__file__).resolve().parents[2]

class OhMyPiDriver(PiLikeDriver):
    """Oh My Pi driver.

    在线和离线都使用 OMP 自带的 ``-p``。离线运行通过只读配置覆盖关闭搜索、
    URL 获取和浏览器工具，同时保留本地工具与 Skills。
    """
    name = "omp"
    _MODEL_ENV = "MUTEKI_OMP_MODEL"
    _PROVIDER_ENV = "MUTEKI_OMP_PROVIDER"
    _RESUME_FLAG = "--resume"
    _ENV_EXTRA = {"OMP_SKIP_SETUP": "1"}
    _OFFLINE_CONFIG = _SOLVER_DIR / "omp_offline_config.yml"

    def native_permission_modes(self) -> tuple[str, ...]:
        return ("always-ask", "write", "yolo")

    def _permission_flags(self, launch: LaunchContext) -> list[str]:
        mode = self.validate_launch_context(launch)
        if not launch.interactive:
            return ["--approval-mode=yolo"]
        return [f"--approval-mode={mode}"] if mode else []

    def _offline_argv(
        self, prompt: str, *, resume: str = "",
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        if not self._OFFLINE_CONFIG.is_file():
            raise FileNotFoundError(
                f"OMP offline runtime file missing: {self._OFFLINE_CONFIG}")
        argv = [
            self.bin, "-p", "--mode", "json",
            "--config", str(self._OFFLINE_CONFIG),
            *self._permission_flags(launch),
            *self._provider_model_flags(),
        ]
        if resume:
            argv += ["--resume", resume]
        argv += [prompt]
        return argv

    def build_execute(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del session, kb_access, stream
        if not web_access:
            return self._offline_argv(prompt, launch=launch)
        return super().build_execute(
            prompt, None, web_access=True, launch=launch)

    def build_resume(
        self, prompt: str, session: str, *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        del kb_access, stream
        if not web_access:
            return self._offline_argv(prompt, resume=session, launch=launch)
        return super().build_resume(
            prompt, session, web_access=True, launch=launch)

    def build_execute_stdin(
        self, prompt: str, session: Optional[str], *,
        web_access: bool = True, kb_access: bool = True, stream: bool = False,
        launch: LaunchContext = WORKER_LAUNCH,
    ) -> list[str]:
        if not web_access:
            del prompt, session, kb_access, stream
            if not self._OFFLINE_CONFIG.is_file():
                raise FileNotFoundError(
                    f"OMP offline runtime file missing: {self._OFFLINE_CONFIG}")
            return [
                self.bin, "-p", "--mode", "json", "--no-session",
                "--config", str(self._OFFLINE_CONFIG),
                *self._permission_flags(launch),
                *self._provider_model_flags(),
            ]
        return super().build_execute_stdin(
            prompt, session, web_access=True, kb_access=kb_access, stream=stream,
            launch=launch)

    def parse(self, stdout: str, stderr: str) -> CliResult:
        if '"type":"system"' in stdout or '"type":"result"' in stdout:
            return CursorDriver.parse(self, stdout, stderr)
        return super().parse(stdout, stderr)

    _tool_summary = staticmethod(CursorDriver._tool_summary)

    def parse_stream_steps(self, line: str) -> list[StreamStep]:
        try:
            event_type = json.loads(line.strip()).get("type")
        except (json.JSONDecodeError, AttributeError, TypeError):
            event_type = None
        if event_type in {"system", "assistant", "tool_call", "result"}:
            return CursorDriver.parse_stream_steps(self, line)
        return super().parse_stream_steps(line)

    def parse_stream_line(self, line: str) -> Optional[StreamStep]:
        steps = self.parse_stream_steps(line)
        return steps[0] if steps else None

    def _hello_argv(self) -> list[str]:
        return self._offline_argv(self.HELLO_PROMPT)

    def _hello_ok(self, r: "subprocess.CompletedProcess") -> bool:
        if r.returncode != 0:
            return False
        parsed = self.parse(r.stdout or "", r.stderr or "")
        return bool(parsed.text.strip()) and not parsed.error
