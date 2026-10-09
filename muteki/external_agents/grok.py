"""Grok ACP Adapter（RUNTIME-03，任务书 7.2/7.6）。

正式接入：``grok agent stdio``（ACP v1，stdio JSON-RPC）。核验与本机实测
（docs/research/third_party_verification.md §ACP；2026-08-21，grok 1.0.5）：

- ``grok agent stdio`` 真实存在，以 ACP agent 身份在 stdin/stdout 跑
  JSON-RPC；本机实测 ``initialize`` 返回 ``loadSession: true``、
  ``sessionCapabilities: {list, resume, close}``、
  ``mcpCapabilities.http: true``——恢复优先 ``session/resume``（不重放），
  ``session/load`` 为降级；
- 认证 ``authMethods``：``cached_token``（需先 ``grok login``，读
  ``~/.grok/auth.json``）或 ``xai.api_key``（需 ``XAI_API_KEY``）；
  ``authenticate`` 必须带 Grok 扩展 ``_meta: {"headless": true}``；
- 注意：核验文档建议自动化加 ``--no-auto-update``，但本机 grok 1.0.5 的
  ``grok agent stdio`` **不接受该参数**（unexpected argument）；该开关只对
  顶层 ``grok -p`` headless 路径有效。本 Adapter 不传该参数，如未来版本
  支持可经 ``extra_argv`` 追加。

兼容路径（保留）：``grok -p --output-format streaming-messages-json`` 与
``--resume`` 由 ``muteki.solver.cli_driver.GrokDriver`` + ``CliDriverAdapter``
（``cli.grok``）承担；本模块不 import solver 层，两个 Adapter 可同时注册。
"""

from __future__ import annotations

import asyncio
import json
import math
import os
import re
import shutil
import tomllib
import uuid
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.agent_events import (
    ApprovalOption,
    ApprovalRequestedPayload,
    UserInputRequestedPayload,
    dump_payload,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.external_agents import (
    AccessMode,
    AgentCapabilities,
    ProbeRequest,
    SessionStart,
)

from .acp import AcpRequestError, BaseAcpAdapter, check_response
from .approvals import ApprovalDecision
from .descriptors import GROK_MCP_READY_WAIT_SECONDS as _DEFAULT_MCP_READY_WAIT_SECONDS
from .user_input_schema import normalize_question

#: 认证方式优先级：本机登录态优先，其次 API key（需环境变量在场）。
_AUTH_CACHED_TOKEN = "cached_token"
_AUTH_XAI_API_KEY = "xai.api_key"

#: Grok answers a prompt it rejected for a usage limit with this JSON-RPC code.
GROK_RATE_LIMIT_ERROR_CODE = -32003
#: Background-command wake turns carry ``task-completed-*`` prompt ids; they
#: never settle a prompt Muteki sent.
_TASK_COMPLETED_PROMPT_PREFIX = "task-completed-"
_PROMPT_COMPLETE_METHODS = frozenset({
    "x.ai/session/prompt_complete", "_x.ai/session/prompt_complete"})
_SESSION_NOTIFICATION_METHODS = frozenset({
    "x.ai/session_notification", "_x.ai/session_notification",
    "_x.ai/session/update"})
_ASK_USER_QUESTION_METHODS = ("x.ai/ask_user_question", "_x.ai/ask_user_question")
_EXIT_PLAN_MODE_METHODS = ("x.ai/exit_plan_mode", "_x.ai/exit_plan_mode")
_ACP_STOP_REASONS = frozenset({
    "cancelled", "end_turn", "max_tokens", "max_turn_requests", "refusal"})

# Grok ACP 会在异步 HTTP MCP 工具发现完成前返回 session/new。真实会话中，
# 首轮 prompt 可比 muteki-control 连接更早到达，而 HTTP MCP 连接与 tools/list
# 常需数秒。默认值覆盖该实测窗口并留出余量，避免模型只 search_tool 时
# muteki-control 工具尚未出现在 Runtime 目录中；较慢的本地 MCP 可在有界
# 范围内单独调大。
_MAX_MCP_READY_WAIT_SECONDS = 30.0
#: Product name, not a model id ``session/set_model`` accepts.
_GROK_PRODUCT_MODEL = "grok-build"
_UNSET_EFFORTS = frozenset({"", "default"})
_MODEL_SWITCH_CONFIRM_SECONDS = 10.0


def _mcp_ready_wait_seconds(value: Optional[float] = None) -> float:
    """返回 Grok HTTP MCP 启动稳定窗口的有界秒数。

    ``MUTEKI_GROK_MCP_READY_WAIT_SECONDS`` 是进程级覆盖项，可在不修改
    持久化 Runtime Profile 的前提下调节较慢的本地 MCP。非法或非有限值回落
    到已核验默认值；显式设为零可用于诊断时关闭等待。
    """
    raw: Any = (
        value if value is not None
        else os.environ.get("MUTEKI_GROK_MCP_READY_WAIT_SECONDS")
    )
    if raw in (None, ""):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    try:
        seconds = float(raw)
    except (TypeError, ValueError):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    if not math.isfinite(seconds):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    return min(max(seconds, 0.0), _MAX_MCP_READY_WAIT_SECONDS)


def _toml_with_value(
    text: str, section: str, key: str, rendered_value: str
) -> str:
    """Replace one TOML table value without rewriting unrelated settings."""
    lines = text.splitlines(keepends=True)
    section_header = re.compile(r"^\s*\[([^]]+)]\s*(?:#.*)?(?:\r?\n)?$")
    key_start = re.compile(rf"^\s*{re.escape(key)}\s*=")
    start: Optional[int] = None
    end = len(lines)
    for index, line in enumerate(lines):
        heading = section_header.match(line)
        if not heading:
            continue
        if start is not None:
            end = index
            break
        if heading.group(1).strip() == section:
            start = index + 1

    replacement = f"{key} = {rendered_value}\n"
    if start is None:
        suffix = "" if not text or text.endswith(("\n", "\r")) else "\n"
        spacer = "" if not text or not text.strip() else "\n"
        return f"{text}{suffix}{spacer}[{section}]\n{replacement}"

    for index in range(start, end):
        if not key_start.match(lines[index]):
            continue
        assignment_end = index + 1
        bracket_depth = lines[index].count("[") - lines[index].count("]")
        while bracket_depth > 0 and assignment_end < end:
            bracket_depth += (
                lines[assignment_end].count("[")
                - lines[assignment_end].count("]")
            )
            assignment_end += 1
        lines[index:assignment_end] = [replacement]
        return "".join(lines)

    lines.insert(end, replacement)
    return "".join(lines)


def _grok_supervised_config(text: str) -> str:
    """Return a private Grok config that enforces supervised mutations.

    Grok's native ``--permission-mode default`` still consults the persisted
    mode, permission rules and remembered grants. Muteki keeps every existing
    setting and rule, then adds native ``ask`` rules for commands and file
    mutations. The user's real config is never edited.
    """
    existing_ask: list[str] = []
    try:
        parsed = tomllib.loads(text)
        configured = (parsed.get("permission") or {}).get("ask") or []
        if isinstance(configured, list):
            existing_ask = [str(rule) for rule in configured]
    except (tomllib.TOMLDecodeError, AttributeError, TypeError):
        pass
    ask_rules = list(dict.fromkeys([
        *existing_ask,
        "Bash",
        "Edit",
        "Write",
    ]))
    prepared = _toml_with_value(text, "ui", "permission_mode", '"ask"')
    return _toml_with_value(
        prepared,
        "permission",
        "ask",
        json.dumps(ask_rules, ensure_ascii=False),
    )


def default_grok_binary() -> str:
    return os.environ.get("MUTEKI_GROK_BIN", "grok")


def _option_current(setup: dict[str, Any], option_id: str) -> Optional[str]:
    for option in setup.get("configOptions") or []:
        if isinstance(option, dict) and option.get("id") == option_id:
            value = option.get("currentValue")
            if isinstance(value, str) and value.strip():
                return value.strip()
    return None


def _effort_levels(meta: dict[str, Any]) -> list[str]:
    catalog = meta.get("reasoningEfforts")
    levels: list[str] = []
    if not isinstance(catalog, list):
        return levels
    for item in catalog:
        if isinstance(item, str) and item.strip():
            levels.append(item.strip())
        elif isinstance(item, dict):
            value = item.get("id") or item.get("value") or item.get("effort")
            if isinstance(value, str) and value.strip():
                levels.append(value.strip())
    return levels


def _model_catalog(setup: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Advertised models and the reasoning efforts each one accepts."""
    models = setup.get("models") if isinstance(setup.get("models"), dict) else {}
    catalog: dict[str, dict[str, Any]] = {}
    for row in models.get("availableModels") or []:
        if not isinstance(row, dict):
            continue
        model_id = str(row.get("modelId") or "").strip()
        if not model_id:
            continue
        meta = row.get("_meta") if isinstance(row.get("_meta"), dict) else {}
        effort = meta.get("reasoningEffort")
        catalog[model_id] = {
            "effort": effort.strip() if isinstance(effort, str) and effort.strip() else None,
            "levels": _effort_levels(meta),
        }
    return catalog


def _requested_model(value: Optional[str]) -> Optional[str]:
    model = str(value or "").strip()
    if not model or model == _GROK_PRODUCT_MODEL:
        return None
    return model


def _requested_effort(value: Optional[str]) -> Optional[str]:
    effort = str(value or "").strip()
    if effort in _UNSET_EFFORTS:
        return None
    return effort


def _live_model(setup: dict[str, Any]) -> tuple[Optional[str], Optional[str]]:
    """Model and effort the session is running, preferring live config options."""
    catalog = _model_catalog(setup)
    model = _option_current(setup, "model")
    if model is None:
        models = setup.get("models") if isinstance(setup.get("models"), dict) else {}
        raw = models.get("currentModelId")
        model = raw.strip() if isinstance(raw, str) and raw.strip() else None
    effort = _option_current(setup, "reasoning_effort")
    if effort is None and model is not None:
        effort = (catalog.get(model) or {}).get("effort")
    return model, effort


def _acked_model(result: dict[str, Any]) -> Optional[str]:
    """Model id from a ``session/set_model`` ack, or raise on a structured rejection."""
    meta = result.get("_meta") if isinstance(result.get("_meta"), dict) else {}
    model = meta.get("model")
    if isinstance(model, dict):
        ok = model.get("Ok")
        if isinstance(ok, str) and ok.strip():
            return ok.strip()
        if "Err" in model:
            raise AcpRequestError(
                "session/set_model rejected the model",
                data={"model": model.get("Err")},
            )
    if isinstance(model, str) and model.strip():
        return model.strip()
    return None


async def _confirm_session_model(
    transport: Any,
    session_id: str,
    target: str,
    effort: Optional[str],
) -> None:
    """Wait until Grok's config options show the model and effort just requested.

    Grok ignores an unparsable ``reasoningEffort`` and still acks the model.
    Confirmation uses the option values, not the ack text.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + _MODEL_SWITCH_CONFIRM_SECONDS
    observed_model: Optional[str] = None
    observed_effort: Optional[str] = None
    while True:
        setup = transport.session_setup(session_id)
        observed_model = _option_current(setup, "model")
        observed_effort = _option_current(setup, "reasoning_effort")
        effort_ok = effort is None or observed_effort == effort
        if observed_model == target and effort_ok:
            return
        if loop.time() >= deadline:
            break
        await asyncio.sleep(0.05)
    raise AcpRequestError(
        "session/set_model did not apply the requested model",
        data={
            "modelId": target,
            "reasoningEffort": effort,
            "observedModel": observed_model,
            "observedEffort": observed_effort,
        },
    )


class GrokAcpAdapter(BaseAcpAdapter):
    """Grok Build 的 ACP 结构化 Adapter。

    ``model`` / ``reasoning_effort`` 写入 argv，并在会话建立后用
    ``session/set_model``（``modelId`` + ``_meta.reasoningEffort``）再设一次；
    对端拒绝或选项没有变成请求值时抛 ``AcpRequestError``，不另开会话。
    Session 级能力（stream/approval/resume/interrupt/cwd）由
    ``BaseAcpAdapter`` 实现。
    """

    adapter_id = "grok.acp"
    # Supervised and auto-accept-edits both launch with ``--permission-mode
    # default`` and a private ask config (``_grok_supervised_config``). The
    # shared permission callback accepts edit/delete/move only for
    # auto-accept-edits. On grok 1.0.46, ``--permission-mode acceptEdits``
    # still asked for a file edit (kind=edit; options allow-edits-session,
    # allow-once, reject-once), the same prompt ``default`` sent, so
    # auto-accept-edits is not native acceptEdits. ``auto`` stays unsupported.
    # Full access uses ``--always-approve``.
    #: grok 1.0.46 measurement recorded on the probe report. See the comment above.
    _ACCEPT_EDITS_PROBE_NOTE = (
        "grok 1.0.46 的 --permission-mode acceptEdits 仍会对文件编辑发起 "
        "session/request_permission（kind=edit，选项与 default 相同，含 "
        "allow-edits-session）。auto-accept-edits 由共享审批回调允许编辑，"
        "不是原生 acceptEdits。"
    )
    supported_access_modes = (
        AccessMode.SUPERVISED.value,
        AccessMode.AUTO_ACCEPT_EDITS.value,
        AccessMode.FULL_ACCESS.value,
    )
    unsupported_access_mode_reasons = {
        AccessMode.AUTO.value: (
            "Grok's native 'auto' permission mode has no documented policy and "
            "showed no confirmation behavior on grok 1.0.46"
        ),
    }
    rate_limit_error_codes = (GROK_RATE_LIMIT_ERROR_CODE,)

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        model: Optional[str] = None,
        reasoning_effort: Optional[str] = None,
        extra_argv: tuple[str, ...] = (),
        runtime_root: Optional[str | Path] = None,
        mcp_ready_wait_seconds: Optional[float] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_grok_binary()
        self._model = model
        self._reasoning_effort = reasoning_effort
        self._extra_argv = list(extra_argv)
        self._mcp_ready_wait_seconds = _mcp_ready_wait_seconds(
            mcp_ready_wait_seconds)
        self._runtime_root = Path(
            runtime_root
            or os.environ.get("MUTEKI_GROK_RUNTIME_ROOT")
            or (
                Path(os.environ.get("MUTEKI_STATE_ROOT") or "state")
                / "_grok_acp_runtime"
            )
        ).expanduser().resolve()

    def _agent_argv(self) -> list[str]:
        return self._argv()

    def _argv(
        self,
        *,
        model: Optional[str] = None,
        reasoning_effort: Optional[str] = None,
        permission_mode: Optional[str] = None,
    ) -> list[str]:
        argv = [self._binary]
        if permission_mode:
            argv += ["--permission-mode", permission_mode]
        selected_model = model or self._model
        selected_effort = reasoning_effort or self._reasoning_effort
        if selected_model:
            argv += ["--model", selected_model]
        if selected_effort:
            argv += ["--reasoning-effort", selected_effort]
        argv += ["agent", "stdio"]
        argv += self._extra_argv
        return argv

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        mode = request.access_mode or AccessMode.SUPERVISED.value
        if mode == AccessMode.FULL_ACCESS.value:
            argv = [self._binary]
            if request.model:
                argv += ["--model", str(request.model)]
            if request.effort:
                argv += ["--reasoning-effort", str(request.effort)]
            return [*argv, "agent", "--always-approve", "stdio",
                    *self._extra_argv]
        return self._argv(
            model=request.model,
            reasoning_effort=request.effort,
            permission_mode="default",
        )

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        del cwd
        # OAuth2 attribution for the hosted login flow; an explicit operator
        # override wins.
        env.setdefault("GROK_OAUTH2_REFERRER", "muteki")
        mode = request.access_mode or AccessMode.SUPERVISED.value
        if mode not in (AccessMode.SUPERVISED.value, AccessMode.AUTO_ACCEPT_EDITS.value):
            return env

        prepared = dict(env)
        source = Path(
            prepared.get("GROK_HOME")
            or os.environ.get("GROK_HOME")
            or (Path.home() / ".grok")
        ).expanduser().resolve()
        target = self._runtime_root / request.agent_session_id
        target.mkdir(parents=True, exist_ok=True)
        try:
            target.chmod(0o700)
        except OSError:
            pass

        # Keep Grok's normal auth, models, plugins and persisted sessions by
        # referencing the user's files.  config.toml is the sole private copy.
        if source.is_dir() and source != target:
            for item in source.iterdir():
                if item.name == "config.toml":
                    continue
                if item.is_symlink() and not item.exists():
                    continue
                destination = target / item.name
                if env.get("MUTEKI_CHAT_PRIVATE_ROOT") and destination.is_symlink():
                    private = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"]).resolve()
                    if not destination.resolve().is_relative_to(private):
                        destination.unlink()
                if destination.exists() or destination.is_symlink():
                    continue
                try:
                    destination.symlink_to(
                        item, target_is_directory=item.is_dir()
                    )
                except OSError:
                    if item.is_file():
                        shutil.copy2(item, destination)

        source_config = source / "config.toml"
        try:
            config_text = source_config.read_text(encoding="utf-8")
        except OSError:
            config_text = ""
        target_config = target / "config.toml"
        target_config.write_text(
            _grok_supervised_config(config_text),
            encoding="utf-8",
        )
        try:
            target_config.chmod(0o600)
        except OSError:
            pass
        prepared["GROK_HOME"] = str(target)
        return prepared

    def _select_auth_method(
        self, auth_methods: list[dict[str, Any]]
    ) -> Optional[str]:
        available = {str(m.get("id")) for m in auth_methods}
        if _AUTH_CACHED_TOKEN in available:
            return _AUTH_CACHED_TOKEN
        if _AUTH_XAI_API_KEY in available and os.environ.get("XAI_API_KEY"):
            return _AUTH_XAI_API_KEY
        return super()._select_auth_method(auth_methods)

    def _select_session_auth_method(
        self,
        auth_methods: list[dict[str, Any]],
        env: dict[str, str],
    ) -> Optional[str]:
        available = {str(method.get("id")) for method in auth_methods}
        # A Thread-selected account is present only in the session environment.
        # Prefer it over a cached host login so credential_id remains authoritative.
        if env.get("XAI_API_KEY") and _AUTH_XAI_API_KEY in available:
            return _AUTH_XAI_API_KEY
        return self._select_auth_method(auth_methods)

    def _authenticate_meta(self) -> Optional[dict[str, Any]]:
        # Grok 扩展：headless 模式标记（核验 §ACP，官方示例流程）。
        return {"headless": True}

    # -- engine dialect ---------------------------------------------------------

    def _initialize_options(self) -> dict[str, Any]:
        return {"initialize_meta": {"clientType": "extension"}}

    def _session_transport_options(
        self, request: SessionStart, handle: dict[str, Any]
    ) -> dict[str, Any]:
        sid = request.agent_session_id
        handlers: dict[str, Any] = {}
        for method in _ASK_USER_QUESTION_METHODS:
            handlers[method] = lambda params, sid=sid: self._ask_user_question(sid, params)
        for method in _EXIT_PLAN_MODE_METHODS:
            handlers[method] = lambda params, sid=sid: self._exit_plan_mode(sid, params)
        return {
            "request_handlers": handlers,
            "cancel_meta": {"cancelTrigger": "ctrl_c"},
        }

    def _prompt_id(self, handle: dict[str, Any]) -> Optional[str]:
        # Grok echoes the id on its completion notifications, which can arrive
        # long before (or instead of) the ``session/prompt`` response.
        return str(uuid.uuid4())

    async def _ask_user_question(
        self, agent_session_id: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        """``x.ai/ask_user_question`` -> user-input card; answers go back by label."""
        handle = self._handle_for(agent_session_id)
        raw_questions = [
            row for row in params.get("questions") or [] if isinstance(row, dict)]
        cancelled: dict[str, Any] = {"outcome": "cancelled"}
        if handle is None or not raw_questions:
            return cancelled
        questions: list[dict[str, Any]] = []
        by_id: dict[str, dict[str, Any]] = {}
        for index, row in enumerate(raw_questions):
            text = str(row.get("question") or "").strip()
            qid = str(row.get("id") or text or f"q{index}")
            options = [
                {"value": str(opt.get("label") or ""),
                 "label": str(opt.get("label") or ""),
                 "description": str(opt.get("description") or "")}
                for opt in row.get("options") or []
                if isinstance(opt, dict) and str(opt.get("label") or "")
            ]
            normalized = normalize_question({
                "question_id": qid,
                "header": str(row.get("header") or ""),
                "question": text,
                "options": options,
                "multi": bool(row.get("multiSelect")),
                # Text outside the offered labels travels as an annotation.
                "allow_free_text": True,
            }, index=index)
            if normalized is not None:
                questions.append(normalized)
                by_id[qid] = row

        def resolve(
            decision: str, answers: dict[str, Any], text: str
        ) -> "tuple[dict[str, Any], str]":
            if decision != "submit":
                return cancelled, "cancelled" if decision == "cancel" else "declined"
            if text and not answers and by_id:
                answers = {next(iter(by_id)): {"values": [], "text": text}}
            return _ask_user_question_result(by_id, answers), "answered"

        payload = dump_payload(UserInputRequestedPayload(
            request_id=str(params.get("toolCallId") or new_id("uinp")),
            user_input_kind="grok.ask_user_question",
            title="Grok question",
            questions=questions,
            response_actions=["submit", "cancel"],
            tool_call_id=str(params.get("toolCallId") or "") or None,
            native={"mode": params.get("mode")},
        ))
        return await self._await_extension_user_input(
            handle, request_id=payload["request_id"], params=params,
            payload=payload, resolve=resolve,
            native_type="grok.ask_user_question", cancel_result=cancelled)

    async def _exit_plan_mode(
        self, agent_session_id: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        """``x.ai/exit_plan_mode`` -> plan approval; Grok decides whether to leave."""
        handle = self._handle_for(agent_session_id)
        abandoned: dict[str, Any] = {
            "outcome": "abandoned",
            "feedback": "No operator was available to review the plan.",
        }
        if handle is None:
            return abandoned
        raw_plan = params.get("planContent")
        plan = raw_plan if isinstance(raw_plan, str) else (
            "" if raw_plan is None else str(raw_plan))
        approval_id = str(params.get("toolCallId") or new_id("approval"))

        def resolve(decision: ApprovalDecision) -> dict[str, Any]:
            if decision.allowed:
                return {"outcome": "approved"}
            return {
                "outcome": "request_changes",
                "feedback": decision.note or "Keep planning.",
            }

        payload = dump_payload(ApprovalRequestedPayload(
            approval_id=approval_id,
            approval_kind="plan_exit",
            title="Grok proposes a plan",
            tool_name="x.ai/exit_plan_mode",
            tool_call_id=str(params.get("toolCallId") or "") or None,
            scopes=["once"],
            options=[
                ApprovalOption(option_id="approve", label="Implement", kind="allow_once"),
                ApprovalOption(option_id="request_changes", label="Keep planning", kind="reject_once"),
            ],
            input={"planContent": plan},
            plan_markdown=plan,
        ))
        return await self._await_extension_approval(
            handle, approval_id=approval_id, params=params, payload=payload,
            resolve=resolve, native_type="grok.exit_plan_mode",
            cancel_result=abandoned)

    def _probe_extra_caps(self, caps: Any, hello: Any) -> None:
        # 本机实测（grok 1.0.46）：spawn_subagent 委派会在同一连接上以子
        # sessionId 流式公布子会话，并经 _x.ai/session_notification 的
        # subagent_progress / subagent_finished 汇报进展与结果。
        caps.subagents = True

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        caps = await super().probe(request)
        report = self._probe_cache
        version = str(getattr(caps, "runtime_version", "") or "")
        if report is not None and "1.0.46" in version.split():
            if self._ACCEPT_EDITS_PROBE_NOTE not in report.degradations:
                report.degradations.append(self._ACCEPT_EDITS_PROBE_NOTE)
        return caps

    # -- 子智能体（实测：grok 1.0.46，2026-11） --------------------------------

    def _delegation_info(self, update: dict[str, Any]) -> Optional[dict[str, Any]]:
        tool = (update.get("_meta") or {}).get("x.ai/tool") or {}
        if str(tool.get("name") or "") != "spawn_subagent":
            return None
        raw = update.get("rawInput") or {}
        # node 等子会话公布时以子 sessionId 为锚，这里只标记委派。
        return {
            "title": str(raw.get("description") or "").strip() or None,
            "request": str(raw.get("prompt") or "").strip() or None,
        }

    def _delegation_result(
        self, handle: dict[str, Any], update: dict[str, Any],
        desc: dict[str, Any],
    ) -> Optional[dict[str, Any]]:
        raw = update.get("rawOutput") or {}
        if str(raw.get("type") or "") != "SubagentCompleted":
            return None
        subagent_id = str(raw.get("subagent_id") or "")
        if not subagent_id:
            return None
        patch: dict[str, Any] = {
            "agent_id": subagent_id,
            "session_ref": subagent_id,
        }
        # subagent_finished 先到且可能已置 failed，不能被这里的 completed 覆盖。
        existing = handle["agent_nodes"].get(subagent_id) or {}
        if existing.get("status") == "failed":
            patch["status"] = "failed"
        output = str(raw.get("output") or "").strip()
        if output:
            patch["result" if patch.get("status") != "failed" else "error"] = output
        if raw.get("subagent_type"):
            patch["role"] = str(raw["subagent_type"])
        for key, native in (("tool_uses", "tool_calls"),
                            ("duration_ms", "duration_ms")):
            if isinstance(raw.get(native), (int, float)):
                patch[key] = raw[native]
        return patch

    def _map_agent_extension(
        self, handle: dict[str, Any], method: str,
        params: dict[str, Any], is_request: bool,
    ):
        if method in _PROMPT_COMPLETE_METHODS:
            self._settle_prompt(handle, params.get("promptId"), params)
            return {}, []
        if method not in _SESSION_NOTIFICATION_METHODS:
            return None, []
        update = params.get("update") or {}
        kind = str(update.get("sessionUpdate") or "")
        if kind == "turn_completed":
            self._settle_prompt(
                handle, update.get("prompt_id") or update.get("promptId"), {
                    "sessionId": params.get("sessionId"),
                    "stopReason": update.get("stop_reason") or update.get("stopReason"),
                    "agentResult": update.get("agent_result"),
                })
            return {}, []
        if not kind.startswith("subagent"):
            return None, []
        subagent_id = str(
            update.get("subagent_id") or update.get("child_session_id") or "")
        if not subagent_id:
            return {}, []
        child_session = str(update.get("child_session_id") or subagent_id)
        previous_key = handle["subagent_sessions"].get(child_session)
        if previous_key is not None and previous_key != subagent_id:
            # 重试等场景下 subagent_id 与子 sessionId 不同：迁移旧 node。
            stale = handle["agent_nodes"].pop(previous_key, None)
            if stale is not None and subagent_id not in handle["agent_nodes"]:
                handle["agent_nodes"][subagent_id] = {
                    **stale, "agent_id": subagent_id}
        handle["subagent_sessions"][child_session] = subagent_id
        parent_session = str(update.get("parent_session_id") or "")
        external = str(handle.get("external_session_id") or "")
        parent_id = (
            None if not parent_session or parent_session == external
            else handle["subagent_sessions"].get(parent_session)
        )
        events = []
        if kind == "subagent_progress":
            patch: dict[str, Any] = {
                "agent_id": subagent_id,
                "parent_id": parent_id,
                "session_ref": child_session,
                "status": "running",
            }
            for key, native in (("tool_uses", "tool_call_count"),
                                ("total_tokens", "tokens_used"),
                                ("duration_ms", "duration_ms")):
                if isinstance(update.get(native), (int, float)):
                    patch[key] = update[native]
            event = self._patch_agent_node(handle, subagent_id, patch)
            if event is not None:
                events.append(event)
            return {}, events
        if kind == "subagent_finished":
            flushed = self._flush_child_activity(handle, subagent_id)
            if flushed is not None:
                events.append(flushed)
            status = str(update.get("status") or "")
            failed = status not in ("", "completed")
            output = str(update.get("output") or "").strip()
            patch = {
                "agent_id": subagent_id,
                "parent_id": parent_id,
                "session_ref": child_session,
                "status": "failed" if failed else "completed",
            }
            if output:
                patch["error" if failed else "result"] = output
            for key, native in (("tool_uses", "tool_calls"),
                                ("total_tokens", "tokens_used"),
                                ("duration_ms", "duration_ms")):
                if isinstance(update.get(native), (int, float)):
                    patch[key] = update[native]
            event = self._patch_agent_node(handle, subagent_id, patch)
            if event is not None:
                events.append(event)
            return {}, events
        return {}, []

    def _settle_prompt(
        self, handle: dict[str, Any], prompt_id: Any, notice: dict[str, Any]
    ) -> None:
        """Settle the pending ``session/prompt`` from Grok's completion signal."""
        transport = handle.get("transport")
        session_id = str(notice.get("sessionId") or "")
        prompt = str(prompt_id or "")
        if (transport is None or not session_id or not prompt
                or prompt.startswith(_TASK_COMPLETED_PROMPT_PREFIX)):
            return
        stop_reason = str(notice.get("stopReason") or "")
        agent_result = notice.get("agentResult")
        error: Optional[AcpRequestError] = None
        # ``stopReason`` is a typed token. Translate it to the JSON-RPC code;
        # the turn failure is classified from that code, not from error text.
        if stop_reason == "rate_limit":
            error = AcpRequestError(
                "Grok usage limit reached", code=GROK_RATE_LIMIT_ERROR_CODE)
        elif stop_reason == "error":
            detail = agent_result if isinstance(agent_result, str) else ""
            error = AcpRequestError(
                "Grok ended the turn with an error"
                + (f": {detail}" if detail else ""), code=-32603)
        transport.settle_prompt(
            session_id, prompt,
            result={
                "stopReason": stop_reason if stop_reason in _ACP_STOP_REASONS else "end_turn",
                "_meta": {"sessionId": session_id, "promptId": prompt},
            },
            error=error)

    async def _set_session_model(
        self, transport: Any, session_id: str, request: SessionStart
    ) -> None:
        """``session/set_model`` with ``modelId`` and ``_meta.reasoningEffort``.

        A cold start also passes both on argv. Resume can restore another
        model, so this call is what changes the live session. Failure raises
        ``AcpRequestError``; this method does not spawn a replacement session.
        """
        requested_model = _requested_model(request.model)
        requested_effort = _requested_effort(request.effort)
        if requested_model is None and requested_effort is None:
            return
        setup = transport.session_setup(session_id)
        current_model, current_effort = _live_model(setup)
        target = requested_model or current_model
        if target is None:
            raise AcpRequestError(
                "session/set_model needs a model id and the session advertised none",
                data={"reasoningEffort": requested_effort},
            )
        levels = list((_model_catalog(setup).get(target) or {}).get("levels") or [])
        if requested_effort and levels and requested_effort not in levels:
            raise AcpRequestError(
                f"Grok model {target} does not offer reasoningEffort {requested_effort!r}",
                data={
                    "modelId": target,
                    "reasoningEffort": requested_effort,
                    "offered": levels,
                },
            )
        model_changed = requested_model is not None and requested_model != current_model
        effort_changed = requested_effort is not None and requested_effort != current_effort
        if not model_changed and not effort_changed:
            return
        params: dict[str, Any] = {"sessionId": session_id, "modelId": target}
        if requested_effort is not None:
            params["_meta"] = {"reasoningEffort": requested_effort}
        result = check_response("session/set_model", await transport.peer.request(
            "session/set_model", params, timeout=30.0))
        acked = _acked_model(result)
        if acked is not None and acked != target:
            raise AcpRequestError(
                f"session/set_model applied {acked!r} instead of {target!r}",
                data={"modelId": target, "applied": acked},
            )
        await _confirm_session_model(
            transport, session_id, target,
            requested_effort if requested_effort is not None else None,
        )

    async def _after_session_open(
        self, transport: Any, session_id: str, request: SessionStart
    ) -> None:
        """Select the requested model inside the session that just opened."""
        await self._set_session_model(transport, session_id, request)
        handle = self._handle_for(request.agent_session_id)
        supervised = getattr(getattr(transport, "peer", None), "_supervised", None)
        if handle is not None and supervised is not None:
            handle["process_ownership"] = supervised.record.containment
        await self._await_injected_mcp_ready(
            transport, session_id, request,
            (handle or {}).get("mcp_servers") or [])

    async def _await_injected_mcp_ready(
        self,
        transport: Any,
        session_id: str,
        request: SessionStart,
        mcp_servers: list[dict[str, Any]],
    ) -> None:
        """避免 Grok 在 HTTP MCP 工具发现完成前接收首轮 prompt。

        Grok ACP 目前没有暴露逐服务的 readiness RPC；其 session/new 响应
        和 ``mcp_servers[].status`` 都只代表配置已接受。因而这里限定为
        已真实注入 HTTP MCP 时的有界稳定窗口，而非将未确认的配置状态
        误报为已连接。仅 Grok 覆盖此钩子，其他 ACP Runtime 不受影响。
        """
        del transport, session_id, request
        has_http_mcp = any(
            str(server.get("type") or "").strip().lower() == "http"
            and bool(str(server.get("url") or "").strip())
            for server in mcp_servers
            if isinstance(server, dict)
        )
        if has_http_mcp and self._mcp_ready_wait_seconds > 0:
            await asyncio.sleep(self._mcp_ready_wait_seconds)


def _answer_values(answer: Any) -> list[str]:
    if isinstance(answer, dict):
        values = [str(item).strip() for item in answer.get("values") or []]
        text = str(answer.get("text") or "").strip()
        return [item for item in [*values, text] if item]
    if isinstance(answer, (list, tuple)):
        return [str(item).strip() for item in answer if str(item).strip()]
    text = str(answer or "").strip()
    return [text] if text else []


def _ask_user_question_result(
    questions: dict[str, dict[str, Any]], answers: dict[str, Any]
) -> dict[str, Any]:
    """``accepted`` result: chosen labels by question text; free text as notes."""
    accepted: dict[str, list[str]] = {}
    annotations: dict[str, dict[str, str]] = {}
    for qid, row in questions.items():
        question = str(row.get("question") or qid)
        values = _answer_values(answers.get(qid, answers.get(question)))
        if not values:
            continue
        labels = {
            str(opt.get("label") or ""): opt for opt in row.get("options") or []
            if isinstance(opt, dict)}
        chosen = [value for value in values if value in labels]
        notes = [value for value in values if value not in labels]
        accepted[question] = chosen or ["Other"]
        annotation: dict[str, str] = {}
        if notes:
            annotation["notes"] = "\n".join(notes)
        if not row.get("multiSelect"):
            preview = next(
                (str(labels[value].get("preview") or "") for value in chosen
                 if labels[value].get("preview")), "")
            if preview:
                annotation["preview"] = preview
        if annotation:
            annotations[question] = annotation
    result: dict[str, Any] = {"outcome": "accepted", "answers": accepted}
    if annotations:
        result["annotations"] = annotations
    return result


__all__ = ["GrokAcpAdapter", "GROK_RATE_LIMIT_ERROR_CODE", "default_grok_binary"]
