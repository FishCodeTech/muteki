"""CliDriver ExternalAgentAdapter wrapper. Moved from cli_driver.py."""
from __future__ import annotations

import asyncio
import os
import shlex
import sys
import threading
from pathlib import Path
from typing import Any, Optional

from muteki.capability_bindings import agent_plugin as capability_agent_plugin
from muteki.external_agents.base import BaseExternalAgentAdapter
from muteki.external_agents.capabilities import probe_cli_driver
from muteki.external_agents.events import build_event
from muteki.external_agents.sessions import (
    EXIT_CLOSED,
    EXIT_INTERRUPTED,
    classify_exit,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import InjectionKind
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from muteki.solver.cli_engines.argv import _insert_model_arg, apply_reasoning_effort
from muteki.solver.cli_engines.base import CliDriver
from muteki.solver.cli_engines.process import run_cli_streaming
from muteki.solver.cli_engines.registry import driver_for
from muteki.solver.cli_engines.types import (
    CliResult,
    LaunchContext,
    LaunchPurpose,
    StreamStep,
)

# ── ExternalAgentAdapter 兼容接线（RUNTIME-01，任务书 7.1 优先级 4）─────────────
# 把现有 CliDriver 包装为 ExternalAgentAdapter 兼容实现。structured transport
# 不可用时的降级是显式的（probe_report().degradations），不静默关闭审批、
# 恢复、来源追踪或 cwd 隔离；结构化 Adapter（Codex app-server、Claude SDK、
# ACP 等）属于 RUNTIME-02~04，不在此实现。
# 延迟到文件末尾导入，避免 external_agents 基础层与 solver 层形成循环依赖。

def _agent_plugin_instructions(plan: Any, plugin_root: Optional[Path]) -> str:
    """把标准 Agent Plugin 交给只能读取文本提示的 Runtime。

    endpoint 与短期 bearer token 已在进程环境中注入；这里仅提供工具目录和
    无敏感信息的调用说明。Runtime 能直接加载 Agent Plugins 时读取根目录的
    ``plugin.json``；skills-only Runtime 读取同包内的标准 Agent Skill。
    """
    python = shlex.quote(sys.executable)
    script = (
        plugin_root / "skills" / capability_agent_plugin.SKILL_NAME
        / "scripts" / "muteki_client.py"
        if plugin_root is not None else None
    )
    script_arg = (
        shlex.quote(str(script))
        if script is not None
        else "<skill-root>/scripts/muteki_client.py"
    )
    describe_command = f"{python} {script_arg} describe"
    invoke_command = (
        f"{python} {script_arg} call <tool_name> --args '<json-object>'")
    tools: list[str] = []
    for tool in plan.tool_descriptions:
        detail = f" — {tool.description.strip()}" if tool.description else ""
        tools.append(f"- {tool.name}{detail}")
    plugin_note = (
        f"Agent Plugins 1.0.0 包位于 {shlex.quote(str(plugin_root))}。"
        if plugin_root is not None else "")
    return "\n".join([
        "[Muteki 平台能力绑定]",
        "当前会话已获得一组可撤销的平台能力。它们通过 CLI Skill 调用，",
        "不会出现在 Runtime 自带的 ToolSearch、MCP 或普通工具列表中。",
        "需要平台数据或操作时，直接使用 Bash/Shell 运行下面的命令；",
        "不要读取 Muteki 数据库、sessions 或 output 目录代替这些工具。",
        plugin_note,
        f"查看当前授权工具：{describe_command}",
        f"调用工具：{invoke_command}",
        "命令输出为 JSON-RPC 结果；根据 result 回答，error 则如实报告。",
        "当前授权工具：",
        *tools,
        "[Muteki 平台能力绑定结束]",
    ])


class CliDriverAdapter(BaseExternalAgentAdapter):
    """CliDriver 的 ExternalAgentAdapter 兼容实现（一次性 CLI 进程模型）。

    语义映射：

    - ``start`` 只固化启动上下文（cwd、env、注入的 endpoint/token 环境
      变量、预分配 session id），进程在首个 turn 才真正拉起；
    - ``send`` 启动一个可取消的 turn 进程（``run_cli_streaming`` +
      ``cancel_event``），stdout 行经 Driver 解析为 StreamStep 后归一化为
      统一 AgentEvent；后续 turn 在 Runtime 支持 resume 时走
      ``build_resume`` 接同一 external session id；
    - ``interrupt`` 置位 cancel_event， watcher 立即 SIGKILL 整个进程树
      （cancellable process tree 沿用 ``_kill_proc_tree`` 既有实现）；
    - ``steer`` 无带内通道，返回 typed unsupported receipt；
    - Secret 经 env 进入子进程（Secret materialization）；进程在指定
      ``cwd`` 运行（cwd 隔离不降级）。事件 payload 原样进入运行记录。
    """

    def __init__(
        self,
        driver: CliDriver,
        *,
        instance_id: str = "default",
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Any = None,
        default_cwd: Optional[str] = None,
        default_env: Optional[dict] = None,
        timeout_s: int = 600,
    ) -> None:
        super().__init__(
            f"cli.{driver.name}",
            instance_id=instance_id,
            store=store,
            binding_service=binding_service,
            gateway_endpoint=gateway_endpoint,
            descriptor_provider=descriptor_provider,
        )
        self._driver = driver
        self._default_cwd = default_cwd
        self._default_env = dict(default_env or {})
        self._timeout_s = int(timeout_s)
        # agent_session_id -> 运行上下文（cwd/env/cancel_event/turns）
        self._runs: "dict[str, dict[str, Any]]" = {}

    @property
    def driver(self) -> CliDriver:
        return self._driver

    # -- probe（实测，见 capabilities.probe_cli_driver） ---------------------

    async def probe(self, request: ProbeRequest):
        report = await asyncio.to_thread(
            probe_cli_driver,
            self._driver,
            adapter_id=self.id,
            instance_id=self.identity.instance_id,
            include_models=request.include_models,
        )
        self._probe_cache = report
        return report.capabilities

    # -- 启动（任务书 7.6 步骤 4 的 Runtime 侧动作） --------------------------

    async def _launch(self, request: SessionStart, plan, bearer_token) -> dict:
        cwd = str(request.options.get("cwd") or self._default_cwd or os.getcwd())
        env = dict(self._default_env)
        env.update({k: str(v) for k, v in (request.options.get("env") or {}).items()})
        if self._driver.name == "kimi" and request.effort and request.effort != "default":
            env["KIMI_MODEL_THINKING_EFFORT"] = str(request.effort)
        capability_prompt = ""
        capability_plugin_root: Optional[Path] = None
        # 能力注入（兼容路径使用标准 Agent Plugin）：endpoint 与 bearer token
        # 仅经环境变量进入子进程；token 本体不进 argv、不进日志、不进事件。
        if plan is not None and bearer_token:
            cfg = plan.runtime_config
            env[str(cfg.get("endpoint_env", "MUTEKI_CAPABILITY_ENDPOINT"))] = (
                plan.gateway_endpoint)
            env[str(cfg.get("token_env", "MUTEKI_CAPABILITY_TOKEN"))] = bearer_token
            if plan.injection_kind is InjectionKind.AGENT_PLUGIN:
                configured_root = str(cfg.get("plugin_install_root") or "").strip()
                plugin_root = (
                    Path(configured_root).expanduser()
                    if configured_root
                    else Path(cwd) / ".muteki" / "agent-plugins"
                    / capability_agent_plugin.PLUGIN_NAME
                )
                capability_plugin_root = capability_agent_plugin.install_plugin(
                    plugin_root)
                capability_prompt = _agent_plugin_instructions(
                    plan, capability_plugin_root)
        pre_seeded = request.resume_handle or self._driver.new_session()
        self._runs[request.agent_session_id] = {
            "cwd": cwd,
            "env": env,
            "options": dict(request.options),
            "model": str(request.model or "").strip(),
            "effort": str(request.effort or "default").strip(),
            "permission_mode": str(request.permission_mode or "").strip(),
            "sandbox_mode": str(request.sandbox_mode or "").strip(),
            "timeout_s": (
                0
                if str(request.options.get("thread_mode") or "") == "conversation"
                else int(request.options.get("timeout_s") or self._timeout_s)
            ),
            "turns": 0,
            "cancel_event": None,
            "external_session_id": pre_seeded,
            "capability_prompt": capability_prompt,
            "capability_plugin_root": (
                str(capability_plugin_root) if capability_plugin_root else ""),
        }
        return {"external_session_id": pre_seeded, "resume_handle": pre_seeded}

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ):
        """启动一个 turn 进程并流出统一 AgentEvent（AsyncIterator）。"""
        return self._turn_stream(session, input)

    def resume(self, session: AgentSessionRef):
        caps = self._probe_cache.capabilities if self._probe_cache else None
        ctx = self._runs.get(session.agent_session_id)
        external_id = session.external_session_id or (
            (ctx or {}).get("external_session_id"))
        if (caps is not None and not caps.resume) or not external_id:
            return self._unsupported_stream(session, "resume", "resume")
        prompt = str(ctx.get("options", {}).get("resume_prompt")
                     or "Continue from where you left off.")
        return self._turn_stream(
            session, AgentInput(kind="message", text=prompt), force_resume=True)

    def _step_events(
        self, step: StreamStep, seq, *, common: dict, turn_id: str,
        external_id: Optional[str],
    ) -> "tuple[list[AgentEvent], Optional[str]]":
        """StreamStep → 统一事件；session 步骤回填 external session id。"""
        events: list[AgentEvent] = []
        if step.kind == "session" and step.session:
            external_id = step.session
            return events, external_id
        if step.kind == "reasoning":
            if step.thinking:
                events.append(build_event(
                    AgentEventType.REASONING_SUMMARY, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="cli.step.reasoning",
                    payload={
                        "reasoning_summary": step.text,
                        "partial": True,
                    },
                    **common))
            else:
                events.append(build_event(
                    AgentEventType.MESSAGE_DELTA, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="cli.step.message",
                    payload={"text": step.text, "thinking": False},
                    **common))
        elif step.kind == "tool":
            events.append(build_event(
                AgentEventType.TOOL_STARTED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.step.tool",
                payload={"tool": step.tool, "call_id": step.call_id,
                         "input": step.text},
                **common))
        elif step.kind == "tool_result":
            events.append(build_event(
                AgentEventType.TOOL_COMPLETED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.step.tool_result",
                payload={"call_id": step.call_id,
                         "output": step.raw or step.text},
                **common))
        elif step.kind == "runtime_warning":
            events.append(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.runtime.warning",
                payload={"message": step.text, "native": step.raw},
                **common))
        return events, external_id

    async def _turn_stream(
        self, session: AgentSessionRef, input: AgentInput, *,
        force_resume: bool = False,
    ):
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        ctx = self._runs.get(sid)
        if ctx is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                external_session_id=session.external_session_id,
                payload={"code": "external_agent.session.unknown",
                         "detail": "session was not started by this adapter"},
            ))
            return
        record = self._tracker.get(sid)
        external_id = (session.external_session_id
                       or ctx.get("external_session_id")
                       or (record.external_session_id if record else None))
        common = dict(
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )

        if ctx["turns"] == 0 and not force_resume:
            yield self.emit(build_event(
                AgentEventType.SESSION_STARTED, seq,
                external_session_id=external_id,
                native_type="cli.launch",
                payload={"transport": "cli",
                         "adapter_id": self.id,
                         "instance_id": self.identity.instance_id,
                         "cwd": ctx["cwd"]},
                **common))
        elif force_resume:
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED, seq,
                external_session_id=external_id,
                native_type="cli.resume",
                payload={"transport": "cli"},
                **common))
        turn_id = new_id("turn")
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="cli.turn.start",
            payload={"kind": input.kind},
            **common))

        web_access = bool(ctx["options"].get("web_access", True))
        kb_access = bool(ctx["options"].get("kb_access", True))
        use_resume = bool(external_id) and (force_resume or ctx["turns"] > 0)
        prompt_text = input.text
        capability_sections = [
            str(ctx.get("capability_prompt") or "").strip(),
            str(input.payload.get("capability_context") or "").strip(),
        ]
        capability_context = "\n\n".join(
            section for section in capability_sections if section
        )
        if capability_context:
            prompt_text = f"{capability_context}\n\n[当前用户请求]\n{input.text}"
        thread_mode = str(ctx["options"].get("thread_mode") or "").strip()
        purpose = {
            "conversation": LaunchPurpose.CONVERSATION,
            "management": LaunchPurpose.MANAGEMENT,
        }.get(thread_mode, LaunchPurpose.WORKER)
        launch = LaunchContext(
            purpose=purpose,
            permission_mode=str(ctx.get("permission_mode") or "").strip(),
            sandbox_mode=str(ctx.get("sandbox_mode") or "").strip(),
        )
        stdin_text: Optional[str] = None
        try:
            if ctx["options"].get("prompt_via_stdin"):
                # Secret 级 prompt 只走 stdin 管道，不进 argv（进程表可读）。
                argv = self._driver.build_execute_stdin(
                    prompt_text, external_id if use_resume else None,
                    web_access=web_access, kb_access=kb_access, stream=True,
                    launch=launch)
                stdin_text = prompt_text
            elif use_resume:
                argv = self._driver.build_resume(
                    prompt_text, external_id,
                    web_access=web_access, kb_access=kb_access, stream=True,
                    launch=launch)
            else:
                argv = self._driver.build_execute(
                    prompt_text, external_id,
                    web_access=web_access, kb_access=kb_access, stream=True,
                    launch=launch)
            selected_model = str(ctx.get("model") or "").strip()
            if selected_model:
                replaced = False
                for flag in ("--model", "-m"):
                    if flag in argv:
                        index = argv.index(flag) + 1
                        if index < len(argv):
                            argv[index] = selected_model
                            replaced = True
                        break
                if not replaced:
                    argv = _insert_model_arg(
                        argv, selected_model, engine=self._driver.name)
            argv = apply_reasoning_effort(
                argv,
                engine=self._driver.name,
                reasoning_effort=str(ctx.get("effort") or "default"),
                native=True,
            )
        except Exception as exc:  # noqa: BLE001
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.argv_error",
                payload={"error": str(exc)[:300]},
                **common))
            return

        loop = asyncio.get_running_loop()
        queue: "asyncio.Queue" = asyncio.Queue()
        cancel_event = threading.Event()
        ctx["cancel_event"] = cancel_event

        def on_step(step: StreamStep) -> None:
            loop.call_soon_threadsafe(queue.put_nowait, ("step", step))

        def work():
            try:
                return run_cli_streaming(
                    self._driver, argv, cwd=ctx["cwd"],
                    timeout=ctx["timeout_s"], on_step=on_step,
                    env=ctx["env"], cancel_event=cancel_event,
                    stdin_text=stdin_text)
            except Exception as exc:  # noqa: BLE001
                return exc

        async def runner() -> None:
            result = await asyncio.to_thread(work)
            await queue.put(("done", result))

        runner_task = asyncio.ensure_future(runner())
        result: Any = None
        while True:
            kind, item = await queue.get()
            if kind == "done":
                result = item
                break
            events, external_id = self._step_events(
                item, seq, common=common, turn_id=turn_id,
                external_id=external_id)
            if external_id and ctx.get("external_session_id") != external_id:
                ctx["external_session_id"] = external_id
                self._tracker.activate(
                    sid, external_session_id=external_id,
                    resume_handle=external_id)
            for event in events:
                yield self.emit(event)
        await runner_task
        ctx["turns"] += 1
        ctx["cancel_event"] = None

        if isinstance(result, Exception):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.spawn_error",
                payload={"error": str(result)[:300]},
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.exit",
                payload={"classification": classify_exit(error=str(result))},
                **common))
            return

        result: CliResult = result
        if result.session and ctx.get("external_session_id") != result.session:
            external_id = result.session
            ctx["external_session_id"] = result.session
            self._tracker.activate(
                sid, external_session_id=result.session,
                resume_handle=result.session)

        classification = classify_exit(
            returncode=result.returncode,
            cancelled=result.cancelled, steered=result.steered,
            timed_out=result.timed_out,
            resume_handle=external_id or None,
            error=result.error,
        )
        from muteki.core.usage import cli_usage
        yield self.emit(build_event(
            AgentEventType.USAGE_UPDATED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="cli.usage",
            payload={"usage": {**cli_usage(result), "num_turns": result.num_turns,
                                "status": classification}, "usage_id": result.usage_id},
            **common))
        if result.cancelled or result.steered or result.timed_out:
            # 被中断/切断/超时：只分类收尾，不伪造 turn 完成。
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.turn.interrupted",
                payload={"reason": classification,
                         "stderr_tail": (result.raw_stderr or "")[-500:]},
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.exit",
                payload={"classification": classification},
                **common))
            return

        if result.returncode not in (None, 0) or result.error \
                or not (result.text or "").strip():
            error = result.error or (
                f"{self._driver.name} exited with code {result.returncode}"
                if result.returncode not in (None, 0)
                else f"{self._driver.name} turn ended without assistant text"
            )
            error_code = (
                "provider_request_timeout"
                if "timed out" in error.casefold()
                else "cli.turn_failed"
            )
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.turn.failed",
                payload={
                    "reason": "failed",
                    "error": {
                        "code": error_code,
                        "message": error[:1800],
                    },
                    "returncode": result.returncode,
                    "stderr_tail": (result.raw_stderr or "")[-1800:],
                },
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="cli.exit",
                payload={"classification": classification},
                **common))
            return

        # turn.completed / message.completed 只来自真实进程输出的解析结果。
        yield self.emit(build_event(
            AgentEventType.MESSAGE_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="cli.result",
            payload={"text": result.text, "role": "assistant"},
            **common))
        yield self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="cli.turn.completed",
            payload={"elapsed_s": result.elapsed_s,
                     "num_turns": result.num_turns},
            **common))

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        ctx = self._runs.get(session.agent_session_id)
        cancel = ctx.get("cancel_event") if ctx else None
        if cancel is None:
            return self.unsupported_receipt(
                "interrupt", "no_active_turn", session=session,
                detail={"detail": "no in-flight turn process for this session"})
        cancel.set()  # watcher 立即 SIGKILL 整个进程树
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        # CLI 一次性进程无带内 steer 通道（turn 级 steer 需结构化传输）。
        return self.unsupported_receipt("steer", "steer", session=session)

    async def _teardown(self, session: AgentSessionRef) -> str:
        ctx = self._runs.pop(session.agent_session_id, None)
        if not ctx:
            return EXIT_CLOSED
        cancel = ctx.get("cancel_event")
        if cancel is not None:
            cancel.set()
            return EXIT_INTERRUPTED
        return EXIT_CLOSED


def cli_adapter_for(
    profile_or_name: "str | dict[str, Any]", **kwargs: Any
) -> CliDriverAdapter:
    """把现有 CLI Driver（按名或 Profile）包装为 ExternalAgentAdapter。"""
    return CliDriverAdapter(driver_for(profile_or_name), **kwargs)
