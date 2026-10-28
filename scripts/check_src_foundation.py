"""WP1 behavioral checks for SRC persistence, trusted bindings and projection."""
from __future__ import annotations

import asyncio
import sys
import tempfile
import traceback
from pathlib import Path
from typing import Any, Awaitable, Callable

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.command_handlers.base import CommandFailed
from muteki.platform.contracts.capabilities import CapabilityBinding, ThreadMode
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope, QueryEnvelope
from muteki.platform.contracts.objects import RunRef, Task, Thread
from muteki.platform.contracts.runs import RunSnapshot, RunEvent
from muteki.platform.store import PlatformStore
from muteki.src import SrcService
from muteki.src.commands import SrcCommandHandler, SrcQueryHandler
from muteki.src.control_commands import SrcControlCommandHandler, SrcControlQueryHandler
from muteki.src.store import SrcStoreError


class ConversationFixture:
    def __init__(self, store: PlatformStore) -> None:
        self.store = store

    def activate_thread(self, thread: Thread, principal_id: str, selection: Any = None) -> None:
        self.store.save(thread)


class RunGatewayFixture:
    def __init__(self) -> None:
        self.ensure_calls = []
        self.other_products: set[str] = set()
        self.event_rows: dict[str, list[RunEvent]] = {}

    async def ensure_bound_run(self, request: Any) -> RunRef:
        self.ensure_calls.append(request)
        return RunRef(run_id=request.run_id, task_id=request.task_id, binding_key=request.binding_key)

    async def snapshot(self, run_id: str) -> RunSnapshot:
        kind = "ctf.challenge" if run_id in self.other_products else "pentest.target"
        return RunSnapshot(run_id=run_id, state="draft", task_kind=kind)

    async def events(self, run_id: str, after_seq: int = 0):
        for event in self.event_rows.get(run_id, []):
            if event.seq > after_seq:
                yield event


class World:
    def __init__(self, root: Path) -> None:
        self.store = PlatformStore(root / "platform.db")
        self.gateway = RunGatewayFixture()
        self.service = SrcService(
            self.store, artifact_root=root / "cas",
            conversation=ConversationFixture(self.store), run_gateway=self.gateway,
        )
        self.api = MutekiCommandApiImpl(self.store)
        self.api.register_command(SrcCommandHandler(self.service))
        self.api.register_command(SrcControlCommandHandler(self.service))
        for handler in (SrcQueryHandler(self.service), SrcControlQueryHandler(self.service)):
            self.api.register_query(handler)
        self.operator = ActorRef(kind="operator", id="local-user")

    async def create_task(self, title: str = "foundation") -> tuple[str, str, ActorRef]:
        receipt = await self.api.dispatch(CommandEnvelope(
            command_type="src.task.create", aggregate_type="task", actor=self.operator,
            payload={"title": title, "goal": "verify persisted behavior"},
        ))
        assert receipt.error is None, receipt.error
        task_id = receipt.output["task_id"]
        thread_id = receipt.output["thread_id"]
        binding = CapabilityBinding(thread_id=thread_id, principal_id="local-user", mode=ThreadMode.SRC)
        self.store.save(binding)
        actor = ActorRef(
            kind="agent", id="local-user", binding_id=binding.binding_id,
            binding_version=binding.binding_version, thread_id=thread_id,
        )
        return task_id, thread_id, actor


def expect_code(code: str, call: Callable[[], Any]) -> None:
    try:
        call()
    except (SrcStoreError, CommandFailed) as exc:
        actual = exc.code if isinstance(exc, SrcStoreError) else exc.error.code
        assert actual == code, f"expected {code}, got {actual}"
        return
    raise AssertionError(f"expected {code}, call succeeded")


async def expect_async_code(code: str, call: Callable[[], Awaitable[Any]]) -> None:
    try:
        await call()
    except SrcStoreError as exc:
        assert exc.code == code, f"expected {code}, got {exc.code}"
        return
    raise AssertionError(f"expected {code}, coroutine succeeded")


async def check_foundation(root: Path) -> None:
    world = World(root)
    assert world.store.schema_version() >= 4
    task_id, thread_id, agent = await world.create_task()
    assert world.service.task_for_thread(thread_id).task_id == task_id
    view = await world.api.query(QueryEnvelope(
        query_type="src.task.get", aggregate_type="thread", aggregate_id=thread_id, actor=agent,
    ))
    assert view.result["task_id"] == task_id
    assert view.result["binding"]["thread_id"] == thread_id

    second_task, second_thread, _ = await world.create_task("second")
    try:
        await world.api.query(QueryEnvelope(
            query_type="src.task.get", aggregate_type="thread", aggregate_id=thread_id,
            actor=agent, params={"task_id": second_task},
        ))
    except CommandFailed as exc:
        assert exc.error.code == "task_binding_invalid"
    else:
        raise AssertionError("cross-Task read was allowed")
    agent_tasks = await world.api.query(QueryEnvelope(query_type="src.task.list", actor=agent))
    assert [row["task_id"] for row in agent_tasks.result] == [task_id]

    denied = await world.api.dispatch(CommandEnvelope(
        command_type="src.policy.update", aggregate_type="thread", aggregate_id=thread_id,
        actor=agent, payload={},
    ))
    assert denied.error is not None and denied.error.code == "policy_denied", denied.error

    payload = {"target_url": "https://example.test/api", "question": "same", "binding_key": "stable"}
    first = await world.api.dispatch(CommandEnvelope(
        command_type="src.run.ensure", aggregate_type="thread", aggregate_id=thread_id,
        actor=agent, payload=payload,
    ))
    assert first.error is None, first.error
    run_id = first.output["binding"]["run_id"]
    second = await world.api.dispatch(CommandEnvelope(
        command_type="src.run.ensure", aggregate_type="thread", aggregate_id=thread_id,
        actor=agent, payload=payload,
    ))
    assert second.error is None and second.output["deduplicated"]
    assert second.output["binding"]["run_id"] == run_id
    assert [call.run_id for call in world.gateway.ensure_calls] == [run_id, run_id]
    assert world.service.task_id_for_run(run_id) == task_id
    assert not world.service.run_delete_allowed(run_id)
    expect_code("artifact_missing", lambda: world.service.run_binding(second_task, run_id))
    binding = world.service.binding(task_id)
    await expect_async_code(
        "authorization_stale",
        lambda: world.service.ensure_run(binding, payload, binding_revision=2),
    )
    assert len(world.gateway.ensure_calls) == 2
    world.gateway.other_products.add(run_id)
    await expect_async_code("task_binding_invalid", lambda: world.service.run_snapshot(task_id, run_id))
    world.gateway.other_products.remove(run_id)

    world.store.save(Task(kind="pentest.target", input={"product_mode": "pentest"}))
    ordinary = world.store.list(Task, kind="pentest.target")[0]
    expect_code("task_binding_invalid", lambda: world.service.task(ordinary.task_id))

    raw = b"POST /api?x=1&x=2\r\nX-Test: a\r\nX-Test: b\r\n\r\n\x00"
    artifact = world.service.artifacts.put(task_id, raw, name="request.raw")
    assert world.service.artifacts.read(artifact, expected_task_id=task_id) == raw
    expect_code("task_binding_invalid", lambda: world.service.artifacts.read(artifact, expected_task_id=second_task))

    events = [
        RunEvent(run_id=run_id, seq=1, event_type="pentest.fact", payload={"execution_generation": 1, "fact": "complete fact"}),
        RunEvent(run_id=run_id, seq=2, event_type="pentest.observation", payload={"execution_generation": 1, "output": "complete observation"}),
    ]
    world.gateway.event_rows[run_id] = list(events)
    for event in events:
        await world.service.consume_run_event(event)
        await world.service.consume_run_event(event)
    assert len(world.service.store.all(task_id, "fact")) == 1
    assert len(world.service.store.all(task_id, "observation")) == 1
    assert world.service.store.source_watermark(task_id, run_id)["next_seq"] == 2

    world.store.close()
    world.store = PlatformStore(root / "platform.db")
    world.service = SrcService(
        world.store, artifact_root=root / "cas",
        conversation=ConversationFixture(world.store), run_gateway=world.gateway,
    )
    world.gateway.event_rows[run_id].append(
        RunEvent(run_id=run_id, seq=3, event_type="pentest.report", payload={"execution_generation": 1, "version": 1})
    )
    recovery = await world.service.recover()
    assert recovery["failures"] == [], recovery
    assert recovery["projected"] == 1, recovery
    assert world.service.store.source_watermark(task_id, run_id)["next_seq"] == 3
    assert len(world.service.store.all(task_id, "report_version")) == 1

    stale = RunEvent(run_id=run_id, seq=4, event_type="pentest.fact", payload={"execution_generation": 0, "fact": "stale"})
    expect_code("authorization_stale", lambda: world.service.projector.project(task_id, run_id, stale))
    assert world.service.store.source_watermark(task_id, run_id)["next_seq"] == 3
    world.store.close()


async def main() -> None:
    with tempfile.TemporaryDirectory(prefix="muteki-src-foundation-") as directory:
        await check_foundation(Path(directory))
    print("PASS: WP1 migration, trusted Task/Thread/Run isolation, idempotent Run, CAS, projection and restart recovery")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except Exception:
        traceback.print_exc()
        raise SystemExit(1)
