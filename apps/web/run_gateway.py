"""RunGateway — 领域模块访问 RunManager 的唯一接口实现（任务书 6.3、CORE-04）。

设计边界：

- ``ensure_bound_run`` 的幂等 / revision 冲突语义与持久索引在
  ``RunManager.ensure_bound_run``（比赛设计 16.3），这里只做契约适配；
- ``command`` 复用 ``muteki/control`` 的持久控制命令路径（command_id 幂等、
  acceptance/effect receipt、执行代校验），不另建第二套暂停/恢复/Operator
  状态机；``start``/``resolve`` 映射到 RunManager 现有执行代生命周期；
- ``snapshot``/``events`` 是 RunManager/SessionStore 现有状态与 JSONL 回放的
  适配视图，不引入新的事件存储；旧 generation 事件由 RunManager 的
  bus generation filter 在进入持久日志前丢弃，本适配器不重述历史。
"""

from __future__ import annotations

import asyncio
import logging
from datetime import datetime, timezone
from typing import Any, AsyncIterator, Optional

from apps.web.control_adapter import ControlPayloadError
from apps.web.dispatch_parse import explicit_category, parse_dispatch
from apps.web.llm_credentials import resolve_llm_profile_credential
from apps.web.run_manager import RunManager
from apps.web.titler import generate_title
from muteki.control import IdempotencyConflict, StateConflict
from muteki.core.events import Event, EventType
from muteki.core.llm import LLMClient, llm_temperature_kwargs
from muteki.core.path_ids import RunIdPathError
from muteki.core.usage import usage_context
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.objects import RunRef
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)
from muteki.platform.contracts.runs import (
    BoundRunRequest,
    RunCommand,
    RunEvent,
    RunSnapshot,
)

LOG = logging.getLogger(__name__)


class RunGatewayMutationError(RuntimeError):
    """兼容 Run 操作的 typed 边界错误。"""

    def __init__(self, error: ErrorEnvelope) -> None:
        super().__init__(error.message)
        self.error = error


def _epoch_to_dt(ts: Any) -> Optional[datetime]:
    try:
        value = float(ts)
    except (TypeError, ValueError):
        return None
    if value <= 0:
        return None
    return datetime.fromtimestamp(value, timezone.utc)


class RunGateway:
    """``muteki.platform.contracts.protocols.RunGateway`` 的 RunManager 实现。"""

    def __init__(self, manager: RunManager) -> None:
        self._mgr = manager

    # ---- ensure_bound_run -------------------------------------------------

    async def ensure_bound_run(self, request: BoundRunRequest) -> RunRef:
        """幂等返回 binding key 对应的 Run（语义见 RunManager.ensure_bound_run）。"""
        run = await self._mgr.ensure_bound_run(request)
        record = self._mgr.bound_runs.get(str(request.binding_key).strip()) or {}
        return RunRef(
            run_id=run.run_id,
            task_id=request.task_id,
            executor_id=request.executor_id or record.get("executor_id"),
            binding_key=str(request.binding_key).strip(),
            created_at=_epoch_to_dt(record.get("created_at"))
            or datetime.now(timezone.utc),
        )

    async def ensure_legacy_run(self, run_id: str = "") -> RunRef:
        """通过 Command Handler 创建或打开旧版 Run 句柄。

        旧 ``POST /api/runs`` 和 ``POST /api/runs/{id}/start`` 需要保留
        ``run-NNNN`` / 调用方指定 id 的兼容外形；实际状态修改集中在本
        Gateway，Web route 只构造 typed command。
        """
        try:
            run = self._mgr.create(run_id) if run_id else self._mgr.create_new()
        except RunIdPathError as exc:
            from muteki.platform.command_handlers.base import CommandFailed, make_error

            raise CommandFailed(make_error(
                "run.id_invalid", str(exc), ErrorCategory.VALIDATION)) from exc
        return RunRef(run_id=run.run_id, created_at=datetime.now(timezone.utc))

    async def mutate_legacy(
        self, operation: str, run_id: str, payload: dict[str, Any]
    ) -> dict[str, Any]:
        """集中承接旧 Run rail、folder、归档和本地操作。"""
        manager = self._mgr
        if operation == "rail.update":
            run = manager.get(run_id)
            if run is None:
                raise self._mutation_error(
                    "run.not_found", f"unknown run {run_id!r}",
                    ErrorCategory.NOT_FOUND)
            ok = True
            if "pinned" in payload:
                ok = manager.set_pinned(
                    run_id, bool(payload["pinned"]),
                    now=float(payload.get("now") or datetime.now().timestamp())) and ok
            if "archived" in payload:
                ok = manager.set_archived(run_id, bool(payload["archived"])) and ok
            if "name" in payload:
                ok = manager.rename(run_id, payload.get("name")) and ok
            if "folder_id" in payload:
                ok = manager.set_folder(run_id, payload.get("folder_id")) and ok
            if "order" in payload:
                ok = manager.set_order(run_id, payload.get("order")) and ok
            current = manager.get(run_id)
            return {"ok": ok, "run": current.summary() if current else None}
        if operation == "rail.delete":
            return {"ok": await manager.delete(run_id)}
        if operation == "folder.create":
            return {"ok": True, "folder": manager.create_folder(
                str(payload.get("name") or ""))}
        if operation == "folder.update":
            return {"ok": manager.update_folder(
                run_id, name=payload.get("name"), order=payload.get("order"))}
        if operation == "folder.delete":
            return {"ok": manager.delete_folder(run_id)}
        if operation == "open":
            return {"ok": manager.open_workspace(run_id)}
        if operation == "artifact.attach":
            return {"ok": True, "files": list(payload.get("files") or [])}
        raise self._mutation_error(
            "run.operation.unsupported", f"unsupported Run operation {operation!r}",
            ErrorCategory.VALIDATION)

    @staticmethod
    def _mutation_error(
        code: str, message: str, category: ErrorCategory, *, recovery_hint: str = ""
    ) -> RunGatewayMutationError:
        return RunGatewayMutationError(ErrorEnvelope(
            code=code, message=message, category=category,
            recovery_hint=recovery_hint))

    # ---- command ----------------------------------------------------------

    async def command(self, run_id: str, command: RunCommand) -> CommandReceipt:
        """下发命令并返回 acceptance receipt。

        ``command_id`` 幂等由控制 journal 保证：同一 command_id 的重试返回
        同一接收结果（``deduplicated=True``），重复内容不一致由 journal 拒绝。
        """
        command_type = str(command.command_type or "").strip().lower()
        if not command_type:
            return self._error_receipt(
                run_id, command,
                code="run.command.invalid",
                message="command_type cannot be empty",
                category=ErrorCategory.VALIDATION)
        if self._mgr.get(run_id) is None:
            return self._error_receipt(
                run_id, command,
                code="run.not_found",
                message=f"unknown run {run_id!r}",
                category=ErrorCategory.NOT_FOUND)
        if command_type == "start":
            return await self._command_start(run_id, command)
        if command_type == "resolve":
            return await self._command_resolve(run_id, command)
        return await self._command_control(run_id, command, command_type)

    async def _command_start(
            self, run_id: str, command: RunCommand) -> CommandReceipt:
        """start 映射到 RunManager.start 的新一代执行代。

        比赛/领域下发的创建请求不发送 ``swarm_class``：空 spec 继续解析为
        标准 ``muteki.swarm.swarm.Swarm``（实验臂只能走显式评测入口）。
        """
        body = dict(command.payload)
        challenge = dict(body.get("challenge") or {})
        binding_key = self._mgr.bound_runs.run_id_owner(run_id)
        binding = (
            self._mgr.bound_runs.get(binding_key)
            if binding_key is not None else None
        ) or {}
        task_kind = str(binding.get("task_kind") or "").strip().lower()
        requested_mode = str(
            challenge.get("mode") or body.get("mode") or ""
        ).strip().lower()
        if not requested_mode and task_kind == "pentest.target":
            body["mode"] = "pentest"
            body.setdefault("race_scout", False)
            challenge["mode"] = "pentest"
            if not str(challenge.get("goal") or body.get("goal") or "").strip():
                challenge["goal"] = str(
                    body.get("prompt") or challenge.get("description") or ""
                ).strip()
            if (not str(challenge.get("scope") or body.get("scope") or "").strip()
                    and str(challenge.get("target") or "").strip()):
                challenge["scope"] = str(challenge["target"]).strip()
        if (task_kind == "pentest.target" or requested_mode == "pentest"
                or str(body.get("mode") or "").strip().lower() == "pentest"):
            from apps.web.task_contract import prepare_dispatch_contract
            try:
                body, _contract = prepare_dispatch_contract(body)
                challenge = dict(body.get("challenge") or {})
            except ValueError as exc:
                return self._error_receipt(
                    run_id, command, code="run.start.pentest_contract_invalid",
                    message=str(exc), category=ErrorCategory.VALIDATION,
                )
        category = str(challenge.get("category") or "").strip().lower()
        if category in {"web", "pwn", "reverse", "crypto", "forensics", "misc"}:
            challenge["category"] = category
        if challenge:
            body["challenge"] = challenge
        if str(body.get("swarm_class") or "").strip():
            return self._error_receipt(
                run_id, command,
                code="run.command.swarm_class_forbidden",
                message="RunGateway start payload must not set swarm_class",
                category=ErrorCategory.VALIDATION,
                recovery_hint="omit swarm_class; the standard Swarm is resolved")
        body.pop("swarm_class", None)
        from apps.web.drivers import build_driver  # 延迟导入，避免模块环
        from muteki.solver.gate import FlagFormatError
        try:
            driver = build_driver(body, mgr=self._mgr)
            await self._mgr.start(run_id, driver)
            self._apply_start_metadata(run_id, body)
        except StateConflict as exc:
            return self._error_receipt(
                run_id, command,
                code="run.start.conflict",
                message=str(exc),
                category=ErrorCategory.CONFLICT,
                recovery_hint="inspect snapshot; do not retry a live run")
        except RunIdPathError as exc:
            return self._error_receipt(
                run_id, command,
                code="run.id_invalid",
                message=str(exc),
                category=ErrorCategory.VALIDATION)
        except FlagFormatError as exc:
            return self._error_receipt(
                run_id, command,
                code="run.start.flag_format_invalid",
                message=str(exc),
                category=ErrorCategory.VALIDATION,
                recovery_hint=(
                    "use token, a flag{...} format example, or a compilable "
                    "Python regular expression"
                ),
            )
        except Exception as exc:  # driver 构造/准入失败，保持边界错误可见
            LOG.exception("gateway start failed for %s", run_id)
            return self._error_receipt(
                run_id, command,
                code="run.start.failed",
                message=f"{type(exc).__name__}: {exc}",
                category=ErrorCategory.INTERNAL)
        return self._accepted_receipt(run_id, command)

    def _apply_start_metadata(self, run_id: str, body: dict[str, Any]) -> None:
        """Apply explicit rail labels, then label missing title/category in the background.

        Swarm uses the Planner HTTP profile for display-only name and category.
        Other kinds keep the Titler. Neither call blocks ``/start`` or writes onto
        the Swarm Challenge used by Decide and Workers.
        """
        run = self._mgr.get(run_id)
        if run is None:
            return
        challenge = dict(body.get("challenge") or {})
        run.mode = (
            "pentest" if str(challenge.get("mode") or body.get("mode") or "") == "pentest"
            else "ctf"
        )
        if challenge.get("name"):
            run.name = str(challenge["name"])
        try:
            roster_category = explicit_category(challenge.get("category"))
        except ValueError:
            LOG.exception("illegal rail category for %s", run.run_id)
            roster_category = ""
        if roster_category:
            run.category = roster_category
        prompt = str(body.get("prompt") or challenge.get("description") or "").strip()
        if not prompt:
            return
        kind = str(body.get("kind") or "swarm")
        need_title = not str(run.name or "").strip()
        try:
            have_category = bool(explicit_category(run.category))
        except ValueError:
            have_category = False
        need_category = kind == "swarm" and not have_category
        if kind != "swarm":
            if not need_title:
                return
            self._start_titler_label(run, prompt)
            return
        if not need_title and not need_category:
            return
        self._start_planner_label(
            run, prompt, need_title=need_title, need_category=need_category)

    def _start_titler_label(self, run: Any, prompt: str) -> None:
        profiles = self._mgr.worker_config.get().get("llm_profiles", {})
        profile = profiles.get("titler") or {}
        try:
            credential = resolve_llm_profile_credential(
                "titler", profile, sessions_root=self._mgr.state_root)
        except ValueError:
            LOG.exception("titler credential missing for %s", run.run_id)
            return
        generation = run.execution_generation
        bus = run.bus
        run_id = run.run_id

        async def _generate_owned_title() -> None:
            current = asyncio.current_task()
            try:
                with usage_context(
                    self._mgr.event_root, run_id=run_id, generation=generation,
                    workspace_kind="single-security-task",
                    actor_kind="auxiliary", role="titler",
                ):
                    title = await generate_title(
                        prompt,
                        bus=None,
                        run_id=None,
                        model=profile.get("model"),
                        base_url=credential.base_url or None,
                        api_key=credential.api_key,
                        temperature_mode=profile.get("temperature_mode"),
                        temperature=profile.get("temperature"),
                    )
                if (
                    run.execution_generation == generation
                    and run.bus is bus
                    and not run.finished
                    and title
                ):
                    await bus.emit(Event(
                        event_type=EventType.RUN_TITLED,
                        run_id=run_id,
                        payload={
                            "title": title,
                            "execution_generation": generation,
                        },
                    ))
            finally:
                if run.title_task is current:
                    run.title_task = None

        run.title_task = asyncio.create_task(_generate_owned_title())

    def _start_planner_label(
        self, run: Any, prompt: str, *, need_title: bool, need_category: bool,
    ) -> None:
        profiles = self._mgr.worker_config.get().get("llm_profiles", {})
        profile = dict(profiles.get("planner") or {})
        try:
            credential = resolve_llm_profile_credential(
                "planner", profile, sessions_root=self._mgr.state_root)
        except ValueError:
            LOG.exception("planner credential missing for %s", run.run_id)
            return
        if not credential.api_key:
            LOG.error("planner API key is missing for %s", run.run_id)
            return
        generation = run.execution_generation
        bus = run.bus
        run_id = run.run_id

        async def _generate_owned_labels() -> None:
            current = asyncio.current_task()
            try:
                kwargs: dict[str, Any] = dict(llm_temperature_kwargs(profile))
                if credential.base_url:
                    kwargs["base_url"] = credential.base_url
                if credential.api_key:
                    kwargs["api_key"] = credential.api_key
                with usage_context(
                    self._mgr.event_root, run_id=run_id, generation=generation,
                    workspace_kind="single-security-task",
                    actor_kind="auxiliary", role="dispatch",
                ):
                    async with LLMClient(**kwargs) as llm:
                        parsed = await parse_dispatch(
                            prompt, llm=llm, model=str(profile.get("model") or "") or None)
                if (
                    run.execution_generation != generation
                    or run.bus is not bus
                    or run.finished
                ):
                    return
                payload: dict[str, Any] = {"execution_generation": generation}
                if need_title and parsed.get("name"):
                    payload["title"] = parsed["name"]
                if need_category and parsed.get("category"):
                    payload["category"] = parsed["category"]
                if "title" not in payload and "category" not in payload:
                    raise ValueError("planner returned no usable rail label")
                await bus.emit(Event(
                    event_type=EventType.RUN_TITLED,
                    run_id=run_id,
                    payload=payload,
                ))
            except Exception:
                LOG.exception("planner rail label failed for %s", run_id)
            finally:
                if run.title_task is current:
                    run.title_task = None

        run.title_task = asyncio.create_task(_generate_owned_labels())

    async def _command_resolve(
            self, run_id: str, command: RunCommand) -> CommandReceipt:
        """resolve 映射到 RunManager.resolve 的续做执行代（standby/reopen 语义
        与既有代际围栏一致，旧 generation 事件不会写入新 generation）。"""
        try:
            ok = await self._mgr.resolve(run_id, dict(command.payload) or None)
        except Exception as exc:
            LOG.exception("gateway resolve failed for %s", run_id)
            return self._error_receipt(
                run_id, command,
                code="run.resolve.failed",
                message=f"{type(exc).__name__}: {exc}",
                category=ErrorCategory.INTERNAL)
        if not ok:
            return self._error_receipt(
                run_id, command,
                code="run.resolve.rejected",
                message="run is not resolvable in its current state",
                category=ErrorCategory.STATE,
                recovery_hint="inspect snapshot and retry after the run settles")
        return self._accepted_receipt(run_id, command)

    async def _command_control(
            self, run_id: str, command: RunCommand,
            command_type: str) -> CommandReceipt:
        """pause/resume/stop/hint/directive 等走持久控制命令路径。"""
        deduplicated = self._mgr.has_control_command(run_id, command.command_id)
        body: dict[str, Any] = {
            "command_id": command.command_id,
            "action": command_type,
            "payload": {
                key: value for key, value in command.payload.items()
                if key not in {"target", "scope"}
            },
        }
        target = command.payload.get("scope", command.payload.get("target"))
        if target not in (None, ""):
            body["target"] = target
        if command.expected_generation is not None:
            body["expected_generation"] = command.expected_generation
        try:
            # Agent Plugin calls return the command's actual terminal control
            # effect.  The ordinary Web control endpoint retains its fast
            # persisted-acceptance behavior by leaving this option disabled.
            result = await self._mgr.post_control(
                run_id, body, wait_for_effect=True)
        except (StateConflict, IdempotencyConflict) as exc:
            return self._error_receipt(
                run_id, command,
                code="run.command.conflict",
                message=str(exc),
                category=ErrorCategory.CONFLICT,
                deduplicated=deduplicated)
        except ControlPayloadError as exc:
            return self._error_receipt(
                run_id, command,
                code="run.command.invalid",
                message=str(exc),
                category=ErrorCategory.VALIDATION)
        except Exception as exc:
            LOG.exception("gateway control command failed for %s", run_id)
            return self._error_receipt(
                run_id, command,
                code="run.command.failed",
                message=f"{type(exc).__name__}: {exc}",
                category=ErrorCategory.INTERNAL)
        if not bool(result.get("ok")):
            code = str(result.get("code") or "") or "run.command.rejected"
            category = (ErrorCategory.CONFLICT
                        if "conflict" in code.lower() else ErrorCategory.STATE)
            return self._error_receipt(
                run_id, command,
                code=code,
                message=str(result.get("detail") or result.get("status") or ""),
                category=category,
                deduplicated=deduplicated)
        # ``post_control`` waits for the durable actor result.  Return that same
        # terminal effect projection for every control action so Agent Plugin
        # clients do not need action-specific guesses or a second storage path.
        output: dict[str, Any] = {}
        control_receipt = self._mgr.control_receipt(
            run_id, command.command_id)
        if control_receipt is not None:
            output["control_receipt"] = control_receipt
            if command_type in {"spawn_worker", "cancel_worker"}:
                target_ids = list(control_receipt.get("target_ids") or [])
                effect_observed = (
                    str(control_receipt.get("status") or "")
                    == "effect_observed"
                )
                if target_ids and effect_observed:
                    output["worker_id"] = str(target_ids[0])
        return self._accepted_receipt(
            run_id,
            command,
            deduplicated=deduplicated,
            output=output,
        )

    # ---- snapshot / events ------------------------------------------------

    async def list_runs(
        self, *, include_archived: bool = False
    ) -> list[dict[str, Any]]:
        """列出 RunManager 已登记的 Run 摘要，供统一只读查询入口使用。"""
        return self._mgr.list_runs(include_archived=include_archived)

    async def snapshot(self, run_id: str) -> RunSnapshot:
        """Run 当前状态快照；未开始的 bound Run 也返回 draft 快照。"""
        run = self._mgr.get(run_id)
        if run is None:
            # 重启后未启动的 bound Run 不在 rail 里；只在确实有持久历史或
            # 绑定记录时才打开 handle，避免给任意 id 捏造快照。
            known = self._mgr.bound_runs.run_id_owner(run_id) is not None
            if not known:
                from muteki.core.session_store import SessionStore
                known = run_id in SessionStore(
                    root=self._mgr.event_root).list_runs()
            if not known:
                raise LookupError(f"unknown run {run_id!r}")
            run = self._mgr.create(run_id)
        binding_key = self._mgr.bound_runs.run_id_owner(run_id)
        record = (self._mgr.bound_runs.get(binding_key)
                  if binding_key is not None else None) or {}
        summary = run.summary()
        return RunSnapshot(
            run_id=run_id,
            state=run.status(),
            generation=run.execution_generation,
            task_kind=str(record.get("task_kind") or ""),
            executor_id=record.get("executor_id") or None,
            created_at=_epoch_to_dt(record.get("created_at")),
            updated_at=_epoch_to_dt(run.updated_at),
            detail={
                "summary": summary,
                "control_generation": run.control_generation,
                "binding_key": binding_key,
                "task_id": record.get("task_id"),
                "task_revision": record.get("task_revision"),
                "flag_progress": {
                    "flags": list(run.flags),
                    "expected_flags": run.expected_flags,
                    "solved": run.solved,
                },
            },
        )

    def events(self, run_id: str, after_seq: int = 0) -> AsyncIterator[RunEvent]:
        """复用 SessionStore 的 JSONL 回放/游标续传（与 SSE 同一数据源）。

        seq 经 replay_monotonic 归一化，调用方用 ``after_seq`` 续传；payload
        控制层写入的完整内容在这里原样透出。
        """
        return self._replay_events(run_id, after_seq=after_seq)

    async def _replay_events(
            self, run_id: str, *, after_seq: int) -> AsyncIterator[RunEvent]:
        run = self._mgr.get(run_id)
        if run is None:
            raise LookupError(f"unknown run {run_id!r}")
        async for ev in run.store.replay_monotonic(run_id, after_seq=after_seq):
            yield RunEvent(
                run_id=run_id,
                seq=int(ev.seq or 0),
                event_type=ev.event_type.value,
                occurred_at=(_epoch_to_dt(ev.ts)
                             or datetime.now(timezone.utc)),
                payload=dict(ev.payload or {}),
            )

    # ---- receipt 构造 ------------------------------------------------------

    def _accepted_receipt(
            self, run_id: str, command: RunCommand, *,
            deduplicated: bool = False,
            output: Optional[dict[str, Any]] = None) -> CommandReceipt:
        run = self._mgr.get(run_id)
        cursor = 0
        if run is not None:
            try:
                cursor = run.store.last_stream_seq(run_id)
            except Exception:
                cursor = 0
        return CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=run_id,
            aggregate=AggregateRef(type="run", id=run_id),
            event_cursor=f"seq:{cursor}",
            deduplicated=deduplicated,
            output=dict(output or {}),
        )

    def _error_receipt(
            self, run_id: str, command: RunCommand, *,
            code: str, message: str, category: ErrorCategory,
            recovery_hint: str = "",
            deduplicated: bool = False) -> CommandReceipt:
        state = (ReceiptState.CONFLICT
                 if category is ErrorCategory.CONFLICT else ReceiptState.FAILED)
        return CommandReceipt(
            command_id=command.command_id,
            state=state,
            run_id=run_id,
            aggregate=AggregateRef(type="run", id=run_id),
            deduplicated=deduplicated,
            error=ErrorEnvelope(
                code=code,
                message=message,
                category=category,
                recovery_hint=recovery_hint,
            ),
        )
