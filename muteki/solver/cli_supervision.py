"""CLI worker session supervision glue for CliSolver. Moved from cli_solver.py."""
from __future__ import annotations

from typing import Optional

def _open_supervised_session(self) -> None:
    """把本 Worker 注册进 WorkerSessionSupervisor（若提供）。

    完成 Profile→Adapter 解析与内部 AgentSession 建立；
    幂等，重复调用不重复开工。
    """
    if self._session_supervisor is None or self._agent_session_ref is not None:
        return
    self._adapter_resolution = self._session_supervisor.resolve(
        self._worker_profile, self.driver.name,
        driver=self.driver, solver_id=self.solver_id)
    if getattr(self._adapter_resolution, "kind", "") != "cli_compat":
        raise RuntimeError(
            "做题模式只允许无交互 CLI Worker，禁止 ACP/App Server/SDK Runtime"
        )
    self._agent_session_ref = self._session_supervisor.open_session(
        self, self._adapter_resolution)


def _runtime_adapter_event_fields(self) -> dict:
    """spawned 生命周期事件附带的 Adapter 解析字段。"""
    res = self._adapter_resolution
    if res is None:
        return {}
    return {
        "runtime_adapter": res.adapter_key,
        "runtime_transport": res.kind,
    }


def _close_supervised_session(self) -> None:
    """Worker 退出收尾：委托 supervisor 做退出分类（不伪造完成事件）。"""
    if self._session_supervisor is None or self._agent_session_ref is None:
        return
    try:
        self._session_supervisor.close_session(
            self.solver_id,
            stop_reason=self._worker_stop_reason,
            cancelled=self._cancel_event.is_set(),
            steered=self._steer_event.is_set(),
        )
    except Exception:
        pass


async def _note_cli_session(self, session: Optional[str]) -> None:
    """Record the worker's live CLI session id and (if it just became known)
    re-emit worker status so the deck can show the resume id for manual
    intervention. Idempotent — only emits when the id actually changes."""
    if not session or session == self._cli_session:
        return
    self._cli_session = session
    # EXEC-01：external session id 回填监督层的内部 AgentSession 记录。
    if self._session_supervisor is not None:
        try:
            self._session_supervisor.note_external_session(
                self.solver_id, session)
        except Exception:
            pass
    try:
        await self._emit_worker_status(online=True, reason="started")
    except Exception:
        pass


def _note_worker_stop(self, reason: str) -> None:
    if reason:
        self._worker_stop_reason = reason
