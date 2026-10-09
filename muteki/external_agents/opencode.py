"""OpenCode Server Adapter（RUNTIME-04，任务书 7.2/7.6）。

按 T3 nightly 611132c 先探测版本代：v1 使用本模块，v2 使用独立
``opencode2.OpenCode2Runtime``（原生 /api/info、inbox/execution、form）。
版本/协议无法确认时明确报错，不猜测代际或切换传输。

v1 接入：``opencode serve`` + v1 HTTP API + SSE（``@opencode-ai/sdk``
对应的同一份 OpenAPI 面）。核验结论
（docs/research/third_party_verification.md §OPENCODE，仓库已更名
``anomalyco/opencode``，稳定线 v1.18.x，本机实测 1.18.18）：

- ``opencode serve [--port 4096] [--hostname 127.0.0.1]`` 暴露 OpenAPI 3.1
  于 ``GET /doc``；``OPENCODE_SERVER_PASSWORD`` / ``OPENCODE_SERVER_USERNAME``
  启用 HTTP Basic Auth；
- Session：``POST /session`` 创建；**resume 语义 = 重新采用既有 session
  id**（先 ``GET /session/:sessionID`` 探测存在性，再继续 prompt；
  session 持久化于 opencode 全局存储，同 cwd 的新 server 可跨进程恢复，
  2026-08-21 本机实测；cwd 变化时官方路径是
  ``POST /session/:sessionID/fork``，本 Adapter 不自动 fork）；
- Prompt：``POST /session/:sessionID/prompt_async``（204 只表示接纳，
  立即返回）。回合结束以 SSE ``session.idle`` / ``session.status=idle``
  为准，且要等这次 prompt 的接纳跟踪放开。同步
  ``POST /session/:sessionID/message`` 不再当作回合完成信号。事件流经
  ``GET /event`` SSE（首事件 ``server.connected``，随后 bus 事件：
  ``session.status``、``message.part.updated``、``permission.asked``、
  ``question.asked``、``todo.updated``、``session.compacted`` 等）；
- Interrupt：``POST /session/:sessionID/abort``。没有该端点时返回
  ``opencode.abort_unsupported``，不靠杀掉 server 假装中断；
- Permission：``GET /permission`` + ``POST /permission/:requestID/reply``
  （body ``{reply, message?}``，``reply`` 为 ``PermissionV1.Reply``
  枚举 once/always/reject；旧的
  ``POST /session/:id/permissions/:permissionID`` 已 deprecated，仅在
  新端点 404 时兜底）；
- Question：``GET /question`` + ``POST /question/:requestID/reply``
  （body ``{answers: Answer[][]}``）/ ``POST /question/:requestID/reject``；
- MCP 注入：``POST /mcp``，body ``{name, config}``，remote 形态
  ``{type:"remote", url, headers, oauth:false}``（T3 生产用法；
  注意 T3 侧限制——仅对自己拉起的 server 注入；attach 外部 server 时
  注入是否生效属「仍需真实环境确认」项，失败只记 RUNTIME_WARNING 与
  probe degradations，不静默忽略）；
- SSE ``message.part.updated`` 中 tool part 的 ``state.output`` 是真实
  命令输出的结构化来源（gate witness 输入），``assistant`` message 的
  ``tokens`` 提供 usage。

能力注入按 probe 选择：server 可用即声明 ``mcp``（``POST /mcp`` 动态
注册）→ MCP 计划（``mcpServers`` dict 形态，由本 Adapter 物化为
``POST /mcp`` 调用）；probe 失败时 ``select_injection_kind`` 自然落到
CLI_SKILL 兜底。

兼容路径：``opencode run --format json`` 一次性 CLI 由
``muteki.solver.cli_driver.OpenCodeDriver`` + ``CliDriverAdapter``
（``cli.opencode``）承担；本模块不 import solver 层。
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import time
from pathlib import Path
from typing import Any, AsyncIterator, Callable, Optional

import httpx

from muteki.capability_bindings.acp_config import TOKEN_PLACEHOLDER
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    CapabilityInjectionPlan,
    InjectionKind,
)
from muteki.platform.contracts.external_agents import (
    AccessMode,
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    ApprovalResponseInput,
    MessageInput,
    ProbeRequest,
    SteerInput,
    SessionOptions,
    SessionStart,
    UserInputResponseInput,
)
from muteki.platform.contracts.protocols import RuntimeOperationAdapter
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .base import BaseExternalAgentAdapter, TurnLimits, TurnRunner
from .approvals import (
    ApprovalDecision,
    ApprovalTarget,
    SessionApprovalGrants,
    native_decision,
)
from .capabilities import (
    AccessModeUnsupportedError,
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    conservative_capabilities,
    _probe_version,
)
from .events import build_event
from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    PlanPayload,
    PlanTaskPayload,
    ReasoningPayload,
    RuntimeCapabilitiesPayload,
    RuntimeErrorPayload,
    RuntimeExitedPayload,
    RuntimeWarningPayload,
    SessionPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    UserInputRequestedPayload,
    UserInputResolvedPayload,
    dump_payload,
)
from .probe_environment import subprocess_environment
from .process_supervisor import SupervisedProcess, spawn_supervised
from .rpc import ProcessOutputLog
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import classify_exit
from .user_input_schema import (
    _answer_values,
    expand_legacy_text_answers,
    normalize_question,
)

class OpenCodeError(RuntimeError):
    """OpenCode HTTP 或版本错误。``code`` 是稳定机器码，不靠正文匹配。"""

    def __init__(
        self, message: str, *, code: str = "opencode.http_error",
        status: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.status = status


_SSE_STALL_TIMEOUT_S = 45.0
# A turn with no session event for this long is checked against
# ``GET /session/status`` so a missed idle cannot hang it.
_IDLE_RECONCILE_S = 30.0
_SETTLE_IDLE_WAIT_S = 15.0


class OpenCodeHttpClient:
    """``opencode serve`` v1 HTTP API 的最小异步客户端（httpx）。

    只做传输：端点路径、鉴权头与错误解包；不含任何会话语义。
    """

    def __init__(
        self,
        base_url: str,
        *,
        username: str = "",
        password: str = "",
        timeout: float = 30.0,
    ) -> None:
        auth = None
        if password or username:
            auth = httpx.BasicAuth(username or "opencode", password)
        self._client = httpx.AsyncClient(
            base_url=base_url.rstrip("/"), auth=auth,
            timeout=httpx.Timeout(timeout, read=None),
            trust_env=not base_url.startswith(
                ("http://127.0.0.1", "http://localhost")),
        )

    async def close(self) -> None:
        await self._client.aclose()

    async def request(
        self, method: str, path: str, *,
        body: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        try:
            resp = await self._client.request(
                method, path, json=body, timeout=timeout)
        except httpx.HTTPError as exc:
            raise OpenCodeError(
                f"{method} {path} 连接失败：{type(exc).__name__}: {exc}",
                code="opencode.http_timeout" if isinstance(exc, httpx.TimeoutException)
                else "opencode.http_error",
            ) from exc
        if resp.status_code == 204:
            return None
        if resp.status_code >= 400:
            raise OpenCodeError(
                f"{method} {path} -> {resp.status_code}: {resp.text}",
                status=resp.status_code)
        if not resp.content:
            return None
        try:
            return resp.json()
        except json.JSONDecodeError as exc:
            raise OpenCodeError(
                f"{method} {path} 返回非 JSON：{resp.text}") from exc

    async def get(self, path: str, **kw: Any) -> Any:
        return await self.request("GET", path, **kw)

    async def post(self, path: str, body: Optional[dict[str, Any]] = None,
                   **kw: Any) -> Any:
        return await self.request("POST", path, body=body, **kw)

    async def patch(self, path: str, body: Optional[dict[str, Any]] = None,
                    **kw: Any) -> Any:
        return await self.request("PATCH", path, body=body, **kw)

    async def stream_sse(
        self,
        path: str,
        on_event: Callable[[str, dict[str, Any]], None],
        stop: asyncio.Event,
        connected: Optional[asyncio.Event] = None,
        *,
        stall_timeout: float = _SSE_STALL_TIMEOUT_S,
    ) -> None:
        """订阅 SSE（``GET /event``）。

        ``stop`` 置位后正常返回；其他任何结束（HTTP 拒绝、连接断开、服务端
        关流、超过 ``stall_timeout`` 没有任何帧）都抛带稳定 ``code`` 的
        ``OpenCodeError``，由持有该任务的一方决定回合如何收尾。
        ``connected`` 在收到 ``server.connected`` 帧后置位：服务端此时已把
        订阅挂到 bus 上，之后发出的 prompt 不会丢事件。服务端每约 10 秒发
        ``server.heartbeat``，所以长时间无帧即连接已死。

        非 JSON 事件帧跳过（bus 事件格式由 Runtime 演进，传输层不做语义判定）。
        """
        try:
            async with self._client.stream("GET", path) as resp:
                if resp.status_code >= 400:
                    await resp.aread()
                    raise OpenCodeError(
                        f"GET {path} -> {resp.status_code}: {resp.text}",
                        code="opencode.sse_rejected")
                lines = resp.aiter_lines().__aiter__()
                event_name = ""
                while not stop.is_set():
                    try:
                        line = await asyncio.wait_for(
                            lines.__anext__(), timeout=stall_timeout)
                    except StopAsyncIteration:
                        break
                    except asyncio.TimeoutError as exc:
                        raise OpenCodeError(
                            f"GET {path} 超过 {stall_timeout}s 没有任何帧"
                            "（含 heartbeat）",
                            code="opencode.sse_stalled") from exc
                    if line.startswith("event:"):
                        event_name = line[6:].strip()
                    elif line.startswith("data:"):
                        try:
                            data = json.loads(line[5:].strip())
                        except json.JSONDecodeError:
                            continue
                        if isinstance(data, dict):
                            if connected is not None and str(
                                    data.get("type") or event_name
                            ) == "server.connected":
                                connected.set()
                            on_event(event_name, data)
                        event_name = ""
        except httpx.HTTPError as exc:
            if stop.is_set():
                return
            raise OpenCodeError(
                f"GET {path} 连接中断：{exc}",
                code="opencode.sse_closed") from exc
        if not stop.is_set():
            raise OpenCodeError(
                f"GET {path} 被服务端关闭", code="opencode.sse_closed")


def _pick_free_port() -> int:
    """选取空闲 loopback 端口（0 由内核分配）。"""
    import socket
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


_BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
_last_message_stamp = 0


def _opencode_message_id() -> str:
    """Client-chosen user message id in OpenCode's own ascending format.

    OpenCode sorts a session's messages by id and decides from that order
    whether the last user message already has an assistant reply. An id that
    sorts before earlier assistant ids (a random uuid does) makes a later
    prompt look already answered. ``msg_`` + 12 hex of ``ms * 0x1000 +
    counter`` + 14 random base62 characters, strictly increasing per process.
    """
    global _last_message_stamp
    stamp = max(int(time.time() * 1000) * 0x1000, _last_message_stamp + 1)
    _last_message_stamp = stamp
    suffix = "".join(secrets.choice(_BASE62) for _ in range(14))
    return f"msg_{stamp & 0xFFFFFFFFFFFF:012x}{suffix}"


def _part_text(part: dict[str, Any]) -> str:
    return str(part.get("text") or "")


def _message_tokens(info: dict[str, Any]) -> dict[str, Any]:
    tokens = info.get("tokens")
    return dict(tokens) if isinstance(tokens, dict) else {}


def _opencode_usage_payload(info: dict[str, Any]) -> dict[str, Any]:
    """OpenCode ``info.tokens`` → normalized usage (per-message, cumulative).

    OpenCode reports ``input`` / ``output`` / ``reasoning`` /
    ``cache.read`` / ``cache.write`` as disjoint input buckets. Reasoning is
    separate from output. The normalized contract includes both exactly once.
    The same message re-reports with a stable ``usage_id`` so consumers
    overwrite instead of double counting.
    """
    tokens = _message_tokens(info)
    from muteki.core.usage import sum_token_buckets
    cache = tokens.get("cache") if isinstance(tokens.get("cache"), dict) else {}
    message_id = str(info.get("id") or "") or "unknown"
    return dump_payload(UsagePayload(
        scope="message",
        usage_id=message_id,
        input_tokens=sum_token_buckets(tokens.get("input"), cache.get("read"), cache.get("write")),
        output_tokens=sum_token_buckets(tokens.get("output"), tokens.get("reasoning")),
        cached_input_tokens=cache.get("read"),
        cache_write_tokens=cache.get("write"),
        reasoning_tokens=tokens.get("reasoning"),
        cost_usd=(info.get("cost")
                  if isinstance(info.get("cost"), (int, float))
                  and not isinstance(info.get("cost"), bool) and info["cost"] > 0 else None),
        native={"tokens": tokens},
    ))


def _response_error(value: Any) -> str:
    """Return a concise model/runtime error from an OpenCode response value."""
    if not value:
        return ""
    if isinstance(value, str):
        return value.strip()
    if not isinstance(value, dict):
        return str(value).strip()
    data = value.get("data") if isinstance(value.get("data"), dict) else {}
    message = str(
        value.get("message")
        or data.get("message")
        or data.get("error")
        or value.get("name")
        or ""
    ).strip()
    return message or json.dumps(value, ensure_ascii=False, default=str)


def _custom_provider(handle: dict[str, Any]) -> str:
    options = handle.get("options")
    if not isinstance(options, SessionOptions):
        return ""
    return options.env.get("MUTEKI_OPENCODE_PROVIDER", "").strip()


# 1.18.30 实测：busy 期间的 prompt_async 在下一次 idle 之前进入同一忙循环。
# 证据 /tmp/muteki-p2b/evidence/opencode-steer.txt。端点不存在时仍为 False。
_PROMPT_ASYNC_SAME_TURN = True

_SUPPORTED_ACCESS_MODES = (
    AccessMode.SUPERVISED.value,
    AccessMode.AUTO_ACCEPT_EDITS.value,
    AccessMode.FULL_ACCESS.value,
)
_OPENCODE_EDIT_PERMISSIONS = frozenset({
    "edit", "write", "patch", "apply_patch",
})
_OPENCODE_RESTRICTED_PERMISSIONS = (
    "bash",
    "edit",
    "write",
    "patch",
    "apply_patch",
    "webfetch",
    "websearch",
    "codesearch",
    "external_directory",
    "doom_loop",
)
_OPENCODE_PLAN_FILE_PATTERN = ".opencode/plans/*.md"
_ENV_READ_ASK = (
    ("read", ".env"),
    ("read", "*.env"),
    ("read", ".env.*"),
    ("read", "*.env.*"),
)
_ENV_READ_ALLOW = (
    ("read", ".env.example"),
    ("read", "*.env.example"),
)
_VERSION_MAJOR = re.compile(r"(?:^|[^0-9])(\d+)\.\d+")


def _opencode_major(version: str) -> Optional[int]:
    """Return the numeric major from ``opencode --version``, or None."""
    match = _VERSION_MAJOR.search(str(version or "").strip())
    if match is None:
        return None
    return int(match.group(1))


def _rule(permission: str, pattern: str, action: str) -> dict[str, str]:
    return {"permission": permission, "pattern": pattern, "action": action}


def _permission_rules(
    access_mode: str, *, plan: bool = False, child_guard: bool = False,
) -> list[dict[str, str]]:
    """Build OpenCode's native per-session PermissionRuleset.

    Full access is the only mode that allows everything. Every other mode
    asks, except auto-accept-edits which allows the edit family. ``.env``
    reads stay ask so a broader read rule cannot silently allow them.
    A task (subagent) session starts its loop before this process can patch
    it, and a loop keeps the rules it started with, so the child's policy has
    to exist when the child is created. OpenCode copies only deny rules from
    the parent. ``child_guard`` therefore leads with denies on the restricted
    permissions using the pattern ``**``: unlike a ``*`` deny it does not
    remove the tool from the child's tool list, and the parent's later ask
    rules still win for the parent. Managed servers get the same policy from
    ``OPENCODE_CONFIG_CONTENT`` instead (see ``_permission_config``).

    Session rules are evaluated after the agent's own rules (last match wins),
    so they would override the plan agent's edit deny. A plan turn therefore
    closes the edit family again, leaving only the plan agent's own plan file,
    and denies ``plan_exit`` so the agent cannot switch to the build agent in
    the middle of a turn; leaving plan mode is the next turn's decision.
    """
    if access_mode == AccessMode.FULL_ACCESS.value:
        rules = [_rule("*", "*", "allow")]
    else:
        edit_action = (
            "allow" if access_mode == AccessMode.AUTO_ACCEPT_EDITS.value else "ask"
        )
        rules = []
        if child_guard:
            rules.extend(
                _rule(permission, "**", "deny")
                for permission in _OPENCODE_RESTRICTED_PERMISSIONS)
        rules.append(_rule("*", "*", "ask"))
        for permission in _OPENCODE_RESTRICTED_PERMISSIONS:
            action = (
                edit_action if permission in _OPENCODE_EDIT_PERMISSIONS else "ask")
            rules.append(_rule(permission, "*", action))
        rules.append(_rule("question", "*", "allow"))
        rules.extend(
            _rule(permission, pattern, "ask")
            for permission, pattern in _ENV_READ_ASK)
        rules.extend(
            _rule(permission, pattern, "allow")
            for permission, pattern in _ENV_READ_ALLOW)
    if plan:
        rules.extend(
            _rule(permission, "*", "deny")
            for permission in sorted(_OPENCODE_EDIT_PERMISSIONS))
        rules.append(_rule("edit", _OPENCODE_PLAN_FILE_PATTERN, "allow"))
        rules.append(_rule("plan_exit", "*", "deny"))
    return rules


def _permission_config(access_mode: str) -> Optional[dict[str, Any]]:
    """The access-mode policy in OpenCode's config ``permission`` form.

    Config permissions are part of every agent's ruleset from the moment the
    server starts, so subagent sessions are governed without any race. Full
    access leaves the user's config alone.
    """
    if access_mode == AccessMode.FULL_ACCESS.value:
        return None
    grouped: dict[str, dict[str, str]] = {}
    for rule in _permission_rules(access_mode):
        grouped.setdefault(rule["permission"], {})[rule["pattern"]] = rule["action"]
    return {
        permission: next(iter(patterns.values()))
        if list(patterns) == ["*"] else patterns
        for permission, patterns in grouped.items()
    }


def _merged_config_content(existing: Optional[str], permission: dict[str, Any]) -> str:
    """Inline config JSON carrying ``permission``; keeps any inline config given."""
    base: dict[str, Any] = {}
    if existing:
        try:
            loaded = json.loads(existing)
        except ValueError as exc:
            raise OpenCodeError(
                f"OPENCODE_CONFIG_CONTENT 不是有效 JSON：{exc}",
                code="opencode.config_invalid") from exc
        if not isinstance(loaded, dict):
            raise OpenCodeError(
                "OPENCODE_CONFIG_CONTENT 必须是 JSON 对象。",
                code="opencode.config_invalid")
        base = loaded
    base["permission"] = permission
    return json.dumps(base)


def _rule_key(rule: dict[str, Any]) -> tuple[str, str, str]:
    return (
        str(rule.get("permission") or ""),
        str(rule.get("pattern") or ""),
        str(rule.get("action") or ""),
    )


def _child_permission_rules(
    parent: list[dict[str, str]], native_child: list[Any],
) -> list[dict[str, str]]:
    """Parent policy plus the denies OpenCode added for the child agent itself.

    A task session starts with only deny rules (the parent's leading denies and
    child-specific ones such as ``task``). The complete parent policy goes in
    front of them. Child rules that are not denies are dropped: placed after
    the parent policy they would override its asks.
    """
    inherited = {_rule_key(rule) for rule in parent}
    extra: list[dict[str, str]] = []
    for rule in native_child:
        if not isinstance(rule, dict) or _rule_key(rule) in inherited:
            continue
        if str(rule.get("action") or "") != "deny":
            continue
        extra.append(_rule(
            str(rule.get("permission") or ""),
            str(rule.get("pattern") or "*"),
            "deny"))
    return [*parent, *extra]


def _operation_body_properties(doc: dict[str, Any], path: str, method: str) -> set[str]:
    paths = doc.get("paths") if isinstance(doc.get("paths"), dict) else {}
    operation = paths.get(path) if isinstance(paths.get(path), dict) else {}
    spec = operation.get(method) if isinstance(operation.get(method), dict) else {}
    body = spec.get("requestBody") if isinstance(spec.get("requestBody"), dict) else {}
    content = body.get("content") if isinstance(body.get("content"), dict) else {}
    json_body = content.get("application/json") if isinstance(
        content.get("application/json"), dict) else {}
    schema = json_body.get("schema") if isinstance(json_body.get("schema"), dict) else {}
    props = schema.get("properties") if isinstance(schema.get("properties"), dict) else {}
    return set(props)


def _openapi_features(doc: dict[str, Any]) -> dict[str, Any]:
    paths = doc.get("paths") if isinstance(doc.get("paths"), dict) else {}

    def has(path: str) -> bool:
        return any(
            item == path or item.startswith(path + "/") or item.startswith(path + "{")
            for item in paths
        )

    prompt_props = _operation_body_properties(
        doc, "/session/{sessionID}/prompt_async", "post")
    return {
        "paths": paths,
        "has": has,
        "prompt_async": has("/session/{sessionID}/prompt_async"),
        "abort": has("/session/{sessionID}/abort"),
        "agent": "agent" in prompt_props,
        "variant": "variant" in prompt_props,
        "event": has("/event"),
        "permission": has("/permission"),
        "question": has("/question"),
        "todo": has("/session/{sessionID}/todo"),
        "summarize": any(
            item.startswith("/session/") and item.endswith("/summarize")
            for item in paths
        ),
        "mcp": has("/mcp"),
    }


def _new_admission(message_id: str, generation: int) -> dict[str, Any]:
    return {
        "pending": True,
        "accepted": False,
        "message_observed": False,
        "idle_during": False,
        "message_id": message_id,
        "generation": generation,
    }


def _advance_admission(admission: Optional[dict[str, Any]], signal: str) -> str:
    """T3 ``advanceOpenCodePromptAdmission``. Returns hold, reconcile-idle, or release."""
    if not admission or not admission.get("pending"):
        return "release"
    if signal == "assistant-completed":
        admission["accepted"] = True
        admission["message_observed"] = True
        admission["pending"] = False
        return "release"
    if signal == "idle":
        admission["idle_during"] = True
        return "hold"
    if signal == "accepted":
        admission["accepted"] = True
    if signal in {"busy", "user-message"}:
        admission["message_observed"] = True
    if not admission.get("accepted") or not admission.get("message_observed"):
        return "hold"
    if admission.get("idle_during"):
        return "reconcile-idle"
    admission["pending"] = False
    return "release"


def _question_answers(
    questions: list[dict[str, Any]], answers: dict[str, Any],
) -> list[list[str]]:
    """Operator answers keyed by ``question_id`` -> OpenCode ``Answer[][]``.

    One list of labels per question, in the order OpenCode asked them. A typed
    answer is sent as the custom answer text.
    """
    rows: list[list[str]] = []
    for question in questions:
        entry = answers.get(str(question.get("question_id") or ""))
        values, text = _answer_values(entry)
        rows.append(values or ([text.strip()] if text.strip() else []))
    return rows


def _plan_from_todos(todos: Any) -> PlanPayload:
    tasks: list[PlanTaskPayload] = []
    if isinstance(todos, list):
        for index, todo in enumerate(todos, start=1):
            if not isinstance(todo, dict):
                continue
            status = str(todo.get("status") or "pending").strip()
            if status == "canceled":
                status = "cancelled"
            if status not in {"pending", "in_progress", "completed", "blocked", "cancelled"}:
                status = "pending"
            title = str(todo.get("content") or "").strip() or f"Todo {index}"
            tasks.append(PlanTaskPayload(
                task_id=f"todo-{index}", title=title, status=status))
    return PlanPayload(tasks=tasks, patch=False)


def _reasoning_increment(
    seen: dict[str, str], part_id: str, full_text: str,
) -> str:
    """Return only the unseen suffix of a growing reasoning snapshot."""
    previous = seen.get(part_id, "")
    if not full_text or full_text == previous:
        return ""
    if full_text.startswith(previous):
        delta = full_text[len(previous):]
        seen[part_id] = full_text
        return delta
    if previous.startswith(full_text):
        return ""
    seen[part_id] = full_text
    return full_text


def _as_process(proc: Any) -> Optional[asyncio.subprocess.Process]:
    if isinstance(proc, SupervisedProcess):
        return proc.process
    if isinstance(proc, asyncio.subprocess.Process):
        return proc
    return None


def _opencode_commands(value: Any) -> list[dict[str, Any]]:
    """Normalize the stable ``GET /command`` response."""
    if isinstance(value, dict):
        value = value.get("data") or value.get("commands") or []
    if not isinstance(value, list):
        return []
    commands: list[dict[str, Any]] = []
    for raw in value:
        if isinstance(raw, str):
            name = raw.strip().lstrip("/")
            item: dict[str, Any] = {"name": name}
        elif isinstance(raw, dict):
            name = str(
                raw.get("name") or raw.get("command") or raw.get("id") or ""
            ).strip().lstrip("/")
            item = dict(raw)
            item["name"] = name
        else:
            continue
        if name:
            commands.append(item)
    return commands


def _opencode_mcp_statuses(value: Any) -> list[dict[str, Any]]:
    """Normalize the stable ``GET /mcp`` response into named states."""
    if isinstance(value, dict) and isinstance(value.get("data"), dict):
        value = value["data"]
    if not isinstance(value, dict):
        return []
    return [
        {"name": str(name), **(dict(state) if isinstance(state, dict)
                                else {"status": str(state)})}
        for name, state in value.items()
    ]


class OpenCodeServerAdapter(BaseExternalAgentAdapter, RuntimeOperationAdapter):
    """OpenCode 的 Server（HTTP + SSE）结构化 Adapter。

    覆盖任务书 RUNTIME-04 要求的 session、prompt、permission、question、
    event stream、resume、cwd 与 MCP 注入：

    - ``send``：``POST /session/:id/prompt_async`` 只负责接纳；SSE 上的
      idle（接纳跟踪放开之后）才是回合完成。事件流归一化为
      delta/reasoning/tool/permission/question/plan/usage；
    - ``interrupt``：``POST /session/:id/abort``。没有 abort 端点时返回
      ``opencode.abort_unsupported``，不杀 server；
    - ``resume``：重新采用既有 session id（先 GET 探测）；session 持久化
      于 opencode 全局存储，同 cwd 的新 server 可跨进程恢复（2026-08-21
      本机实测）；跨 cwd 恢复需 fork，本 Adapter 不自动 fork；
    - cwd：Adapter 自管 server 以 ``cwd`` 启动（session 继承 server
      工作目录）；attach 模式 cwd 由外部 server 决定并记入 degradations；
    - 审批/提问：沿用 OpenCode 会话的原生 permission rules；需要用户决定
      的请求接到 Conversation 审批卡。
    """

    adapter_id = "opencode.server"
    context_compaction_events = True

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        base_url: Optional[str] = None,
        server_username: str = "",
        server_password: str = "",
        manage_server: bool = True,
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
        extra_env: Optional[dict[str, str]] = None,
        log_root: Optional[str | Path] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OPENCODE_BIN",
                                                "opencode")
        self._log_root = Path(log_root) if log_root is not None else None
        self._server_outputs: dict[int, ProcessOutputLog] = {}
        self._attach_url = base_url
        self._server_username = server_username or os.environ.get(
            "OPENCODE_SERVER_USERNAME", "")
        self._server_password = server_password or os.environ.get(
            "OPENCODE_SERVER_PASSWORD", "")
        if not base_url and manage_server and not self._server_password:
            # A loopback server without auth lets any local process drive the
            # session (shell included). The secret lives only in this process
            # and the child's environment, never in argv or logs.
            self._server_password = secrets.token_urlsafe(24)
            self._server_username = self._server_username or "opencode"
        self._manage_server = manage_server
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        self._extra_env = dict(extra_env or {})
        # agent_session_id -> 会话句柄
        self._sessions: dict[str, dict[str, Any]] = {}
        self._generation: Optional[str] = None
        self._runtime_version = ""
        self._v2_runtime: Any = None

    def _v2(self) -> Any:
        if self._v2_runtime is None:
            from .opencode2 import OpenCode2Runtime
            self._v2_runtime = OpenCode2Runtime(self)
        return self._v2_runtime

    async def _select_generation(self, *, refresh: bool = False) -> str:
        """T3's explicit runtime probe; a failed probe never chooses a protocol."""
        if self._generation is not None and not refresh:
            return self._generation
        if self._attach_url:
            client = OpenCodeHttpClient(self._attach_url, username=self._server_username,
                                        password=self._server_password)
            try:
                for path in ("/api/info", "/global/health"):
                    response = await client._client.get(path, timeout=5)
                    if response.status_code == 401:
                        raise OpenCodeError("OpenCode server rejected its configured password",
                                            code="opencode.server_unauthorized", status=401)
                    if response.status_code != 200 or response.headers.get("content-type", "").split(";")[0] != "application/json":
                        continue
                    body = response.json()
                    if not isinstance(body, dict):
                        continue
                    valid = (isinstance(body.get("pid"), int) if path == "/api/info"
                             else body.get("healthy") is True)
                    major = _opencode_major(str(body.get("version") or ""))
                    if valid and major is not None and ((path == "/api/info" and major >= 2)
                                                       or (path == "/global/health" and major == 1)):
                        self._runtime_version = str(body["version"])
                        self._generation = "v2" if major >= 2 else "v1"
                        return self._generation
            finally:
                await client.close()
            raise OpenCodeError("Configured server did not identify its OpenCode protocol",
                                code="opencode.version_unreadable")
        version = await asyncio.to_thread(_probe_version, self._binary)
        major = _opencode_major(version)
        if major is None:
            raise OpenCodeError("OpenCode CLI did not report a readable version",
                                code="opencode.version_unreadable")
        self._generation = "v2" if major >= 2 else "v1"
        self._runtime_version = version
        return self._generation

    # -- server 生命周期 -------------------------------------------------------

    def _server_env(
        self, session_env: Optional[dict[str, str]] = None
    ) -> dict[str, str]:
        env = {
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
            "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
        }
        env.update(self._extra_env)
        env.update(dict(session_env or {}))
        if self._server_password or self._server_username:
            env["OPENCODE_SERVER_USERNAME"] = self._server_username \
                or "opencode"
            env["OPENCODE_SERVER_PASSWORD"] = self._server_password
        return env

    def _serve_argv(self, port: int) -> list[str]:
        """自管 server 进程命令行（子类/测试可覆盖指向 mock server）。"""
        return [self._binary, "serve", "--hostname", "127.0.0.1",
                "--port", str(port), "--pure"]

    def _require_v1(self) -> str:
        """Guard the v1 branch using the actual runtime's version identity."""
        version = self._runtime_version if self._attach_url else _probe_version(self._binary)
        major = _opencode_major(version)
        if major is not None and major >= 2:
            raise OpenCodeError(
                f"opencode {version} is major version {major}; this adapter "
                "only speaks the v1 HTTP API",
                code="opencode.major_unsupported",
            )
        return version

    async def _start_server(
        self,
        cwd: str,
        *,
        port: Optional[int] = None,
        env: Optional[dict[str, str]] = None,
        session_id: str = "",
    ) -> "tuple[Optional[SupervisedProcess], str, dict[str, Any]]":
        """拉起自管 server 并等待 ``GET /doc``；返回 (监督进程, base_url, doc)。

        attach 模式（``base_url`` 构造参数非空）不拉起进程，直接读取既有
        server 的 ``/doc``。
        """
        if self._attach_url:
            client = OpenCodeHttpClient(
                self._attach_url, username=self._server_username,
                password=self._server_password)
            try:
                doc = await client.get("/doc", timeout=self._startup_timeout)
                health = await client.get(
                    "/global/health", timeout=self._startup_timeout)
            finally:
                await client.close()
            version = str(
                health.get("version") if isinstance(health, dict) else "")
            major = _opencode_major(version)
            if major is None:
                raise OpenCodeError(
                    f"attached opencode server reports version {version!r}, "
                    "which has no readable major",
                    code="opencode.version_unreadable")
            if major >= 2:
                raise OpenCodeError(
                    f"attached opencode server is major version {major}; "
                    "this adapter only speaks the v1 HTTP API",
                    code="opencode.major_unsupported")
            return None, self._attach_url, doc if isinstance(doc, dict) else {}
        if not self._manage_server:
            raise OpenCodeError("manage_server=False 时必须提供 base_url")
        port = port or _pick_free_port()
        base_url = f"http://127.0.0.1:{port}"
        process_cwd = os.path.abspath(cwd or os.getcwd())
        argv = self._with_launch_args(self._serve_argv(port))
        if (env or {}).get("MUTEKI_CHAT_PRIVATE_ROOT"):
            argv = [value for value in argv if value != "--pure"]
        output = ProcessOutputLog.create(
            self._log_root, label=f"opencode-serve-{port}")
        try:
            supervised = await spawn_supervised(
                argv,
                adapter_id=self.adapter_id,
                session_id=session_id,
                label=f"opencode-serve-{port}",
                cwd=process_cwd,
                env=subprocess_environment(
                    {**self._server_env(env), "PWD": process_cwd}),
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except BaseException:
            await output.close()
            raise
        process = supervised.process
        output.attach(process)
        self._server_outputs[process.pid] = output
        client = OpenCodeHttpClient(
            base_url, username=self._server_username,
            password=self._server_password)
        doc: Any = {}
        try:
            deadline = asyncio.get_running_loop().time() + self._startup_timeout
            while True:
                if process.returncode is not None:
                    await output.close()
                    raise OpenCodeError(
                        f"opencode serve 提前退出（code={process.returncode}）；"
                        f"{output.detail()}")
                try:
                    doc = await client.get("/doc", timeout=5.0)
                    break
                except OpenCodeError:
                    if asyncio.get_running_loop().time() > deadline:
                        raise OpenCodeError(
                            "opencode serve 启动超时（/doc 不可达）；"
                            f"{output.detail()}")
                    await asyncio.sleep(0.2)
        except BaseException:
            await self._stop_server(supervised)
            raise
        finally:
            await client.close()
        return supervised, base_url, doc if isinstance(doc, dict) else {}

    def server_output_detail(self, proc: Any) -> str:
        process = _as_process(proc)
        output = self._server_outputs.get(process.pid) if process is not None else None
        return output.detail() if output is not None else ""

    async def _stop_server(self, proc: Any) -> int:
        """Stop a supervised server process group. Not used as interrupt."""
        process = _as_process(proc)
        if process is None:
            return -1
        try:
            if isinstance(proc, SupervisedProcess):
                await proc.terminate()
            elif process.returncode is None:
                try:
                    process.terminate()
                    await asyncio.wait_for(process.wait(), timeout=5.0)
                except (asyncio.TimeoutError, ProcessLookupError):
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
                    await process.wait()
        finally:
            output = self._server_outputs.pop(process.pid, None)
            if output is not None:
                await output.close()
        return int(process.returncode if process.returncode is not None else -1)

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        try:
            generation = await self._select_generation(refresh=True)
        except (OpenCodeError, OSError, httpx.HTTPError, ValueError) as exc:
            caps = conservative_capabilities(transport_kind="http", capability_source=SOURCE_STATIC)
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id, instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources={name: SOURCE_STATIC for name in BOOL_CAPABILITY_FIELDS},
                degradations=[f"{getattr(exc, 'code', 'opencode.probe_failed')}: {exc}"], detail=str(exc))
            return caps
        if generation == "v2":
            return await self._v2().probe(request)
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="http", capability_source=SOURCE_STATIC)
        try:
            version = self._require_v1()
        except OpenCodeError as exc:
            caps.runtime_version = _probe_version(self._binary)
            degradations.append(f"{exc.code}: {exc}")
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=str(exc),
            )
            return caps
        caps.runtime_version = version
        detail = ""
        doc: dict[str, Any] = {}
        if version or self._attach_url:
            proc: Any = None
            try:
                proc, _base_url, doc = await self._start_server(os.getcwd())
            except (OSError, OpenCodeError, asyncio.TimeoutError) as exc:
                detail = f"opencode serve 探测失败：{exc}"
                doc = {}
            finally:
                if proc is not None:
                    await self._stop_server(proc)
        if not isinstance(doc, dict):
            doc = {}
        features = _openapi_features(doc)
        paths = features["paths"]

        probed = bool(paths)
        if not probed:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "structured transport 不可用：HTTP/SSE session/prompt/"
                "permission/question/mcp 注入均未实测，按保守默认 False"
                "（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 /doc 不可达",
            )
            return caps

        # 能力以 OpenAPI paths 实测为准（不按版本号推断能力，2.x 已在上面拒绝）。
        has = features["has"]
        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = str(
            (doc.get("openapi") or doc.get("swagger") or ""))
        caps.streaming = features["event"]
        caps.tool_events = features["event"]
        caps.usage_events = True  # assistant message.tokens（含 reasoning）
        caps.interrupt = bool(features["abort"])
        caps.approval = features["permission"]
        caps.access_modes = list(_SUPPORTED_ACCESS_MODES)
        caps.user_input = features["question"]
        caps.resume = True  # session id 重新采用（需同一 server 存活）
        caps.resume_continues_turn = False
        caps.session_persistence = True
        # POST /mcp 动态注册（§OPENCODE-3）→ MCP 注入档。
        caps.mcp = features["mcp"]
        # 1.18.30 临时目录实测：busy 期间 prompt_async 的文本在同一次
        # busy 结束（唯一一次 idle）之前进入下一 provider step。
        caps.steer = bool(features["prompt_async"] and _PROMPT_ASYNC_SAME_TURN)
        caps.plan = bool(features["todo"])
        caps.plan_mode = bool(features["agent"])
        # task 工具产生带 parentID 的 child session，其工具/审批/提问都经
        # /event 上报（SSE 实测），没有事件流就无法归属。
        caps.subagents = features["event"]
        caps.compaction = bool(features["summarize"])
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not caps.steer:
            degradations.append(
                "opencode.steer_unsupported: prompt_async 不在这份 OpenAPI "
                "里，或同回合实测未通过；不把同步 /message 当成 steer")
        if not caps.interrupt:
            degradations.append(
                "opencode.abort_unsupported: 没有 POST /session/:id/abort，"
                "interrupt 不会靠杀掉 server 假装成功")
        if not caps.plan_mode:
            degradations.append(
                "opencode.plan_unsupported: prompt body 没有 agent 字段，"
                "interaction_mode=plan 会类型化拒绝")
        if not caps.mcp:
            degradations.append(
                "无 POST /mcp 端点：MCP 注入不可用，能力注入降级（由 "
                "select_injection_kind 选择后续档位）")
        if self._attach_url:
            degradations.append(
                "attach 外部 server 模式：cwd 由外部 server 决定；"
                "POST /mcp 动态注入对外部 server 是否生效属待确认项"
                "（T3 侧限制），失败只记 warning 不静默忽略")
        degradations.append(
            "resume 语义为重新采用 session id：session 持久化于 opencode "
            "全局存储（同 cwd 新 server 可跨进程恢复，2026-08-21 本机实测）；"
            "跨 cwd/跨项目恢复需 fork，本 Adapter 不自动 fork")
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail=f"opencode serve OpenAPI 探测成功（{len(paths)} 条路径）",
        )
        return caps

    # -- 注入物化 --------------------------------------------------------------

    async def _inject_mcp(
        self,
        client: OpenCodeHttpClient,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> "tuple[int, list[str]]":
        """把注入计划的 mcpServers 物化为 ``POST /mcp`` 动态注册。

        返回 ``(注入数量, 警告清单)``；token 缺失时占位符 server 跳过
        （不注入无效凭据），单条注册失败只记 warning（§OPENCODE 影响：
        对外部 server 的适用性待确认）。
        """
        if plan is None or plan.injection_kind is not InjectionKind.MCP:
            return 0, []
        cfg = (plan.runtime_config or {}).get("mcpServers")
        if not isinstance(cfg, dict) or not cfg:
            return 0, []
        injected = 0
        warnings: list[str] = []
        for name, entry in cfg.items():
            entry = dict(entry or {})
            headers: dict[str, str] = {}
            usable = True
            raw_headers = entry.get("headers")
            items = (raw_headers.items() if isinstance(raw_headers, dict)
                     else ((h.get("name"), h.get("value"))
                           for h in (raw_headers or [])))
            for key, value in items:
                value = str(value or "")
                if TOKEN_PLACEHOLDER in value:
                    if not bearer_token:
                        usable = False
                        break
                    value = value.replace(TOKEN_PLACEHOLDER, bearer_token)
                headers[str(key)] = value
            if not usable:
                warnings.append(f"mcp server {name!r} 缺 bearer token，跳过注入")
                continue
            try:
                await client.post("/mcp", {
                    "name": str(name),
                    "config": {
                        "type": "remote",
                        "url": str(entry.get("url")
                                   or plan.gateway_endpoint),
                        "headers": headers,
                        "oauth": False,
                    },
                })
                injected += 1
            except OpenCodeError as exc:
                warnings.append(f"POST /mcp {name!r} 失败：{exc}")
        return injected, warnings

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        if await self._select_generation(refresh=True) == "v2":
            return await self._v2().launch(request, plan, bearer_token)
        await asyncio.to_thread(self._require_v1)
        cwd = request.options.cwd or os.getcwd()
        session_env = dict(request.options.env)
        proc: Any = None
        client: Optional[OpenCodeHttpClient] = None
        try:
            access_mode = str(
                request.access_mode or AccessMode.SUPERVISED.value
            ).strip()
            if access_mode not in _SUPPORTED_ACCESS_MODES:
                raise AccessModeUnsupportedError(
                    self.adapter_id, access_mode, _SUPPORTED_ACCESS_MODES,
                    "OpenCode has no native auto-review mode")
            server_env = dict(session_env)
            config = _permission_config(access_mode)
            if config is not None and not self._attach_url:
                server_env["OPENCODE_CONFIG_CONTENT"] = _merged_config_content(
                    server_env.get("OPENCODE_CONFIG_CONTENT")
                    or self._extra_env.get("OPENCODE_CONFIG_CONTENT"),
                    config)
            proc, base_url, doc = await self._start_server(
                cwd, env=server_env, session_id=request.agent_session_id)
            features = _openapi_features(doc if isinstance(doc, dict) else {})
            client = OpenCodeHttpClient(
                base_url, username=self._server_username,
                password=self._server_password,
                timeout=self._prompt_timeout + 30.0)

            resume_handle = request.resume_handle
            warnings: list[str] = []
            child_guard = bool(self._attach_url)
            permissions = _permission_rules(
                access_mode, child_guard=child_guard)
            if resume_handle:
                # resume = 重新采用既有 session id（先探测存在性）。
                try:
                    await client.get(f"/session/{resume_handle}")
                except OpenCodeError as exc:
                    raise OpenCodeError(
                        f"resume 失败：session {resume_handle} 在此 server "
                        f"不可达（{exc}）") from exc
                session_id = resume_handle
                await client.patch(
                    f"/session/{session_id}", {"permission": permissions})
            else:
                body: dict[str, Any] = {"permission": permissions}
                title = request.options.title
                if title:
                    body["title"] = title
                created = await client.post("/session", body)
                session_id = str((created or {}).get("id") or "")
                if not session_id:
                    raise OpenCodeError("POST /session 未返回 session id")

            injected, inject_warnings = await self._inject_mcp(
                client, plan, bearer_token)
            warnings.extend(inject_warnings)
            try:
                available_commands = _opencode_commands(
                    await client.get("/command"))
            except OpenCodeError as exc:
                available_commands = []
                warnings.append(
                    f"GET /command 失败，Runtime 命令目录不可用：{exc}"
                )
            try:
                mcp_statuses = _opencode_mcp_statuses(
                    await client.get("/mcp"))
            except OpenCodeError as exc:
                mcp_statuses = []
                warnings.append(
                    f"GET /mcp 失败，Runtime MCP 状态不可用：{exc}"
                )
        except BaseException:
            if client is not None:
                await client.close()
            if proc is not None:
                await self._stop_server(proc)
            raise

        handle: dict[str, Any] = {
            "conversation_thread_id": request.thread_id,
            "proc": proc,
            "client": client,
            "base_url": base_url,
            "cwd": cwd,
            "options": request.options,
            "turns": 0,
            "external_session_id": session_id,
            "model": request.model,
            "variant": (
                request.effort
                if request.effort and request.effort != "default"
                and features.get("variant") else None
            ),
            "variant_supported": bool(features.get("variant")),
            "plan_agent": bool(features.get("agent")),
            "prompt_async": bool(features.get("prompt_async")),
            "abort_supported": bool(features.get("abort")),
            "permissions": permissions,
            "child_guard": child_guard,
            "permission_plan": False,
            "resumed": bool(resume_handle),
            "event_sink": None,
            "current_turn_id": None,
            "sse_stop": None,
            "sse_task": None,
            "background_tasks": set(),
            "approval_grants": SessionApprovalGrants(access_mode),
            "mcp_injected": injected,
            "warnings": warnings,
            "access_mode": access_mode,
            "pending_approvals": {},
            "pending_questions": {},
            "available_commands": available_commands,
            "mcp_statuses": mcp_statuses,
            "capability_revision": 1,
        }
        self._sessions[request.agent_session_id] = handle
        try:
            await self._open_event_stream(request.agent_session_id, handle)
        except BaseException:
            self._sessions.pop(request.agent_session_id, None)
            await client.close()
            if proc is not None:
                await self._stop_server(proc)
            raise
        return {"external_session_id": session_id,
                "resume_handle": session_id}

    # -- SSE 订阅生命周期 --------------------------------------------------------

    async def _open_event_stream(
        self, agent_session_id: str, handle: dict[str, Any],
    ) -> None:
        """Subscribe to ``GET /event`` and wait for ``server.connected``.

        Callers send ``prompt_async`` only after this returns, so no event of
        the prompt can predate the subscription.
        """
        client: OpenCodeHttpClient = handle["client"]
        stop = asyncio.Event()
        connected = asyncio.Event()
        task = asyncio.ensure_future(client.stream_sse(
            "/event",
            lambda name, data: self._dispatch_sse(
                agent_session_id, name, data),
            stop, connected))
        waiter = asyncio.ensure_future(connected.wait())
        try:
            await asyncio.wait(
                {task, waiter}, timeout=self._startup_timeout,
                return_when=asyncio.FIRST_COMPLETED)
        finally:
            waiter.cancel()
        if not connected.is_set():
            stop.set()
            if not task.done():
                task.cancel()
            results = await asyncio.gather(task, return_exceptions=True)
            error = results[0]
            if isinstance(error, OpenCodeError):
                raise error
            raise OpenCodeError(
                "GET /event did not deliver server.connected in time",
                code="opencode.sse_connect_timeout")
        handle["sse_stop"] = stop
        handle["sse_task"] = task

    async def _close_event_stream(self, handle: dict[str, Any]) -> None:
        stop = handle.get("sse_stop")
        if stop is not None:
            stop.set()
        task = handle.get("sse_task")
        handle["sse_stop"] = None
        handle["sse_task"] = None
        if task is not None:
            if not task.done():
                task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    def _spawn(self, handle: dict[str, Any], coro: Any) -> "asyncio.Future[Any]":
        """Run a fire-and-forget coroutine that ``_teardown`` can cancel."""
        task = asyncio.ensure_future(coro)
        tasks: set = handle["background_tasks"]
        tasks.add(task)
        task.add_done_callback(tasks.discard)
        return task

    # -- SSE 事件归一化 ------------------------------------------------------------

    def _queue_event(
        self, handle: dict[str, Any], etype: Any, native_type: str, payload: Any,
    ) -> None:
        item = (etype, native_type, payload)
        sink = handle.get("event_sink")
        if handle.get("generation") == "v2" and isinstance(etype, AgentEventType):
            origin = handle.get("routing_turn_id")
            if origin and (origin != handle.get("conversation_turn_id")
                           or sink is None or handle.get("execution_ended")):
                callback = handle.get("background_handler")
                if callback is not None:
                    callback(origin, [item])
                    return
        if sink is not None:
            sink.put_nowait(item)
            return
        if isinstance(etype, str) and etype.startswith("__"):
            return
        handle.setdefault("deferred_events", []).append(item)

    def _signal_turn(self, handle: dict[str, Any], kind: str, payload: Any = None) -> None:
        sink = handle.get("event_sink")
        if sink is not None:
            sink.put_nowait((kind, "", payload))

    def _note_admission(self, handle: dict[str, Any], signal: str) -> str:
        action = _advance_admission(handle.get("admission"), signal)
        if action == "reconcile-idle":
            self._spawn(handle, self._reconcile_admission(handle))
        return action

    def _note_root_status(self, handle: dict[str, Any], status_type: str) -> None:
        handle["root_status_revision"] = int(handle.get("root_status_revision") or 0) + 1
        if status_type == "busy":
            self._note_admission(handle, "busy")
            return
        if status_type != "idle":
            return
        handle.pop("settlement_pending", None)
        settle_idle = handle.get("settle_idle")
        if settle_idle is not None:
            settle_idle.set()
        action = self._note_admission(handle, "idle")
        if action == "hold":
            return
        if action == "reconcile-idle":
            return
        self._signal_turn(handle, "__turn_idle__")

    def _session_status_kind(
        self, handle: dict[str, Any], status: Any, session_id: str,
    ) -> Optional[str]:
        """Only a valid status map can establish that an absent session is idle."""
        kinds = {"busy", "idle", "retry"}
        if isinstance(status, dict) and all(
            isinstance(key, str) and (
                isinstance(row, dict) and isinstance(row.get("type"), str)
                and row["type"] in kinds
                or isinstance(row, str) and row in kinds
            )
            for key, row in status.items()
        ):
            if session_id not in status:
                return "idle"
            row = status[session_id]
            return row["type"] if isinstance(row, dict) else row
        self._emit_warning(
            handle, "opencode.status_invalid",
            "OpenCode session status is not a valid status map: "
            + json.dumps(status, ensure_ascii=False))
        return None

    async def _session_is_idle(
        self, handle: dict[str, Any], client: OpenCodeHttpClient,
        session_id: str,
    ) -> bool:
        """True when OpenCode reports the root session idle (absent = idle)."""
        if handle.get("pending_approvals") or handle.get("pending_questions"):
            return False
        status_revision = int(handle.get("root_status_revision") or 0)
        try:
            status = await client.get("/session/status")
        except OpenCodeError as exc:
            self._emit_warning(
                handle, "opencode.reconcile_failed",
                f"OpenCode session status could not be read: {exc}")
            return False
        if (int(handle.get("root_status_revision") or 0) != status_revision
                or handle.get("pending_approvals") or handle.get("pending_questions")):
            return False
        return self._session_status_kind(handle, status, session_id) == "idle"

    async def _wait_settled(
        self, handle: dict[str, Any], settled: asyncio.Event, what: str,
    ) -> None:
        try:
            await asyncio.wait_for(settled.wait(), timeout=_SETTLE_IDLE_WAIT_S)
        except asyncio.TimeoutError:
            handle["settlement_pending"] = what
            message = f"OpenCode did not report the session idle after {what}"
            self._emit_warning(
                handle, "opencode.settle_timeout",
                message)
            raise OpenCodeError(message, code="opencode.settle_timeout") from None

    async def _reconcile_admission(self, handle: dict[str, Any]) -> None:
        admission = handle.get("admission") or {}
        generation = admission.get("generation")
        status_revision = int(handle.get("root_status_revision") or 0)
        client: Optional[OpenCodeHttpClient] = handle.get("client")
        session_id = str(handle.get("external_session_id") or "")
        if client is None or not session_id:
            return
        try:
            status = await client.get("/session/status")
        except OpenCodeError as exc:
            self._emit_warning(
                handle, "opencode.reconcile_failed",
                f"OpenCode session status could not be read: {exc}")
            return
        current = handle.get("admission") or {}
        # A status event received while this request was in flight is newer
        # than its REST snapshot. Never let an older idle end that busy turn.
        if int(handle.get("root_status_revision") or 0) != status_revision:
            return
        if current.get("generation") != generation or not current.get("pending"):
            return
        kind = self._session_status_kind(handle, status, session_id)
        if kind == "idle":
            current["pending"] = False
            self._signal_turn(handle, "__turn_idle__")
        elif kind in {"busy", "retry"}:
            current["pending"] = False
            current["idle_during"] = False

    def _emit_reasoning(self, handle: dict[str, Any], part: dict[str, Any]) -> None:
        part_id = str(part.get("id") or "")
        text = _part_text(part)
        if not part_id or not text:
            return
        delta = _reasoning_increment(
            handle.setdefault("reasoning_text", {}), part_id, text)
        if not delta:
            return
        completed = isinstance(part.get("time"), dict) and part["time"].get("end") is not None
        self._queue_event(
            handle, AgentEventType.REASONING_SUMMARY,
            "opencode.reasoning",
            dump_payload(ReasoningPayload(
                text=delta,
                channel="thinking",
                partial=not completed,
                item_id=part_id,
            )))

    def _emit_plan(self, handle: dict[str, Any], todos: Any) -> None:
        self._queue_event(
            handle, AgentEventType.PLAN_UPDATED, "opencode.todo.updated",
            dump_payload(_plan_from_todos(todos)))

    def _emit_warning(
        self, handle: dict[str, Any], code: str, message: str,
    ) -> None:
        self._queue_event(
            handle, AgentEventType.RUNTIME_WARNING, code,
            dump_payload(RuntimeWarningPayload(
                kind="notice", code=code, message=message)))

    def _schedule_child_permission(self, handle: dict[str, Any], child_id: str) -> None:
        root_id = str(handle.get("external_session_id") or "")
        if not child_id or child_id == root_id:
            return
        scheduled = handle.setdefault("child_permission_scheduled", set())
        if child_id in scheduled:
            return
        scheduled.add(child_id)
        self._spawn(handle, self._install_child_permission(handle, child_id))

    async def _install_child_permission(
        self, handle: dict[str, Any], child_id: str,
    ) -> None:
        # The child's running loop keeps the rules it started with, so this
        # patch only governs prompts sent to the child afterwards; the first
        # loop is governed by the server config / inherited guard denies.
        client: Optional[OpenCodeHttpClient] = handle.get("client")
        parent = list(handle.get("permissions") or [])
        if client is None or not parent:
            handle.get("child_permission_scheduled", set()).discard(child_id)
            return
        try:
            info = await client.get(f"/session/{child_id}")
            native = info.get("permission") if isinstance(info, dict) else []
            if not isinstance(native, list):
                native = []
            await client.patch(
                f"/session/{child_id}",
                {"permission": _child_permission_rules(parent, native)})
        except OpenCodeError as exc:
            handle.get("child_permission_scheduled", set()).discard(child_id)
            self._emit_warning(
                handle, "opencode.child_permission_failed",
                f"OpenCode child session permission install failed: {exc}")

    def _typed_receipt(
        self, session: Optional[AgentSessionRef], code: str, message: str,
        *, operation: str,
        category: ErrorCategory = ErrorCategory.STATE,
    ) -> CommandReceipt:
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.FAILED,
            aggregate=AggregateRef(
                type="agent_session",
                id=session.agent_session_id if session else "",
            ),
            error=ErrorEnvelope(
                code=code,
                message=message,
                category=category,
                retryable=False,
                detail={"operation": operation, "adapter_id": self.adapter_id},
            ),
        )

    def _prompt_body(
        self, handle: dict[str, Any], text: str, *,
        message_id: str, interaction_mode: str,
    ) -> dict[str, Any]:
        body: dict[str, Any] = {
            "messageID": message_id,
            "parts": [{"type": "text", "text": text}],
        }
        if interaction_mode == "plan":
            if not handle.get("plan_agent"):
                raise OpenCodeError(
                    "OpenCode prompt schema has no agent field, so plan mode "
                    "cannot be selected",
                    code="opencode.plan_unsupported",
                )
            body["agent"] = "plan"
        if handle.get("variant") and handle.get("variant_supported"):
            body["variant"] = str(handle["variant"])
        model = handle.get("model")
        custom_provider = _custom_provider(handle)
        if model and custom_provider:
            body["model"] = {
                "providerID": custom_provider,
                "modelID": str(model),
            }
        elif model and "/" in str(model):
            provider, model_id = str(model).split("/", 1)
            body["model"] = {"providerID": provider, "modelID": model_id}
        return body

    def _begin_admission(self, handle: dict[str, Any], message_id: str) -> None:
        if handle.get("settlement_pending"):
            raise OpenCodeError(
                "OpenCode is still waiting for the previous operation to become idle",
                code="opencode.session_unsettled")
        generation = int(handle.get("admission_generation") or 0) + 1
        handle["admission_generation"] = generation
        handle["admission"] = _new_admission(message_id, generation)
        handle.setdefault("turn_user_ids", []).append(message_id)

    def _dispatch_sse(
        self, agent_session_id: str, event_name: str, data: dict[str, Any]
    ) -> None:
        """bus 事件 → turn 队列（在 SSE task 上运行，只 put_nowait）。"""
        handle = self._sessions.get(agent_session_id)
        if handle is None:
            return
        props = data.get("properties") if isinstance(
            data.get("properties"), dict) else data
        etype = str(data.get("type") or event_name or "")

        event_session_id = str(props.get("sessionID") or "")
        root_session_id = str(handle.get("external_session_id") or "")
        child_agent_id = (
            handle.get("child_sessions", {}).get(event_session_id) or ""
        )

        if etype in ("session.created", "session.updated"):
            info = props.get("info") if isinstance(props.get("info"), dict) else {}
            parent_id = str(info.get("parentID") or "")
            child_id = str(info.get("id") or "")
            children = handle.setdefault("child_sessions", {})
            if child_id and parent_id and (
                parent_id == root_session_id
                or parent_id in children
                or child_id in children
            ):
                self._register_child_session(handle, info)
            return
        if etype in {"session.compacted", "session.compaction.ended"}:
            if event_session_id and event_session_id != root_session_id:
                return
            self._context_compacted(agent_session_id)
            self._emit_warning(
                handle, "opencode.session.compacted",
                "OpenCode compacted the session context")
            return
        if etype == "todo.updated":
            if event_session_id and event_session_id != root_session_id:
                return
            self._emit_plan(handle, props.get("todos"))
            return
        if etype == "session.status":
            status = props.get("status") if isinstance(
                props.get("status"), dict) else {}
            status_type = str(status.get("type") or "")
            root_status = (
                not event_session_id or event_session_id == root_session_id
            )
            if status_type == "retry":
                message = str(status.get("message") or "").strip() \
                    or "OpenCode is retrying the model request"
                self._emit_warning(handle, "opencode.session.retry", message)
                if root_status:
                    self._note_root_status(handle, "busy")
                return
            if root_status:
                self._note_root_status(handle, status_type)
                return
        if etype == "session.idle" and (
            not event_session_id or event_session_id == root_session_id
        ):
            self._note_root_status(handle, "idle")
            return
        if etype == "session.error" and (
            not event_session_id or event_session_id == root_session_id
        ):
            error = props.get("error")
            self._signal_turn(
                handle, "__turn_failed__",
                (str(error.get("name") or "") if isinstance(error, dict) else "",
                 _response_error(error) or "OpenCode session error"))
            return
        if etype == "session.next.prompt.admitted" and (
            not event_session_id or event_session_id == root_session_id
        ):
            self._note_admission(handle, "user-message")
            return
        if event_session_id and event_session_id != root_session_id:
            if child_agent_id:
                self._dispatch_child_event(
                    handle, child_agent_id, event_session_id, etype, props)
            # Unknown non-root sessions never reach the parent's stream.
            return
        if etype in {"permission.asked", "permission.v2.asked"}:
            self._handle_permission(handle, props)
            return
        if etype == "question.asked":
            self._handle_question(handle, props)
            return

        sink = handle.get("event_sink")
        if sink is None:
            handle["sse_outside_turn"] = handle.get("sse_outside_turn", 0) + 1
            return

        if etype == "message.part.updated":
            part = props.get("part") if isinstance(
                props.get("part"), dict) else {}
            kind = str(part.get("type") or "")
            part_id = str(part.get("id") or "")
            message_id = str(part.get("messageID") or "")
            if part_id and kind:
                handle.setdefault("part_kinds", {})[part_id] = kind
            if kind == "reasoning":
                self._emit_reasoning(handle, part)
            elif kind == "text" and _part_text(part) and part_id and (
                    handle.get("message_roles", {}).get(message_id)
                    == "assistant"):
                # The part snapshot carries the whole text; deltas already
                # streamed most of it, so only the unseen suffix is new.
                handle.setdefault("turn_part_text", {})[part_id] = _part_text(part)
                delta = _reasoning_increment(
                    handle.setdefault("text_seen", {}), part_id, _part_text(part))
                if delta:
                    sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                     "opencode.message.part.updated",
                                     dump_payload(MessageDeltaPayload(
                                         text=delta,
                                         message_id=message_id or None,
                                         native={"part_id": part_id}))))
            elif kind == "tool":
                state = part.get("state") if isinstance(
                    part.get("state"), dict) else {}
                status = str(state.get("status") or "")
                tool_name = str(part.get("tool") or "")
                if tool_name == "question":
                    return
                call_id = str(part.get("callID") or part.get("id") or "")
                is_task_tool = tool_name == "task"
                if is_task_tool:
                    metadata = state.get("metadata") if isinstance(
                        state.get("metadata"), dict) else {}
                    child_id = str(metadata.get("sessionId") or "")
                    if child_id and call_id:
                        self._link_task_call(handle, call_id, child_id, state)
                    if status in ("completed", "error"):
                        self._complete_task_call(
                            handle, call_id, state, failed=status == "error")
                if status == "running":
                    # OpenCode re-sends the running part for each metadata update.
                    started = handle.setdefault("tool_started", set())
                    if call_id in started:
                        return
                    started.add(call_id)
                    sink.put_nowait((AgentEventType.TOOL_STARTED,
                                     "opencode.tool.running",
                                     dump_payload(ToolPayload(
                                         tool_call_id=call_id,
                                         name=tool_name or None,
                                         input=state.get("input"),
                                         status="running",
                                         kind=("agent" if is_task_tool
                                               else None),
                                     ))))
                elif status in ("completed", "error"):
                    # tool part 的 state.output 是真实命令输出的结构化
                    # 来源（gate witness 输入，§OPENCODE 影响）。
                    sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                                     "opencode.tool.completed",
                                     dump_payload(ToolPayload(
                                         tool_call_id=call_id,
                                         name=tool_name or None,
                                         output=state.get("output"),
                                         status=("failed" if status == "error"
                                                 else "completed"),
                                         error=(str(state.get("error"))
                                                if status == "error"
                                                and state.get("error")
                                                else None),
                                         kind=("agent" if is_task_tool
                                               else None),
                                     ))))
        elif etype == "message.part.delta":
            part_id = str(props.get("partID") or "")
            if str(props.get("field") or "") != "text" or not part_id:
                return
            delta = str(props.get("delta") or "")
            if not delta:
                return
            part_kind = handle.get("part_kinds", {}).get(part_id)
            if part_kind == "reasoning":
                seen = handle.setdefault("reasoning_text", {})
                seen[part_id] = seen.get(part_id, "") + delta
                self._queue_event(
                    handle, AgentEventType.REASONING_SUMMARY,
                    "opencode.reasoning.delta",
                    dump_payload(ReasoningPayload(
                        text=delta, channel="thinking", partial=True,
                        item_id=part_id)))
            elif part_kind == "text" and (
                    handle.get("message_roles", {}).get(
                        str(props.get("messageID") or "")) == "assistant"):
                seen = handle.setdefault("text_seen", {})
                seen[part_id] = seen.get(part_id, "") + delta
                handle.setdefault("turn_part_text", {})[part_id] = seen[part_id]
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "opencode.message.part.delta",
                                 dump_payload(MessageDeltaPayload(
                                     text=delta,
                                     message_id=(
                                         str(props.get("messageID") or "")
                                         or None),
                                     native={"part_id": part_id}))))
        elif etype == "message.updated":
            info = props.get("info") if isinstance(
                props.get("info"), dict) else {}
            role = str(info.get("role") or "")
            message_id = str(info.get("id") or "")
            if message_id and role:
                handle.setdefault("message_roles", {})[message_id] = role
            if role == "user":
                admission = handle.get("admission") or {}
                if admission.get("pending") and (
                    not admission.get("message_id")
                    or message_id == admission.get("message_id")
                ):
                    self._note_admission(handle, "user-message")
            tokens = _message_tokens(info)
            completed = isinstance(info.get("time"), dict) and (
                info["time"].get("completed") is not None)
            if role == "assistant" and completed:
                if message_id:
                    ids = handle.setdefault("turn_assistant_ids", [])
                    if message_id not in ids:
                        ids.append(message_id)
                handle["turn_assistant_info"] = info
                self._note_admission(handle, "assistant-completed")
            if tokens and role == "assistant":
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "opencode.message.updated",
                                 _opencode_usage_payload(info)))

    # -- 子智能体（task 工具 / child session）------------------------------------

    def _agent_patch(
        self, handle: dict[str, Any], agent_id: str, update: dict[str, Any]
    ) -> None:
        nodes = handle.setdefault("agent_nodes", {})
        previous = nodes.get(agent_id)
        if previous is None:
            previous = {"agent_id": agent_id, "title": agent_id,
                        "status": "running"}
        node = {**previous, **{k: v for k, v in update.items() if v is not None}}
        if node == previous and agent_id in nodes:
            return
        nodes[agent_id] = node
        sink = handle.get("event_sink")
        if sink is not None or handle.get("generation") == "v2":
            self._queue_event(handle, AgentEventType.AGENT_UPDATED,
                             "opencode.subagent.updated",
                             dump_payload(AgentUpdatedPayload(
                                 agents=[AgentNodePayload(**node)],
                                 patch=True)))

    def _register_child_session(
        self, handle: dict[str, Any], info: dict[str, Any]
    ) -> None:
        """session.created/updated：登记子会话并维护对应 Agent 节点。"""
        child_id = str(info.get("id") or "")
        if not child_id:
            return
        root_id = str(handle.get("external_session_id") or "")
        children = handle.setdefault("child_sessions", {})
        parent_session = str(info.get("parentID") or "")
        children.setdefault(child_id, child_id)
        if parent_session and parent_session == child_id:
            parent_session = ""
        model = info.get("model") if isinstance(info.get("model"), dict) else {}
        title = re.sub(r"\s*\(@\w+ subagent\)\s*$", "",
                       str(info.get("title") or "")).strip()
        existing = (handle.get("agent_nodes") or {}).get(child_id) or {}
        update: dict[str, Any] = {
            "session_ref": child_id,
            "parent_id": (
                None
                if not parent_session or parent_session == root_id
                else parent_session
            ),
            # task 工具描述比会话标题更准确，已有关联标题时保留。
            "title": None if existing.get("title") not in (None, "", child_id)
            else (title or None),
            "role": str(info.get("agent") or "").strip() or None,
            "model": str(model.get("id") or "").strip() or None,
        }
        # session.created 时子会话开始工作；之后的 updated 不降级状态。
        if child_id not in (handle.get("agent_nodes") or {}):
            update["status"] = "running"
        self._agent_patch(handle, child_id, update)
        self._schedule_child_permission(handle, child_id)

    def _link_task_call(
        self, handle: dict[str, Any], call_id: str, child_id: str,
        state: dict[str, Any],
    ) -> None:
        """task 工具 part 的 metadata.sessionId 把委派调用与子会话关联。"""
        children = handle.setdefault("child_sessions", {})
        children.setdefault(child_id, child_id)
        self._schedule_child_permission(handle, child_id)
        nodes = handle.get("agent_nodes") or {}
        if (nodes.get(child_id) or {}).get("call_id") == call_id:
            return
        input_args = state.get("input") if isinstance(state.get("input"), dict) else {}
        self._agent_patch(handle, child_id, {
            "call_id": call_id,
            "request": str(input_args.get("prompt") or "").strip() or None,
            "title": str(input_args.get("description") or "").strip() or None,
            "role": str(input_args.get("subagent_type") or "").strip() or None,
        })

    def _complete_task_call(
        self, handle: dict[str, Any], call_id: str, state: dict[str, Any],
        *, failed: bool,
    ) -> None:
        nodes = handle.get("agent_nodes") or {}
        child_id = next(
            (agent_id for agent_id, node in nodes.items()
             if node.get("call_id") == call_id),
            "",
        )
        if not child_id:
            return
        output = state.get("output")
        error = state.get("error")
        # task 输出是 <task …><task_result>…</task_result></task> 包装。
        result_text: Optional[str] = None
        if output is not None:
            text = str(output)
            match = re.search(
                r"<task_result>(.*?)</task_result>", text, re.DOTALL)
            result_text = (match.group(1) if match else text).strip() or None
        update: dict[str, Any] = {
            "status": "failed" if failed else "completed",
            "result": None if failed else result_text,
            "error": str(error or output) if failed else None,
        }
        time_range = state.get("time") if isinstance(state.get("time"), dict) else {}
        start, end = time_range.get("start"), time_range.get("end")
        if isinstance(start, (int, float)) and isinstance(end, (int, float)):
            update["duration_ms"] = max(0, int(end - start))
        self._agent_patch(handle, child_id, update)

    def _dispatch_child_event(
        self, handle: dict[str, Any], agent_id: str, session_id: str,
        etype: str, props: dict[str, Any],
    ) -> None:
        """子会话事件：工具/进展归属到对应 Agent，审批与提问照常转发。"""
        if etype in {"permission.asked", "permission.v2.asked"}:
            self._handle_permission(handle, props)
            return
        if etype == "question.asked":
            self._handle_question(handle, props)
            return
        sink = handle.get("event_sink")
        if sink is None:
            return
        if etype == "message.part.updated":
            part = props.get("part") if isinstance(props.get("part"), dict) else {}
            kind = str(part.get("type") or "")
            message = props.get("message") if isinstance(
                props.get("message"), dict) else {}
            role = str(part.get("role") or message.get("role")
                       or props.get("role") or "").lower()
            if kind == "reasoning":
                text = _part_text(part)
                if text:
                    self._emit_reasoning(handle, part)
                    self._agent_patch(handle, agent_id, {
                        "status": "running", "activity": text})
            elif kind == "text":
                # 子会话的 text part 不带 role；用 message.updated 里记录的
                # messageID → role 判定，避免把用户 prompt 当成子智能体活动。
                msg_role = (handle.setdefault("child_msg_roles", {})
                            .get(session_id, {})
                            .get(str(part.get("messageID") or "")))
                text = _part_text(part)
                part_id = str(part.get("id") or "")
                seen = handle.setdefault("child_text_parts", {})
                if msg_role == "assistant" and text and seen.get(part_id) != text:
                    seen[part_id] = text
                    self._agent_patch(handle, agent_id, {
                        "status": "running", "activity": text})
            elif kind == "tool":
                state = part.get("state") if isinstance(
                    part.get("state"), dict) else {}
                status = str(state.get("status") or "")
                call_id = str(part.get("callID") or part.get("id") or "")
                tool_name = str(part.get("tool") or "")
                if tool_name == "question":
                    return
                if status == "running":
                    started = handle.setdefault("tool_started", set())
                    if call_id in started:
                        return
                    started.add(call_id)
                    sink.put_nowait((AgentEventType.TOOL_STARTED,
                                     "opencode.tool.running",
                                     dump_payload(ToolPayload(
                                         tool_call_id=call_id,
                                         name=tool_name or None,
                                         input=state.get("input"),
                                         status="running",
                                         agent_id=agent_id,
                                     ))))
                elif status in ("completed", "error"):
                    sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                                     "opencode.tool.completed",
                                     dump_payload(ToolPayload(
                                         tool_call_id=call_id,
                                         name=tool_name or None,
                                         output=state.get("output"),
                                         status=("failed" if status == "error"
                                                 else "completed"),
                                         error=(str(state.get("error"))
                                                if status == "error"
                                                and state.get("error")
                                                else None),
                                         agent_id=agent_id,
                                     ))))
        elif etype == "message.updated":
            info = props.get("info") if isinstance(props.get("info"), dict) else {}
            role = str(info.get("role") or "")
            message_id = str(info.get("id") or "")
            if message_id and role:
                (handle.setdefault("child_msg_roles", {})
                 .setdefault(session_id, {})[message_id]) = role
            tokens = _message_tokens(info)
            if tokens and role == "assistant":
                # 每条消息只计最新累计值；节点总量 = 各消息之和。
                usage_by_msg = handle.setdefault("child_usage", {}).setdefault(
                    agent_id, {})
                usage_by_msg[message_id] = tokens
                total = 0
                for row in usage_by_msg.values():
                    if row.get("total"):
                        total += int(row.get("total") or 0)
                    else:
                        total += (
                            int(row.get("input") or 0)
                            + int(row.get("output") or 0)
                            + int(row.get("reasoning") or 0)
                        )
                self._agent_patch(handle, agent_id, {
                    "total_tokens": total or None,
                })
        elif etype in ("session.status", "session.idle"):
            status = props.get("status") if isinstance(
                props.get("status"), dict) else {}
            idle = etype == "session.idle" or str(status.get("type") or "") == "idle"
            if not idle:
                self._agent_patch(handle, agent_id, {"status": "running"})
                return
            node = (handle.get("agent_nodes") or {}).get(agent_id) or {}
            if node.get("status") in ("completed", "failed", "cancelled"):
                return
            blocked = any(
                str(p.get("sessionID") or "") == session_id
                for p in (handle.get("pending_approvals") or {}).values()
            ) or any(
                str(p.get("sessionID") or "") == session_id
                for p in (handle.get("pending_questions") or {}).values()
            )
            # 子会话进入 idle 即结束本轮工作；结果被父会话 task 工具带回。
            if not blocked:
                self._agent_patch(handle, agent_id, {"status": "completed"})

    def _permission_request(
        self, handle: dict[str, Any], props: dict[str, Any], *,
        request_id: str, agent_id: str,
    ) -> ApprovalRequestedPayload:
        """Structured approval request for one native ``permission.asked``."""
        permission = str(props.get("permission") or props.get("action") or "")
        raw_patterns = props.get("patterns") or props.get("resources") or []
        patterns = [str(p) for p in raw_patterns if str(p)] if isinstance(
            raw_patterns, list) else []
        metadata = props.get("metadata") if isinstance(
            props.get("metadata"), dict) else {}
        tool = props.get("tool") if isinstance(props.get("tool"), dict) else {}
        fields: dict[str, Any] = {}
        if permission == "bash":
            kind = "command_execution"
            command = str(metadata.get("command") or "").strip() or (
                " && ".join(patterns))
            if command:
                fields["command"] = command
        elif permission in _OPENCODE_EDIT_PERMISSIONS:
            kind = "file_change"
            paths = [str(metadata.get("filepath") or "")] if metadata.get(
                "filepath") else patterns
            fields["paths"] = [p for p in paths if p]
            if isinstance(metadata.get("diff"), str) and metadata["diff"]:
                fields["unified_diff"] = metadata["diff"]
        else:
            kind = "tool"
            fields["input"] = {"patterns": patterns}
        return ApprovalRequestedPayload(
            approval_id=request_id,
            agent_id=agent_id or None,
            approval_kind=kind,
            title=permission or "OpenCode operation",
            tool_name=permission or None,
            tool_call_id=str(tool.get("callID") or "") or None,
            cwd=str(handle.get("cwd") or "") or None,
            native={
                "permission": permission,
                "patterns": patterns,
                "always": props.get("always"),
                "metadata": metadata,
                "access_mode": str(handle.get("access_mode") or ""),
                **({"background": True} if props.get("background") else {}),
            },
            **fields,
        )

    def _handle_permission(
        self, handle: dict[str, Any], props: dict[str, Any]
    ) -> None:
        """Route an OpenCode native permission request to Conversation."""
        request_id = str(props.get("id") or props.get("requestID") or "")
        if not request_id:
            return
        sink = handle.get("event_sink")
        access_mode = str(handle.get("access_mode") or "")
        permission = str(
            props.get("permission") or props.get("action") or ""
        )
        session_id = str(props.get("sessionID") or "")
        agent_id = (handle.get("child_sessions") or {}).get(session_id) or ""
        if access_mode == AccessMode.FULL_ACCESS.value:
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        if (access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
                and permission in _OPENCODE_EDIT_PERMISSIONS):
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        payload = self._permission_request(
            handle, props, request_id=request_id, agent_id=agent_id)
        request = dump_payload(payload)
        cwd = str(handle.get("cwd") or "")
        grants: SessionApprovalGrants = handle["approval_grants"]
        if grants.covers(request, cwd=cwd):
            # OpenCode's own "always" covers a wider pattern than the exact
            # target the operator approved, so repeats are answered once here.
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        if sink is None and not (props.get("background") and handle.get("background_handler")):
            self._emit_warning(
                handle, "opencode.permission_outside_turn",
                f"OpenCode asked for {permission or 'a permission'} while no "
                "turn was running; the request stays pending in OpenCode")
            return
        remembered = ApprovalTarget.from_payload(request, cwd=cwd) is not None
        payload = payload.model_copy(update={
            "scopes": ["once", "session"] if remembered else ["once"]})
        handle["pending_approvals"][request_id] = dict(props)
        handle.setdefault("approval_requests", {})[request_id] = dump_payload(
            payload)
        self._queue_event(handle, AgentEventType.APPROVAL_REQUESTED,
                          "opencode.permission.asked", dump_payload(payload))

    async def _auto_reply_permission(
        self, handle: dict[str, Any], request_id: str, reply: str,
        *, session_id: str,
    ) -> None:
        try:
            await self._reply_permission(
                handle, request_id, reply, session_id=session_id)
        except OpenCodeError as exc:
            self._emit_warning(
                handle, "opencode.permission_reply_failed",
                f"OpenCode permission auto-reply failed: {exc}")

    @staticmethod
    async def _reply_permission(
        handle: dict[str, Any], request_id: str, reply: str,
        *, session_id: str = "", message: str = "",
    ) -> None:
        client: OpenCodeHttpClient = handle["client"]
        if handle.get("generation") == "v2":
            await client.post(
                f"/api/session/{session_id or handle['external_session_id']}/permission/{request_id}/reply",
                {"decision": reply, **({"message": message} if message else {})})
            return
        body: dict[str, Any] = {"reply": reply}
        if message:
            body["message"] = message
        try:
            await client.post(f"/permission/{request_id}/reply", body)
            return
        except OpenCodeError as exc:
            # The pre-1.x endpoint is only a fallback when the new one is absent.
            if exc.status != 404:
                raise
        if not session_id:
            session_id = str(
                (handle.get("pending_approvals", {}).get(request_id) or {})
                .get("sessionID") or "")
        if not session_id:
            session_id = str(handle.get("external_session_id") or "")
        await client.post(
            f"/session/{session_id}/permissions/{request_id}",
            {"response": reply},
        )

    def _handle_question(
        self, handle: dict[str, Any], props: dict[str, Any]
    ) -> None:
        """Expose OpenCode's native question request to Conversation."""
        request_id = str(props.get("id") or props.get("requestID") or "")
        if not request_id:
            return
        sink = handle.get("event_sink")
        if sink is None:
            self._emit_warning(
                handle, "opencode.question_outside_turn",
                "OpenCode asked a question while no turn was running; the "
                "request stays pending in OpenCode")
            return
        raw_questions = props.get("questions")
        raw_questions = raw_questions if isinstance(raw_questions, list) else []
        questions: list[dict[str, Any]] = []
        for index, raw in enumerate(raw_questions):
            if not isinstance(raw, dict):
                continue
            item = dict(raw)
            item["question_id"] = f"q{index}"
            # OpenCode lets the user type a custom answer unless disabled.
            item["allow_free_text"] = raw.get("custom") is not False
            normalized = normalize_question(item, index=index)
            if normalized is not None:
                questions.append(normalized)
        agent_id = (handle.get("child_sessions") or {}).get(
            str(props.get("sessionID") or "")) or ""
        handle["pending_questions"][request_id] = {
            **props, "normalized_questions": questions}
        sink.put_nowait((AgentEventType.USER_INPUT_REQUESTED,
                         "opencode.question.asked",
                         dump_payload(UserInputRequestedPayload(
                             request_id=request_id,
                             user_input_kind="opencode.question",
                             agent_id=agent_id or None,
                             questions=questions,
                             response_actions=["submit", "cancel"],
                             native={"props": props},
                         ))))

    # -- turn 流 --------------------------------------------------------------

    def bind_continuations(self, session, turn_id, updates, offer) -> None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is not None and handle.get("generation") == "v2":
            handle.update(conversation_turn_id=turn_id, background_handler=updates,
                          continuation_handler=offer)

    def pending_continuation(self, session) -> tuple[str, str] | None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is not None and handle.get("generation") == "v2":
            for wake in handle["wakes"]:
                if not wake["dropped"]:
                    return wake["id"], wake["detail"]
        return None

    def run_continuation(self, session, wake_id):
        return self._v2()._turn_stream(session, MessageInput(text=""), wake_id=wake_id)

    def background_turn_id(self, session) -> str | None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is None or handle.get("generation") != "v2":
            return None
        # T3 hasBackground / owesWork, including nested subagent follow-ups.
        if (handle["wakes"] or any(handle["reports"].values())
                or handle["busy"].intersection(handle["child_sessions"])
                or any(c["background"] and c["status"] == "running" for c in handle["calls"].values())):
            return handle.get("conversation_turn_id")
        return None

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return self._v2().send(session, input)
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response_stream(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._user_input_response_stream(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        try:
            decision = ApprovalDecision.from_payload(input.payload.model_dump())
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.invalid",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.VALIDATION, "approval.invalid",
                    message=f"Invalid approval response: {exc}")),
            ))
            return
        pending = (
            handle.get("pending_approvals", {}).get(decision.approval_id)
            if handle else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.stale",
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "approval.stale",
                    message="OpenCode approval is no longer pending")),
            ))
            return
        request = (handle.get("approval_requests") or {}).get(
            decision.approval_id) or {}
        cwd = str(handle.get("cwd") or "")
        # OpenCode's "always" approves a pattern wider than the shown target,
        # so a session grant is answered once and kept in the local ledger.
        sent = native_decision(
            request, decision, native_scope_exact=False, cwd=cwd)
        reply = "reject" if not sent.allowed else "once"
        handle.setdefault("answering_requests", set()).add(decision.approval_id)
        try:
            await self._reply_permission(
                handle, decision.approval_id, reply,
                session_id=str(pending.get("sessionID") or ""),
                message=decision.note if not decision.allowed else "")
        except OpenCodeError as exc:
            handle["answering_requests"].discard(decision.approval_id)
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.reply_failed",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.TRANSPORT, "approval.reply_failed",
                    message=f"OpenCode approval reply failed: {exc}")),
            ))
            return
        handle["pending_approvals"].pop(decision.approval_id, None)
        handle["answering_requests"].discard(decision.approval_id)
        (handle.get("approval_requests") or {}).pop(decision.approval_id, None)
        handle["approval_grants"].remember(request, decision, cwd=cwd)
        if not decision.allowed:
            if pending.get("background"):
                handle.setdefault("rejected_sessions", set()).add(pending.get("sessionID"))
            else:
                handle["turn_rejected"] = True
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            agent_session_id=sid,
            external_session_id=handle.get("external_session_id"),
            turn_id=handle.get("current_turn_id"),
            native_type="opencode.permission.replied",
            payload=ApprovalResolvedPayload(
                approval_id=decision.approval_id,
                decision="allow" if decision.allowed else "deny",
                scope=sent.scope.value,
                native={"reply": reply}),
        ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: UserInputResponseInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        request_id = input.payload.request_id
        pending = (
            handle.get("pending_questions", {}).get(request_id)
            if handle else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.question.stale",
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "user_input.stale",
                    message="OpenCode question is no longer pending")),
            ))
            return
        client: OpenCodeHttpClient = handle["client"]
        answered = input.payload.decision == "submit"
        questions = pending.get("normalized_questions") or []
        raw_answers: dict[str, Any] = dict(input.payload.answers)
        if answered and not raw_answers:
            raw_answers = expand_legacy_text_answers(
                {"questions": questions}, input.text)
        try:
            if answered:
                await client.post(
                    f"/question/{request_id}/reply",
                    {"answers": _question_answers(questions, raw_answers)})
            else:
                await client.post(f"/question/{request_id}/reject")
        except OpenCodeError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.question.reply_failed",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.TRANSPORT, "user_input.reply_failed",
                    message=f"OpenCode question reply failed: {exc}")),
            ))
            return
        handle["pending_questions"].pop(request_id, None)
        if not answered:
            handle["turn_rejected"] = True
        yield self.emit(build_event(
            AgentEventType.USER_INPUT_RESOLVED, seq,
            agent_session_id=sid,
            external_session_id=handle.get("external_session_id"),
            turn_id=handle.get("current_turn_id"),
            native_type=("opencode.question.replied" if answered
                         else "opencode.question.rejected"),
            payload=UserInputResolvedPayload(
                request_id=request_id,
                outcome="answered" if answered else "cancelled",
                answers=raw_answers if answered else None),
        ))

    async def _collect_turn_result(
        self, handle: dict[str, Any], client: OpenCodeHttpClient, session_id: str,
    ) -> dict[str, Any]:
        """Assistant text for this turn, from the session log when SSE has it."""
        info = dict(handle.get("turn_assistant_info") or {})
        user_ids = set(handle.get("turn_user_ids") or [])
        assistant_ids = set(handle.get("turn_assistant_ids") or [])
        texts: list[str] = []
        try:
            rows = await client.get(f"/session/{session_id}/message")
        except OpenCodeError:
            rows = []
        if isinstance(rows, list) and (user_ids or assistant_ids):
            for item in rows:
                if not isinstance(item, dict):
                    continue
                row_info = item.get("info") if isinstance(item.get("info"), dict) else {}
                if str(row_info.get("role") or "") != "assistant":
                    continue
                message_id = str(row_info.get("id") or "")
                parent_id = str(row_info.get("parentID") or "")
                if message_id not in assistant_ids and parent_id not in user_ids:
                    continue
                if row_info:
                    info = row_info
                for part in item.get("parts") or []:
                    if isinstance(part, dict) and part.get("type") == "text":
                        piece = _part_text(part)
                        if piece:
                            texts.append(piece)
        if not any(piece.strip() for piece in texts):
            texts = [
                str(value) for value in (handle.get("turn_part_text") or {}).values()
                if str(value).strip()
            ]
        return {
            "info": info,
            "parts": [{
                "type": "text",
                "role": "assistant",
                "text": "\n".join(texts),
            }],
        }

    async def _apply_turn_permissions(
        self, handle: dict[str, Any], *, plan: bool,
    ) -> None:
        """Install the session ruleset that matches this turn's interaction mode."""
        if bool(handle.get("permission_plan")) == plan:
            return
        rules = _permission_rules(
            str(handle["access_mode"]), plan=plan,
            child_guard=bool(handle.get("child_guard")))
        await handle["client"].patch(
            f"/session/{handle['external_session_id']}",
            {"permission": rules})
        handle["permissions"] = rules
        handle["permission_plan"] = plan

    def _turn_failed_event(
        self, seq: Any, common: dict[str, Any], external_id: Any, *,
        native_type: str, failure: Any, turn_id: Optional[str] = None,
    ) -> AgentEvent:
        return self.emit(build_event(
            AgentEventType.TURN_FAILED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type=native_type,
            payload=TurnFailedPayload(error=failure), **common))

    async def _turn_stream(
        self, session: AgentSessionRef, input: MessageInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        if handle is None or handle.get("client") is None:
            async for event in self._unsupported_stream(session, "send",
                                                        "session"):
                yield event
            return
        record = self._tracker.get(sid)
        external_id = handle.get("external_session_id") \
            or session.external_session_id
        common = dict(
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )
        if handle["turns"] == 0:
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED if handle.get("resumed")
                else AgentEventType.SESSION_STARTED, seq,
                external_session_id=external_id,
                native_type="opencode.session.start",
                payload=SessionPayload(
                    transport="http+sse",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=handle["cwd"],
                    native={
                        "mcp_injected": handle.get("mcp_injected", 0),
                        "warnings": list(handle.get("warnings") or []),
                    }),
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_CAPABILITIES_UPDATED, seq,
                external_session_id=external_id,
                native_type="opencode.runtime_capabilities",
                payload=RuntimeCapabilitiesPayload(
                    revision=int(handle.get("capability_revision", 1)),
                    reason="opencode.session.start",
                    native={
                        "adapter_id": self.id,
                        "commands": list(
                            handle.get("available_commands") or []),
                        "mcp_servers": list(
                            handle.get("mcp_statuses") or []),
                    }),
                **common))
        interaction_mode = str(input.payload.interaction_mode or "default")
        invocation = input.payload.runtime_capability.get("invocation")
        invocation = invocation if isinstance(invocation, dict) else {}
        if interaction_mode == "plan" and not handle.get("plan_agent"):
            yield self._turn_failed_event(
                seq, common, external_id,
                native_type="opencode.plan_unsupported",
                failure=self.failure(
                    FailureCategory.UNSUPPORTED, "opencode.plan_unsupported",
                    message=("OpenCode prompt schema has no agent field, so "
                             "interaction_mode=plan is refused"),
                    native_code="opencode.plan_unsupported"))
            handle["turns"] += 1
            return
        if (invocation.get("transport") != "opencode.command"
                and not handle.get("prompt_async")):
            yield self._turn_failed_event(
                seq, common, external_id,
                native_type="opencode.prompt_async_unsupported",
                failure=self.failure(
                    FailureCategory.UNSUPPORTED,
                    "opencode.prompt_async_unsupported",
                    message="OpenCode server has no prompt_async endpoint",
                    native_code="opencode.prompt_async_unsupported"))
            handle["turns"] += 1
            return
        try:
            sse_task = handle.get("sse_task")
            if sse_task is None or sse_task.done():
                # The previous subscription ended between turns. A fresh one
                # is awaited until server.connected before the prompt is sent.
                await self._close_event_stream(handle)
                await self._open_event_stream(sid, handle)
            await self._apply_turn_permissions(
                handle, plan=interaction_mode == "plan")
        except OpenCodeError as exc:
            yield self._turn_failed_event(
                seq, common, external_id,
                native_type="opencode.turn_setup_failed",
                failure=self.exception_failure(
                    exc,
                    FailureCategory.TRANSPORT if exc.code.startswith(
                        "opencode.sse") else FailureCategory.PROVIDER,
                    exc.code,
                    message=f"OpenCode turn setup failed: {exc}"))
            handle["turns"] += 1
            return
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        handle["turn_part_text"] = {}
        handle["turn_rejected"] = False
        handle["turn_assistant_ids"] = []
        handle["turn_assistant_info"] = {}
        handle["turn_user_ids"] = []
        handle["interaction_mode"] = interaction_mode
        handle["idle_before_admit"] = False
        handle.pop("abort_requested", None)
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="opencode.prompt.start",
            payload=TurnStartedPayload(kind=input.kind),
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue
        for deferred in handle.pop("deferred_events", []):
            queue.put_nowait(deferred)

        client: OpenCodeHttpClient = handle["client"]
        process = _as_process(handle.get("proc"))
        output = (self._server_outputs.get(process.pid)
                  if process is not None else None)

        async def abort_turn(_failure: Any) -> None:
            # The abort's session.error / idle events must be consumed by this
            # turn; left in flight they would end the next turn.
            handle["abort_requested"] = True
            settled = asyncio.Event()
            handle["settle_idle"] = settled
            try:
                await client.post(f"/session/{external_id}/abort")
                await self._wait_settled(handle, settled, "abort")
            finally:
                handle.pop("settle_idle", None)

        runner = TurnRunner(
            self, session, turn_id=turn_id,
            limits=TurnLimits(overall_s=self.conversation_turn_timeout(
                handle.get("conversation_thread_id"), self._prompt_timeout)),
            exit_watch=process.wait if process is not None else None,
            on_abort=abort_turn,
            diagnostics=output.read_all if output is not None else None,
            auto_ack=False,
            run_id=common["run_id"],
            execution_generation=common["execution_generation"],
        )
        try:
            async for event in runner.stream(self._raw_turn_events(
                    session, handle, input, runner=runner, queue=queue,
                    seq=seq, common=common, turn_id=turn_id,
                    external_id=external_id,
                    interaction_mode=interaction_mode,
                    invocation=invocation)):
                yield event
        finally:
            handle["event_sink"] = None
            handle["current_turn_id"] = None
            handle["admission"] = None
            handle["turns"] += 1

    async def _raw_turn_events(
        self, session: AgentSessionRef, handle: dict[str, Any],
        input: MessageInput, *, runner: TurnRunner, queue: asyncio.Queue,
        seq: Any, common: dict[str, Any], turn_id: str, external_id: Any,
        interaction_mode: str, invocation: dict[str, Any],
    ) -> AsyncIterator[AgentEvent]:
        """One turn's events. Timeouts and process exit belong to TurnRunner."""
        client: OpenCodeHttpClient = handle["client"]

        async def run_prompt() -> Any:
            try:
                runner.mark_sent()
                if invocation.get("transport") == "opencode.command":
                    command = str(invocation.get("command") or "").strip()
                    arguments = input.payload.runtime_command_arguments
                    if not command:
                        raise OpenCodeError(
                            "Runtime 命令缺少经过服务端解析的 command")
                    return await client.post(
                        f"/session/{external_id}/command",
                        {"command": command, "arguments": arguments},
                        timeout=self.conversation_turn_timeout(
                            handle.get("conversation_thread_id"),
                            self._prompt_timeout),
                    )
                message_id = _opencode_message_id()
                self._begin_admission(handle, message_id)
                await client.post(
                    f"/session/{external_id}/prompt_async",
                    self._prompt_body(
                        handle, input.text, message_id=message_id,
                        interaction_mode=interaction_mode),
                    timeout=30.0)
                runner.ack()
                return {"__async__": True}
            except Exception as exc:  # noqa: BLE001
                return {"__error__": exc}

        task: Optional[asyncio.Task] = asyncio.ensure_future(run_prompt())
        get_task = asyncio.ensure_future(queue.get())
        result: Any = None
        usage_seen = False  # SSE 已给出 usage 时不再用同步响应重复上报
        async_turn = False
        try:
            while True:
                sse_task = handle.get("sse_task")
                wait_set = {get_task}
                if task is not None:
                    wait_set.add(task)
                if sse_task is not None:
                    wait_set.add(sse_task)
                done, _pending = await asyncio.wait(
                    wait_set,
                    timeout=_IDLE_RECONCILE_S if async_turn else None,
                    return_when=asyncio.FIRST_COMPLETED)
                if not done:
                    if await self._session_is_idle(handle, client, external_id):
                        (handle.get("admission") or {})["pending"] = False
                        result = {"__idle__": True}
                        break
                    continue
                if task is not None and task in done:
                    admitted = task.result()
                    task = None
                    if isinstance(admitted, dict) and admitted.get("__async__"):
                        async_turn = True
                        self._note_admission(handle, "accepted")
                        admission = handle.get("admission") or {}
                        if handle.pop("idle_before_admit", False) and not admission.get("pending"):
                            result = {"__idle__": True}
                            break
                        continue
                    result = admitted
                    break
                if get_task in done:
                    etype, native_type, payload = get_task.result()
                    get_task = asyncio.ensure_future(queue.get())
                    runner.ack()
                    if etype == "__turn_idle__":
                        if not async_turn:
                            handle["idle_before_admit"] = True
                            continue
                        admission = handle.get("admission") or {}
                        if admission.get("pending"):
                            continue
                        result = {"__idle__": True}
                        break
                    if etype == "__turn_failed__":
                        name, message = payload
                        result = {"__error__": OpenCodeError(
                            message,
                            code=("opencode.aborted"
                                  if name == "MessageAbortedError"
                                  else "opencode.session_error"))}
                        break
                    if etype is AgentEventType.USAGE_UPDATED:
                        usage_seen = True
                    yield self.emit(build_event(
                        etype, seq,
                        external_session_id=external_id, turn_id=turn_id,
                        native_type=native_type, payload=payload,
                        **common))
                    continue
                if sse_task in done:
                    # Subscription lost: events after this point are unseen,
                    # so the turn cannot be completed truthfully.
                    error = (None if sse_task.cancelled()
                             else sse_task.exception())
                    process = _as_process(handle.get("proc"))
                    if process is not None:
                        try:
                            await asyncio.wait_for(process.wait(), 1.0)
                        except asyncio.TimeoutError:
                            pass
                        if process.returncode is not None:
                            # TurnRunner reports the exit with server output.
                            return
                    handle["abort_requested"] = True
                    abort_detail = ""
                    try:
                        await client.post(f"/session/{external_id}/abort")
                    except OpenCodeError as abort_exc:
                        abort_detail = f"\nabort failed: {abort_exc}"
                    code = (error.code if isinstance(error, OpenCodeError)
                            else "opencode.sse_closed")
                    yield self._turn_failed_event(
                        seq, common, external_id, turn_id=turn_id,
                        native_type="opencode.event_stream_lost",
                        failure=self.failure(
                            FailureCategory.TRANSPORT, code,
                            message="OpenCode event stream was lost mid-turn",
                            detail=f"{error}{abort_detail}",
                            native_code=code,
                            delivery_unknown=runner.delivery_unknown))
                    return
        finally:
            get_task.cancel()
            if task is not None:
                task.cancel()

        if isinstance(result, dict) and result.get("__idle__"):
            if handle.pop("abort_requested", False):
                result = {"__aborted__": True}
            else:
                result = await self._collect_turn_result(
                    handle, client, external_id)
        if isinstance(result, dict) and result.get("__aborted__"):
            yield self._turn_failed_event(
                seq, common, external_id, turn_id=turn_id,
                native_type="opencode.prompt.aborted",
                failure=self.failure(
                    FailureCategory.CANCELLED, "interrupted",
                    message="OpenCode turn aborted",
                    native_code="opencode.aborted"))
            return

        if isinstance(result, dict) and isinstance(
                result.get("__error__"), Exception):
            error = result["__error__"]
            code = error.code if isinstance(error, OpenCodeError) else ""
            interrupted = (handle.pop("abort_requested", False)
                           or code == "opencode.aborted")
            server_exit: dict[str, Any] = {}
            server_process = _as_process(handle.get("proc"))
            if server_process is not None and server_process.returncode is not None:
                server_exit = {
                    "returncode": server_process.returncode,
                    "output": self.server_output_detail(handle.get("proc")),
                }
            if code == "opencode.plan_unsupported":
                category = FailureCategory.UNSUPPORTED
                reason = code
            elif interrupted:
                category = FailureCategory.CANCELLED
                reason = "interrupted"
            else:
                category = FailureCategory.PROVIDER
                reason = code or "prompt_error"
            failure = self.failure(
                category, reason,
                message=f"OpenCode prompt failed: {error}",
                detail=str(error),
                native_code=code or type(error).__name__,
                delivery_unknown=runner.delivery_unknown)
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.prompt.error",
                payload=TurnFailedPayload(
                    error=failure,
                    native={"server_exit": server_exit} if server_exit else {}),
                **common))
            if server_exit:
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_EXITED, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="opencode.exit",
                    payload=RuntimeExitedPayload(
                        classification=classify_exit(
                            cancelled=interrupted, error=str(error),
                            returncode=server_exit.get("returncode")),
                        error=failure),
                    **common))
            return

        # turn 完成：响应 {info, parts}；正文 parts 里 text 合并为
        # MESSAGE_COMPLETED（若 SSE 已流式给出 delta，这里给最终权威文本）。
        # usage 优先取 SSE 已上报值；仅当 SSE 未提供时才用同步响应补报，
        # 且补报排在 MESSAGE_COMPLETED 之前，保持 TURN_COMPLETED 收尾。
        info = result.get("info") if isinstance(result, dict) else {}
        parts = (result.get("parts") if isinstance(result, dict) else None) \
            or []
        response_error = _response_error(
            (result or {}).get("error") if isinstance(result, dict) else None
        ) or _response_error(
            (info or {}).get("error") if isinstance(info, dict) else None
        )
        finish = str((info or {}).get("finish") or "").strip().lower()
        if response_error or finish in {"error", "failed"}:
            detail = response_error or f"OpenCode finish={finish}"
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.prompt.error_response",
                payload=TurnFailedPayload(error=self.failure(
                    FailureCategory.PROVIDER, "turn_failed",
                    message=detail)),
                **common))
            return
        tokens = _message_tokens(info or {})
        if tokens and not usage_seen:
            yield self.emit(build_event(
                AgentEventType.USAGE_UPDATED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.usage",
                payload=_opencode_usage_payload(info or {}),
                **common))
        text = "".join(
            _part_text(p) for p in parts
            if isinstance(p, dict) and p.get("type") == "text"
            and str(p.get("role") or "assistant").lower() == "assistant")
        if not text.strip():
            text = "\n".join(
                str(value) for value in (handle.get("turn_part_text") or {}).values()
                if str(value).strip()
            )
        if not text.strip() and handle.get("turn_rejected"):
            # OpenCode stops the loop when a permission is rejected without
            # guidance, so there is no assistant text to wait for.
            yield self._turn_failed_event(
                seq, common, external_id, turn_id=turn_id,
                native_type="opencode.permission.rejected",
                failure=self.failure(
                    FailureCategory.CANCELLED, "approval_denied",
                    message="OpenCode stopped the turn after a rejected permission"))
            return
        for event in self.completed_turn_events(
            seq, text=text,
            common={**common, "external_session_id": external_id, "turn_id": turn_id},
            native_type="opencode.prompt.completed",
            payload=TurnCompletedPayload(
                stop_reason=str((info or {}).get("finish") or "stop"))):
            yield event

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return await self._v2().operation(session, name, arguments)
        handle = self._sessions.get(session.agent_session_id)
        if not handle or handle.get("current_turn_id"):
            raise RuntimeError("请等待当前回复结束后再压缩")
        name = name.strip().lstrip("/")
        if arguments:
            raise ValueError(f"/{name} 不接受参数")
        paths = {"mcp": "/mcp", "agents": "/agent", "status": f"/session/{handle['external_session_id']}"}
        if name in paths:
            return {"status": "completed", "result": await handle["client"].get(paths[name])}
        if name not in {"compact", "summarize"}:
            raise RuntimeError("未知原生操作")
        model = str(handle.get("model") or "")
        provider = _custom_provider(handle)
        if not provider and "/" in model:
            provider, model = model.split("/", 1)
        if not provider or not model:
            raise RuntimeError("当前模型未提供 OpenCode 压缩所需的 provider/model")
        # The POST can return before the compaction's own status/idle events
        # reach the stream; a stale idle would end the next turn early.
        settle_idle = asyncio.Event()
        handle["settle_idle"] = settle_idle
        try:
            result = await handle["client"].post(
                f"/session/{handle['external_session_id']}/summarize",
                {"providerID": provider, "modelID": model}, timeout=180,
            )
            if result is not True:
                raise RuntimeError("OpenCode 未确认压缩完成")
            await self._wait_settled(handle, settle_idle, "compaction")
        finally:
            handle.pop("settle_idle", None)
        return {"status": "completed", "message": "上下文已由 OpenCode 原生压缩"}

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        if session is not None and (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return await self._v2().snapshot(session)
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            base.stale = True
            base.diagnostics.append(
                "OpenCode 命令目录属于具体 server/session，当前没有活动会话"
            )
            return base
        handle = self._sessions.get(session.agent_session_id)
        if handle is None or handle.get("client") is None:
            base.stale = True
            base.diagnostics.append("OpenCode 活动会话已结束")
            return base

        client: OpenCodeHttpClient = handle["client"]
        try:
            commands = _opencode_commands(await client.get("/command"))
            handle["available_commands"] = commands
        except OpenCodeError as exc:
            commands = list(handle.get("available_commands") or [])
            base.stale = True
            base.diagnostics.append(
                f"刷新 OpenCode 命令目录失败：{exc}"
            )
        try:
            mcp_statuses = _opencode_mcp_statuses(await client.get("/mcp"))
            handle["mcp_statuses"] = mcp_statuses
        except OpenCodeError as exc:
            mcp_statuses = list(handle.get("mcp_statuses") or [])
            base.diagnostics.append(
                f"刷新 OpenCode MCP 状态失败：{exc}"
            )

        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items: list[RuntimeCapabilityItem] = []
        model = str(handle.get("model") or "")
        provider = _custom_provider(handle)
        if model and (provider or "/" in model):
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:operation:compact", kind="operation", name="compact",
                description="由 OpenCode 原生压缩当前上下文", source=self.id, scope="session",
                engine="opencode", channel="app_server_rpc", resolution="client",
                origin="verified_static", delivery="guaranteed", verification="verified",
                action="invoke-runtime-operation", invocation={"method": "session.summarize"},
            ))
        from .command_providers import operation_item
        for name, method, description in (("mcp", "mcp.status", "查看 OpenCode MCP 连接状态"),
                                          ("agents", "agent.list", "查看 OpenCode 可用 Agent"),
                                          ("status", "session.get", "查看 OpenCode 会话状态")):
            items.append(operation_item(self.id, "opencode", name, method, description))
        compact = next((item for item in items if item.name == "compact"), None)
        if compact:
            items.append(compact.model_copy(update={"id": f"runtime:{self.id}:operation:summarize", "name": "summarize"}))
        operation_names = {item.name for item in items}
        for command in commands:
            name = str(command.get("name") or "").strip().lstrip("/")
            if not name or name in operation_names:
                continue
            description = str(
                command.get("description") or command.get("template") or ""
            ).strip()
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="opencode",
                name=name,
                description=description,
                argument_hint=str(
                    command.get("arguments") or command.get("argumentHint") or ""
                ),
                channel="app_server_rpc",
                kind="skill" if str(command.get("source") or "").lower()
                == "skill" else "command",
                invocation={
                    "transport": "opencode.command",
                    "command": name,
                    "wire_text": f"/{name}",
                },
            ))
        runtime_mcp = {str(item.get("name")): item for item in mcp_statuses}
        for item in base.items:
            if item.kind == "mcp_status" and item.name in runtime_mcp:
                state = runtime_mcp[item.name]
                item.status = str(state.get("status") or "runtime_reported")
                item.origin = "dynamic"
                item.verification = "verified"
                item.delivery = "guaranteed"
        known_mcp = {item.name for item in base.items if item.kind == "mcp_status"}
        for name, state in runtime_mcp.items():
            if name in known_mcp:
                continue
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:mcp:{name}",
                kind="mcp_status",
                name=name,
                description=str(state.get("error") or ""),
                source=self.id,
                scope="session",
                engine="opencode",
                channel="session_config",
                origin="dynamic",
                delivery="guaranteed",
                verification="verified",
                action="inspect-runtime-status",
                status=str(state.get("status") or "runtime_reported"),
                invocation={"runtime": state},
            ))
        base.items = items + base.items
        base.revision = int(handle["capability_revision"])
        base.external_session_id = handle.get("external_session_id")
        return base

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return self._v2().resume(session)
        return self._resume_stream(session)

    async def _resume_stream(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        """恢复：session id 重新采用在 ``_launch`` 完成；这里透出状态。

        历史消息可经 ``GET /session/:id/message`` 由调用方另行检阅；
        本流不把历史当作新事件投影（与 ACP replay 语义一致）。
        """
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        if handle is None:
            async for event in self._unsupported_stream(session, "resume",
                                                        "resume"):
                yield event
            return
        record = self._tracker.get(sid)
        yield self.emit(build_event(
            AgentEventType.SESSION_RESUMED, seq,
            external_session_id=handle.get("external_session_id"),
            native_type="opencode.session.resumed",
            payload=SessionPayload(
                transport="http+sse", cwd=handle["cwd"]),
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        ))

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return await self._v2().interrupt(session)
        self._mark_turn_interrupted(session.agent_session_id)
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if client is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        if not (handle or {}).get("abort_supported"):
            return self._typed_receipt(
                session, "opencode.abort_unsupported",
                "OpenCode server has no abort endpoint; the server process "
                "was left running",
                operation="interrupt")
        # Record the operator's abort intent before awaiting the HTTP response.
        # Abort stops the session loop. It does not stop the server process.
        handle["abort_requested"] = True
        try:
            await client.post(f"/session/{external_id}/abort")
        except OpenCodeError as exc:
            handle.pop("abort_requested", None)
            return self._typed_receipt(
                session, "opencode.abort_failed",
                f"OpenCode abort failed: {exc}",
                operation="interrupt",
                category=ErrorCategory.RUNTIME)
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return await self._v2().steer(session, input)
        """prompt_async into the running busy span. Same Muteki turn."""
        handle = self._sessions.get(session.agent_session_id)
        if not isinstance(input, (MessageInput, SteerInput)):
            return self._typed_receipt(
                session, "opencode.steer_unsupported",
                f"OpenCode steer cannot deliver {input.kind!r}",
                operation="steer")
        if handle is None or not handle.get("current_turn_id") or handle.get("client") is None:
            return self._typed_receipt(
                session, "opencode.steer_no_active_turn",
                "OpenCode has no running turn to steer",
                operation="steer")
        if not handle.get("prompt_async"):
            return self._typed_receipt(
                session, "opencode.steer_unsupported",
                "OpenCode server has no prompt_async endpoint",
                operation="steer")
        # expected_turn_id is the Conversation turn id. The running OpenCode
        # span is handle["current_turn_id"], a different id space.
        text = input.text
        if not str(text or "").strip():
            return self._typed_receipt(
                session, "opencode.steer_empty",
                "OpenCode steer requires text",
                operation="steer",
                category=ErrorCategory.VALIDATION)
        external_id = handle.get("external_session_id") or session.external_session_id
        message_id = _opencode_message_id()
        self._begin_admission(handle, message_id)
        try:
            await handle["client"].post(
                f"/session/{external_id}/prompt_async",
                self._prompt_body(
                    handle, text, message_id=message_id,
                    interaction_mode=str(handle.get("interaction_mode") or "default")),
                timeout=30.0)
        except OpenCodeError as exc:
            admission = handle.get("admission") or {}
            if admission.get("message_id") == message_id:
                admission["pending"] = False
            return self._typed_receipt(
                session, exc.code or "opencode.steer_rejected",
                f"OpenCode steer was not admitted: {exc}",
                operation="steer",
                category=ErrorCategory.RUNTIME)
        self._note_admission(handle, "accepted")
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        if (self._sessions.get(session.agent_session_id) or {}).get("generation") == "v2":
            return await self._v2().teardown(session)
        handle = self._sessions.get(session.agent_session_id)
        if not handle:
            return "closed"
        await self._close_event_stream(handle)
        background = list(handle.get("background_tasks") or ())
        for task in background:
            task.cancel()
        if background:
            await asyncio.gather(*background, return_exceptions=True)
        client = handle.get("client")
        if client is not None:
            await client.close()
        returncode = await self._stop_server(handle.get("proc"))
        self._sessions.pop(session.agent_session_id, None)
        return classify_exit(
            returncode=returncode if handle.get("proc") is not None else None,
            cancelled=handle.get("current_turn_id") is not None,
            resume_handle=handle.get("external_session_id")
            if handle.get("proc") is None else None,
        )


__all__ = [
    "OpenCodeError",
    "OpenCodeHttpClient",
    "OpenCodeServerAdapter",
]
