"""Run 控制命令归属与 actor 归因检查（SRC Agent 越权修复的确定性验证）。

无模型调用、无宿主写入；全部状态在临时目录。覆盖：

1. SRC Thread 的 Agent 控制自己 Task 绑定的 Run —— 放行（现有产品行为）。
2. SRC Thread 的 Agent 控制其他 Run（如操作员的普通 CTF/Pentest Run）
   —— 持久拒绝 ``run.not_owner``（PERMISSION），命令不进入 RunGateway。
3. operator 控制任意 Run —— 行为不变。
4. run.resolve 走同一归属校验。
5. 非 SRC Thread（普通 conversation Agent）控制 Run —— 行为不变。
6. ControlCommand.actor 归因：Agent 命令记 ``agent:<principal>`` 且不进入
   payload；operator/Web 缺省仍为 ``operator``。
"""
from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path
import sys
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.control.secrets import SecretStore
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.capability_catalog import ThreadMode
from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.command_handlers.run_handlers import RunCommandHandler
from muteki.platform.contracts.capabilities import CapabilityBinding
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope
from muteki.platform.contracts.objects import AgentSession, Thread
from muteki.platform.contracts.receipts import (
    AggregateRef, CommandReceipt, ReceiptState,
)
from muteki.platform.contracts.runs import RunSnapshot
from muteki.platform.store import PlatformStore
from muteki.src import SrcService

from scripts.check_src_execution import ConversationFixture


class StubRunGateway:
    """只记录 RunCommand 的 RunGateway 桩（不触碰 RunManager/容器）。"""

    def __init__(self) -> None:
        self.commands: list[dict] = []

    async def snapshot(self, run_id: str) -> RunSnapshot:
        return RunSnapshot(run_id=run_id, state="draft", generation=0)

    async def command(self, run_id: str, command) -> CommandReceipt:
        self.commands.append({
            "run_id": run_id,
            "command_type": command.command_type,
            "actor_id": command.actor_id,
        })
        return CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=run_id,
            aggregate=AggregateRef(type="run", id=run_id),
        )

    async def list_runs(self, *, include_archived: bool = False):
        return []

    async def ensure_bound_run(self, request):
        return SimpleNamespace(run_id=str(request.run_id or "crun_stub"))

    async def ensure_legacy_run(self, run_id: str = ""):
        return SimpleNamespace(run_id=run_id or "run-legacy")


def _agent_actor(binding: CapabilityBinding) -> ActorRef:
    return ActorRef(
        kind="agent", id=binding.principal_id,
        binding_id=binding.binding_id, binding_version=binding.binding_version,
        thread_id=binding.thread_id,
    )


async def dispatch(api: MutekiCommandApiImpl, actor: ActorRef,
                   command_type: str, run_id: str, payload: dict,
                   tag: str) -> CommandReceipt:
    envelope = CommandEnvelope(
        command_type=command_type,
        aggregate_type="run",
        aggregate_id=run_id,
        actor=actor,
        payload=payload,
        command_id=f"cmd-{tag}",
    )
    return await api.dispatch(envelope)


async def main() -> None:
    failures: list[str] = []

    def check(name: str, condition: bool, detail: str = "") -> None:
        status = "PASS" if condition else "FAIL"
        print(f"[{status}] {name}" + (f" — {detail}" if detail else ""))
        if not condition:
            failures.append(name)

    tmp = Path(tempfile.mkdtemp(prefix="check-run-ownership-"))
    store = PlatformStore(tmp / "platform.db")
    gateway = StubRunGateway()
    api = MutekiCommandApiImpl(store, run_gateway=gateway)
    api.register_command(RunCommandHandler())

    service = SrcService(
        store, artifact_root=tmp / "cas",
        conversation=ConversationFixture(store), run_gateway=gateway,
    )
    actor = type("Actor", (), {"id": "local-user"})()
    thread, task, task_binding = service.prepare_task(actor, {"title": "t"})
    service.commit_task(thread, task, task_binding, principal_id="local-user")
    service.execution.ensure_default_policy(
        task.task_id, ["https://program.example/"], "local-user")

    bindings = CapabilityBindingService(store)
    src_binding = bindings.issue_binding(
        thread.thread_id, "src-agent-1", ThreadMode.SRC)
    session = AgentSession(adapter_id="check", thread_id=thread.thread_id)
    store.save(session)
    bindings.issue_grant(src_binding, session.agent_session_id, audience="check")

    own = await service.ensure_run(
        task_binding,
        {"target_url": "https://program.example/", "binding_key": "own-1"},
        binding_revision=1,
    )
    own_run_id = str(own["run"]["run_id"])
    foreign_run_id = "run-0001"  # 操作员的普通 CTF/Pentest Run，无 SRC 绑定

    # 1. SRC Agent 控制自己的 Run：放行。
    before = len(gateway.commands)
    receipt = await dispatch(
        api, _agent_actor(src_binding), "run.stop", own_run_id, {}, "own-stop")
    check(
        "SRC agent stops its own task run",
        receipt.error is None and len(gateway.commands) > before
        and gateway.commands[-1]["command_type"] == "stop",
        f"receipt={receipt.state.value}",
    )

    # 2. SRC Agent 控制操作员 Run：持久拒绝且不进入 RunGateway。
    before = len(gateway.commands)
    receipt = await dispatch(
        api, _agent_actor(src_binding), "run.stop", foreign_run_id,
        {}, "foreign-stop")
    check(
        "SRC agent stopping a foreign run is denied (run.not_owner)",
        receipt.error is not None
        and receipt.error.code == "run.not_owner"
        and receipt.error.category.value == "permission"
        and len(gateway.commands) == before,
        f"code={receipt.error.code if receipt.error else None}",
    )
    prior = await api.get_receipt("cmd-foreign-stop")
    check(
        "denial is durable across replay",
        prior.error is not None and prior.error.code == "run.not_owner",
        f"state={prior.state.value}",
    )

    # 3. operator 控制同一 Run：行为不变。
    before = len(gateway.commands)
    receipt = await dispatch(
        api, ActorRef(kind="operator", id="local-user"), "run.stop",
        foreign_run_id, {}, "operator-stop")
    check(
        "operator stopping the same run still works",
        receipt.error is None and len(gateway.commands) > before,
        f"receipt={receipt.state.value}",
    )

    # 4. run.resolve 同一路径：foreign 拒绝、own 放行。
    before = len(gateway.commands)
    receipt = await dispatch(
        api, _agent_actor(src_binding), "run.resolve", foreign_run_id,
        {}, "foreign-resolve")
    check(
        "SRC agent resolving a foreign run is denied",
        receipt.error is not None and receipt.error.code == "run.not_owner"
        and len(gateway.commands) == before,
        f"code={receipt.error.code if receipt.error else None}",
    )
    before = len(gateway.commands)
    receipt = await dispatch(
        api, _agent_actor(src_binding), "run.resolve", own_run_id,
        {}, "own-resolve")
    check(
        "SRC agent resolving its own run works",
        receipt.error is None and len(gateway.commands) > before,
        f"receipt={receipt.state.value}",
    )

    # 5. 非 SRC Thread 的 conversation Agent：行为不变。
    chat_thread = Thread(title="chat", mode="conversation")
    store.save(chat_thread)
    chat_binding = bindings.issue_binding(
        chat_thread.thread_id, "chat-agent", ThreadMode.CONVERSATION)
    before = len(gateway.commands)
    receipt = await dispatch(
        api, _agent_actor(chat_binding), "run.stop", foreign_run_id,
        {}, "chat-stop")
    check(
        "non-SRC conversation agent behavior is unchanged",
        receipt.error is None and len(gateway.commands) > before,
        f"receipt={receipt.state.value}",
    )

    # 6. ControlCommand actor 归因。
    from apps.web.control_adapter import compile_control_command
    secrets = SecretStore(tmp / "secrets")
    agent_body = {"action": "stop", "actor": "agent:src-agent-1"}
    agent_cmd = compile_control_command(
        foreign_run_id, agent_body, secrets=secrets,
        actor=str(agent_body.get("actor") or "operator"))
    check(
        "agent control command keeps its real actor",
        agent_cmd.actor == "agent:src-agent-1",
        f"actor={agent_cmd.actor}",
    )
    check(
        "actor stays out of the driver payload",
        "actor" not in agent_cmd.payload,
        f"payload_keys={sorted(agent_cmd.payload)}",
    )
    operator_cmd = compile_control_command(
        foreign_run_id, {"action": "stop"}, secrets=secrets)
    check(
        "operator default actor is preserved",
        operator_cmd.actor == "operator",
        f"actor={operator_cmd.actor}",
    )
    # handler → gateway → post_control body 的 actor 透传。
    from apps.web.run_gateway import RunGateway
    posted: list[dict] = []

    class StubManager:
        def get(self, run_id):
            return SimpleNamespace(
                store=SimpleNamespace(last_stream_seq=lambda _rid: 0))

        async def has_control_command(self, run_id, command_id):
            return False

        async def post_control(self, run_id, body, *, wait_for_effect=False):
            posted.append(dict(body))
            return {"ok": True, "command_id": str(body.get("command_id") or "")}

        async def control_receipt(self, run_id, command_id):
            return None

    run_gateway = RunGateway(StubManager())
    from muteki.platform.contracts.runs import RunCommand
    await run_gateway.command(
        foreign_run_id,
        RunCommand(command_type="stop", command_id="cmd-gw-actor",
                   actor_id="agent:src-agent-1",
                   payload={"run_id": foreign_run_id}))
    check(
        "gateway forwards the agent actor into the control body",
        bool(posted) and posted[0].get("actor") == "agent:src-agent-1",
        f"body_actor={posted[0].get('actor') if posted else None}",
    )

    print()
    if failures:
        print(f"FAIL: {len(failures)} check(s) failed: {failures}")
        raise SystemExit(1)
    print("PASS: run control ownership and actor attribution checks")


if __name__ == "__main__":
    asyncio.run(main())
