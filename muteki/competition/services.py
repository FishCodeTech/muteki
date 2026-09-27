"""CompetitionServiceFactory：Web 产品路径的完整比赛服务图与执行循环。"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

from muteki.competition.artifacts import CompetitionArtifactStore
from muteki.competition.binding import RunBindingService
from muteki.competition.compiler import ChallengeCompiler
from muteki.competition.factory import PlatformAdapterFactory
from muteki.competition.instances import InstanceLeaseManager
from muteki.competition.models import (
    Competition, PlatformConnection, RunBinding, SchedulerState,
)
from muteki.competition.outbox_consumer import CompetitionOutboxConsumer
from muteki.competition.projections import CompetitionProjectionManager
from muteki.competition.reconciler import (
    CompetitionReconciler,
    RunFlagInvalidationProjection,
)
from muteki.competition.scheduler import CompetitionScheduler
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.store import CompetitionStore
from muteki.competition.submission import SubmissionService
from muteki.competition.sync import CompetitionSyncService
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.runs import RunCommand


@dataclass
class CompetitionServiceStatus:
    name: str
    state: str = "unavailable"  # ready/degraded/unavailable/stopped
    detail: str = "not started"
    iterations: int = 0
    last_error: str = ""


class CompetitionServiceFactory:
    """集中创建、恢复、启动和停止比赛领域的全部长驻服务。"""

    def __init__(
        self,
        *,
        store: CompetitionStore,
        platform_store: Any,
        run_gateway: Any,
        root: str | Path,
        shared_graph_for: Optional[Callable[[str], Any]] = None,
        workspace_root_for: Optional[Callable[[str], Any]] = None,
        source_witness_resolver: Optional[Callable[..., str]] = None,
        now: Optional[Callable[[], Any]] = None,
        transport_policy: Optional[dict[str, dict[str, Any]]] = None,
        secret_store: Optional[PlatformSecretStore] = None,
        capability_registry: Any = None,
        extension_bridge: Any = None,
    ) -> None:
        self.store = store
        self.platform_store = platform_store
        self.run_gateway = run_gateway
        self._now = now or utcnow
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.secrets = secret_store or PlatformSecretStore(self.root / "secrets")
        self.adapters = PlatformAdapterFactory(
            store,
            self.secrets,
            state_root=self.root / "platform-state",
            transport_policy=transport_policy,
            capability_registry=capability_registry,
            extension_bridge=extension_bridge,
        )
        self.artifacts = CompetitionArtifactStore(
            store, self.root / "artifacts")
        self.sync = CompetitionSyncService(store, self.artifacts)
        self.compiler = ChallengeCompiler(store, self.artifacts)
        self.bindings = RunBindingService(
            store,
            gateway=run_gateway,
            compiler=self.compiler,
            workspace_root_for=workspace_root_for,
        )
        self.leases = InstanceLeaseManager(
            store,
            binding_service=self.bindings,
            adapter_for=self.adapters.for_connection,
            now=now,
        )
        self.submissions = SubmissionService(
            store,
            adapter_for=self.adapters.for_connection,
            binding_service=self.bindings,
            lease_manager=self.leases,
            gateway=run_gateway,
            shared_graph_for=shared_graph_for,
            source_witness_resolver=source_witness_resolver,
            now=now,
        )
        self.projections = CompetitionProjectionManager(store)
        if shared_graph_for is not None:
            self.projections.register(RunFlagInvalidationProjection(
                store, shared_graph_for))
        self.scheduler = CompetitionScheduler(
            store, self.bindings, lease_manager=self.leases)
        self.consumer = CompetitionOutboxConsumer(
            store,
            adapter_factory=self.adapters,
            sync_service=self.sync,
            lease_manager=self.leases,
            binding_service=self.bindings,
            run_gateway=run_gateway,
            effect_store=platform_store,
        )
        self.reconciler = CompetitionReconciler(
            store,
            submission_service=self.submissions,
            lease_manager=self.leases,
            binding_service=self.bindings,
            gateway=run_gateway,
            projection_manager=self.projections,
            now=now,
        )
        self._stop = asyncio.Event()
        self._tasks: dict[str, asyncio.Task[Any]] = {}
        self._first_iterations: dict[str, asyncio.Event] = {}
        self.status: dict[str, CompetitionServiceStatus] = {
            name: CompetitionServiceStatus(name=name)
            for name in (
                "outbox_consumer", "submission_poller", "scheduler_loop",
                "lease_renewal", "projection_loop", "remote_sync", "deadline",
            )
        }
        self.recovery: dict[str, Any] = {}

    async def recover(self) -> dict[str, Any]:
        recovered_processing = self.store.outbox.recover_processing()
        report = await self.reconciler.recover()
        self.consumer.synchronize_submission_effects()
        self.recovery = {
            "ok": report.ok,
            "failed_steps": list(report.failed_steps),
            "steps": dict(report.steps),
            "watermark": report.watermark,
            "processing_recovered": recovered_processing,
        }
        return dict(self.recovery)

    async def start(self) -> None:
        if self._tasks:
            return
        self._stop.clear()
        loops = {
            "outbox_consumer": self._outbox_loop,
            "submission_poller": self._submission_loop,
            "scheduler_loop": self._scheduler_loop,
            "lease_renewal": self._lease_loop,
            "projection_loop": self._projection_loop,
            "remote_sync": self._remote_sync_loop,
            "deadline": self._deadline_loop,
        }
        for name, runner in loops.items():
            status = self.status[name]
            status.state = "unavailable"
            status.detail = "waiting for first iteration"
            status.last_error = ""
            self._first_iterations[name] = asyncio.Event()
            self._tasks[name] = asyncio.create_task(
                runner(), name=f"muteki-competition-{name}")
        try:
            await asyncio.wait_for(asyncio.gather(*(
                event.wait() for event in self._first_iterations.values()
            )), timeout=15.0)
        except asyncio.TimeoutError:
            for name, event in self._first_iterations.items():
                if not event.is_set():
                    self.status[name].state = "unavailable"
                    self.status[name].detail = "first iteration did not finish"
                    self.status[name].last_error = "startup timeout"

    async def stop(self) -> None:
        self._stop.set()
        tasks = list(self._tasks.values())
        self._tasks.clear()
        if tasks:
            done, pending = await asyncio.wait(tasks, timeout=15.0)
            for task in pending:
                task.cancel()
            await asyncio.gather(*done, *pending, return_exceptions=True)
        self._first_iterations.clear()
        for status in self.status.values():
            status.state = "stopped"
            status.detail = "loop stopped"
        await self.adapters.close()

    @property
    def ready(self) -> bool:
        return self.running and all(
            status.state == "ready" for status in self.status.values())

    @property
    def running(self) -> bool:
        """所有长驻循环均已启动且存活；首次迭代可以仍在进行。"""
        return bool(self._tasks) and all(
            not task.done() for task in self._tasks.values()
        )

    def health(self) -> dict[str, Any]:
        return {
            "ready": self.ready,
            "services": {
                name: vars(status).copy()
                for name, status in self.status.items()
            },
            "recovery": dict(self.recovery),
        }

    async def _wait(self, seconds: float) -> None:
        try:
            await asyncio.wait_for(self._stop.wait(), timeout=seconds)
        except asyncio.TimeoutError:
            return

    async def _iteration(self, name: str, operation: Callable[[], Any]) -> None:
        status = self.status[name]
        try:
            result = operation()
            if hasattr(result, "__await__"):
                await result
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            status.state = "degraded"
            status.detail = "iteration failed; loop remains active"
            status.last_error = f"{type(exc).__name__}: {exc}"
        else:
            status.state = "ready"
            status.detail = "loop running"
            status.last_error = ""
        status.iterations += 1
        event = self._first_iterations.get(name)
        if event is not None:
            event.set()

    async def _outbox_loop(self) -> None:
        while not self._stop.is_set():
            await self._iteration(
                "outbox_consumer", lambda: self.consumer.run_once(limit=100))
            await self._wait(0.1)

    async def _submission_loop(self) -> None:
        async def pump() -> None:
            await self.submissions.pump(limit=100)
            # GZCTF 等异步判定平台在首次提交后进入 unknown，并携带远端
            # submission id。长驻 poller 必须持续核对；只在启动恢复时核对
            # 会让运行期间产生的 pending 回执永久停留在 unknown。
            await self.submissions.reconcile_unknown()
            self.consumer.synchronize_submission_effects()

        while not self._stop.is_set():
            await self._iteration("submission_poller", pump)
            await self._wait(0.2)

    async def _scheduler_loop(self) -> None:
        async def tick_all() -> None:
            for competition in self.store.list(Competition):
                if competition.scheduler_state == SchedulerState.RUNNING.value:
                    await self.scheduler.tick(competition.competition_id)

        while not self._stop.is_set():
            await self._iteration("scheduler_loop", tick_all)
            await self._wait(0.5)

    async def _lease_loop(self) -> None:
        while not self._stop.is_set():
            await self._iteration("lease_renewal", self.leases.maintenance)
            await self._wait(1.0)

    async def _stop_expired_runs(self) -> None:
        """到期停止独立于平台同步；失败的 Run 留到下一轮重试。"""
        errors: list[str] = []
        for competition in self.store.list(Competition):
            if competition.ends_at is None or competition.ends_at > self._now():
                continue
            if competition.scheduler_state != SchedulerState.STOPPED.value:
                self.store.save(competition.model_copy(update={
                    "scheduler_state": SchedulerState.STOPPED.value,
                }))
            run_ids = {
                binding.run_id for binding in self.store.list(
                    RunBinding, competition_id=competition.competition_id)
                if binding.run_id
            }
            for run_id in sorted(run_ids):
                try:
                    snapshot = await self.run_gateway.snapshot(run_id)
                    if snapshot.state not in {"running", "paused"}:
                        continue
                    generation = int(
                        (snapshot.detail or {}).get("control_generation") or 0)
                    receipt = await self.run_gateway.command(run_id, RunCommand(
                        command_type="stop",
                        expected_generation=generation,
                    ))
                    RunBindingService._raise_for_failed_receipt(
                        receipt, operation="deadline stop", run_id=run_id)
                except Exception as exc:
                    errors.append(f"{run_id}: {exc}")
        if errors:
            raise RuntimeError("; ".join(errors))

    async def _deadline_loop(self) -> None:
        while not self._stop.is_set():
            await self._iteration("deadline", self._stop_expired_runs)
            await self._wait(0.5)

    async def _projection_loop(self) -> None:
        while not self._stop.is_set():
            await self._iteration("projection_loop", self.projections.run_all)
            await self._wait(0.5)

    async def _remote_sync_loop(self) -> None:
        """Keep the current remote snapshot fresh without replaying history."""
        async def sync_running() -> None:
            for competition in self.store.list(Competition):
                if (
                    competition.archived
                    or competition.tombstoned
                    or competition.scheduler_state != SchedulerState.RUNNING.value
                ):
                    continue
                connection = self.store.get(
                    PlatformConnection, competition.connection_id
                )
                if connection is None or connection.status != "active":
                    continue
                await self.sync.sync(
                    competition.competition_id,
                    self.adapters.for_connection(connection),
                )

        while not self._stop.is_set():
            await self._iteration("remote_sync", sync_running)
            await self._wait(5.0)


__all__ = ["CompetitionServiceFactory", "CompetitionServiceStatus"]
