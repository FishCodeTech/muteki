"""ConversationManager：Conversation 领域业务逻辑（CONV-01，任务书 9.1）。

职责：

- Project / Workspace / Thread 的创建与恢复（对象本体存 PlatformStore）；
- Thread 的 CapabilityBinding 生命周期：创建时按模式签发（CAP-01
  ``CapabilityBindingService``）、归档时撤销（联动撤销全部 Grant）；
- Turn 的幂等创建与状态查询（命令层幂等之外，Turn 级
  ``(thread_id, idempotency_key)`` 唯一约束兜底崩溃重放）；
- Artifact 附着（内容寻址，内容本体写 conversation_artifacts/）；
- Thread fork（复制模式 / Runtime 选择 / Workspace 绑定，签发新 Binding，
  可从历史 Turn 派生新 Task）；
- 长期记忆写入 ``memory.timeline.v1``：只在用户显式允许（consent=True）
  时落一条带来源的记忆事件（任务书 8.3）；Executor 不自动写记忆。

本 Manager 不直接对接 Runtime——执行生命周期在
``executor.ExternalAgentSessionExecutor``；API route 也不直接调本 Manager
（一律经 MutekiCommandAPI dispatch）。
"""

from __future__ import annotations

import base64
import hashlib
from pathlib import Path
from typing import Any, Optional

from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.contracts.base import new_id, utcnow
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES,
    AccessMode,
)
from muteki.platform.contracts.capabilities import (
    CapabilityBinding,
    CapabilityGrant,
    ThreadMode,
)
from muteki.platform.contracts.graphs import GraphEvent, GraphScope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.objects import (
    AgentSession,
    Artifact,
    Project,
    Task,
    Thread,
    Workspace,
)
from muteki.platform.store import PlatformStore
from muteki.external_agents.factory import engine_for_adapter
from muteki.external_agents.interaction_matrix import build_matrix_from_probe
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.external_agents.c24_cu_fixtures import (
    apply_cu_fixture_to_runtime_connection,
)
from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_id_from_credential_id,
    account_store_root,
    canonical_credential_id,
    detect_system_login,
    engine_from_system_credential_id,
    system_credential_id,
)
from muteki.solver.engine_registry import (
    EngineTemporarilyUnsupportedError,
    ensure_engine_supported,
)

from .impact import build_impact_preview, resolve_rewind_capability
from .models import (
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_KIND_EDIT_RESEND,
    TURN_KIND_RETRY,
    TURN_SUPERSEDED,
    ConversationMessage,
    ThreadRuntimeSelection,
    TurnRecord,
    TurnRunRef,
)
from .projections import ConversationProjection
from .store import ConversationStore

#: Workspace 绑定形态。
WORKSPACE_LOCAL = "local"        # 本地已有目录
WORKSPACE_GIT = "git"            # 现有 Git 仓库 / worktree（目录内含 .git）
WORKSPACE_ISOLATED = "isolated"  # 独立工作区（Muteki 创建目录）

#: C06 default / hard caps for Thread body message pages.
DEFAULT_MESSAGES_PAGE_LIMIT = 50
MAX_MESSAGES_PAGE_LIMIT = 500
FULL_MESSAGES_HARD_CAP = 5000
#: Recent event window for artifact collection on slim views (not full history).
SLIM_EVENT_SCAN_LIMIT = 500
TURN_PROCESS_EVENT_LIMIT = 2000


def _parse_messages_limit(value: Any) -> int:
    """Normalize messages_limit query values.

    ``None`` / omitted → default page size.
    ``0`` / ``\"all\"`` → hard-capped full export for tests/admin.
    """
    if value is None or value == "":
        return DEFAULT_MESSAGES_PAGE_LIMIT
    if isinstance(value, str) and value.strip().lower() in {"all", "full"}:
        return FULL_MESSAGES_HARD_CAP
    try:
        parsed = int(value)
    except (TypeError, ValueError) as exc:
        raise ConversationError(
            f"invalid messages_limit: {value!r}"
        ) from exc
    if parsed == 0:
        return FULL_MESSAGES_HARD_CAP
    if parsed < 0:
        raise ConversationError("messages_limit must be >= 0")
    return min(parsed, FULL_MESSAGES_HARD_CAP)


def _messages_page_meta(page: dict[str, Any]) -> dict[str, Any]:
    meta = dict(page.get("messages_page") or {})
    return meta


def _usage_token_coverage(usage: dict[str, Any] | None) -> str:
    """Derive session token consumption coverage from ledger totals / legacy payloads."""
    usage = dict(usage or {})
    coverage = usage.get("token_coverage")
    if coverage in {"missing", "partial", "complete"}:
        return str(coverage)

    observed = usage.get("observed")
    if isinstance(observed, dict):
        inp_obs = int(observed.get("input_tokens") or 0)
        out_obs = int(observed.get("output_tokens") or 0)
        if inp_obs == 0 and out_obs == 0:
            return "missing"
        if inp_obs == 0 or out_obs == 0:
            return "partial"
        quality = usage.get("quality") if isinstance(usage.get("quality"), dict) else {}
        if int(quality.get("missing") or 0) or int(quality.get("partial") or 0):
            return "partial"
        return "complete"

    quality = usage.get("quality") if isinstance(usage.get("quality"), dict) else {}
    records = int(usage.get("records") or 0)
    # Legacy aggregates coerced missing fields to 0 while retaining quality counts.
    if records and int(quality.get("missing") or 0) == records and not (
        int(quality.get("reported") or 0)
        or int(quality.get("estimated") or 0)
        or int(quality.get("partial") or 0)
    ):
        return "missing"

    def _has(*keys: str) -> bool:
        for key in keys:
            value = usage.get(key)
            if value is None or isinstance(value, bool):
                continue
            try:
                if float(value) >= 0:
                    return True
            except (TypeError, ValueError):
                continue
        return False

    has_in = _has("input_tokens", "inputTokens", "prompt_tokens", "promptTokens", "input")
    has_out = _has("output_tokens", "outputTokens", "completion_tokens", "completionTokens", "output")
    if not has_in and not has_out:
        return "missing"
    if not has_in or not has_out:
        return "partial"
    if int(quality.get("missing") or 0) or int(quality.get("partial") or 0):
        return "partial"
    return "complete"


def _lightweight_statistics(
    turns: list[TurnRecord],
    usage: dict[str, Any],
) -> dict[str, Any]:
    """Stats without a full event replay — turn count + ledger usage only."""

    def number(source: dict[str, Any], *keys: str) -> Optional[float]:
        for key in keys:
            value = source.get(key)
            if value is None or isinstance(value, bool):
                continue
            try:
                parsed = float(value)
            except (TypeError, ValueError):
                continue
            if parsed >= 0:
                return parsed
        return None

    usage = dict(usage or {})
    result: dict[str, Any] = {
        "turn_count": len(turns),
        "step_count": 0,
    }
    input_tokens = number(
        usage, "input_tokens", "inputTokens", "prompt_tokens",
        "promptTokens", "input",
    )
    output_tokens = number(
        usage, "output_tokens", "outputTokens", "completion_tokens",
        "completionTokens", "output",
    )
    coverage = _usage_token_coverage(usage)
    # Missing coverage must not publish coerced zeros as definitive consumption.
    if coverage != "missing":
        if input_tokens is not None:
            result["input_tokens"] = round(input_tokens)
        if output_tokens is not None:
            result["output_tokens"] = round(output_tokens)
    result["token_coverage"] = coverage
    return result

#: Thread 工作区模式（存于 Workspace.settings["mode"]）。
WS_MODE_SHARED = "shared_checkout"
WS_MODE_EXISTING = "existing_worktree"
WS_MODE_NEW = "new_worktree"


def _conversation_statistics(
    events: list[Any],
    turns: list[TurnRecord],
    usage: dict[str, Any],
) -> dict[str, Any]:
    """把 Thread 的公开事件聚合为输入框下方的会话统计。

    统计只使用 Runtime 已上报的 token 与事件时间。执行器没有提供的
    LLM 耗时或吞吐量保持为空，前端会省略对应字段。
    """

    def number(source: dict[str, Any], *keys: str) -> Optional[float]:
        for key in keys:
            value = source.get(key)
            if value is None or isinstance(value, bool):
                continue
            try:
                parsed = float(value)
            except (TypeError, ValueError):
                continue
            if parsed >= 0:
                return parsed
        return None

    turn_started: dict[str, Any] = {}
    turn_finished: dict[str, Any] = {}
    step_started: dict[str, Any] = {}
    step_has_token: dict[str, bool] = {}
    ttft_samples: list[float] = []
    reported_turn_duration: dict[str, float] = {}
    usage_samples: dict[str, int] = {}
    latest_turn_usage: dict[str, dict[str, Any]] = {}
    tool_started: dict[tuple[str, str], Any] = {}
    tool_finished: dict[tuple[str, str], tuple[Any, float | None]] = {}
    tool_duration_ms = 0.0

    for event in events:
        payload = dict(event.payload or {})
        turn_id = str(payload.get("turn_id") or "")
        event_type = event.event_type

        if event_type == "core.turn.started" and turn_id:
            turn_started.setdefault(turn_id, event.occurred_at)
            step_started[turn_id] = event.occurred_at
            step_has_token[turn_id] = False
        elif event_type in (
            "core.turn.completed",
            "core.turn.failed",
            "core.turn.interrupted",
        ) and turn_id:
            turn_finished[turn_id] = event.occurred_at
            duration = number(payload, "duration_ms", "elapsed_ms")
            if duration is not None:
                reported_turn_duration[turn_id] = duration
        elif (
            event_type in ("core.message.delta", "core.message.completed")
            and turn_id
            and payload.get("thinking") is not True
            and str(payload.get("text") or "")
        ):
            started = step_started.get(turn_id)
            if started is not None and not step_has_token.get(turn_id, False):
                ttft_samples.append(max(
                    0.0, (event.occurred_at - started).total_seconds() * 1000,
                ))
                step_has_token[turn_id] = True
        elif event_type == "core.usage.updated":
            sample = payload.get("usage")
            normalized = dict(sample) if isinstance(sample, dict) else payload
            if turn_id:
                usage_samples[turn_id] = usage_samples.get(turn_id, 0) + 1
                latest_turn_usage[turn_id] = normalized
        elif event_type == "core.tool.started":
            call_id = str(payload.get("call_id") or "")
            if call_id:
                # Keep the first valid start for (turn_id, call_id). Progress
                # mis-mapped as started / reconnect replay must not move it (#218).
                key = (turn_id, call_id)
                previous_start = tool_started.get(key)
                if previous_start is None or event.occurred_at < previous_start:
                    tool_started[key] = event.occurred_at
        elif event_type == "core.tool.progress":
            # Output/progress chunks do not affect lifecycle timing.
            pass
        elif event_type == "core.tool.completed":
            call_id = str(payload.get("call_id") or "")
            key = (turn_id, call_id) if call_id else None
            reported = number(payload, "duration_ms", "elapsed_ms", "tool_duration_ms")
            previous_end = tool_finished.get(key) if key else None
            if key and (previous_end is None or event.occurred_at < previous_end[0]):
                tool_finished[key] = (event.occurred_at, reported)
            elif key and previous_end is not None and previous_end[1] is None and reported is not None:
                # A replay can add native duration, but never add a second sample.
                tool_finished[key] = (previous_end[0], reported)
            if turn_id and previous_end is None:
                # 工具结果交回后会开始下一次模型生成；以最后一个完成的工具
                # 为该 step 的可观测起点。
                step_started[turn_id] = event.occurred_at
                step_has_token[turn_id] = False

    # Aggregate after collecting lifecycle endpoints so out-of-order starts and
    # repeated completed events yield exactly one duration per logical call.
    tool_intervals: dict[str, list[tuple[Any, Any]]] = {}
    timing_sources: set[str] = set()
    for key, (finished, reported) in tool_finished.items():
        started = tool_started.get(key)
        if reported is not None:
            tool_duration_ms += reported
            timing_sources.add("reported")
        elif started is not None:
            tool_duration_ms += max(0.0, (finished - started).total_seconds() * 1000)
            timing_sources.add("lifecycle")
        if started is not None and finished >= started:
            lower = max(started, turn_started.get(key[0], started))
            upper = min(finished, turn_finished.get(key[0], finished))
            if upper >= lower:
                tool_intervals.setdefault(key[0], []).append((lower, upper))
    # Wall occupancy is a per-turn union, not the sum of overlapping calls.
    tool_wall_ms = 0.0
    for intervals in tool_intervals.values():
        merged: list[tuple[Any, Any]] = []
        for lower, upper in sorted(intervals):
            if merged and lower <= merged[-1][1]:
                merged[-1] = (merged[-1][0], max(merged[-1][1], upper))
            else:
                merged.append((lower, upper))
        tool_wall_ms += sum((upper - lower).total_seconds() * 1000 for lower, upper in merged)

    step_count = 0
    for turn in turns:
        turn_usage = latest_turn_usage.get(turn.turn_id, {})
        reported_steps = number(
            turn_usage, "step_count", "steps", "num_turns", "numTurns",
        )
        step_count += int(
            reported_steps
            if reported_steps is not None
            else usage_samples.get(turn.turn_id, 0)
        )

    total_duration_ms = 0.0
    for turn in turns:
        turn_id = turn.turn_id
        started = turn_started.get(turn_id)
        finished = turn_finished.get(turn_id)
        reported = reported_turn_duration.get(turn_id)
        if reported is not None:
            total_duration_ms += reported
        elif started is not None and finished is not None:
            total_duration_ms += max(
                0.0, (finished - started).total_seconds() * 1000,
            )

    usage = dict(usage or {})
    input_tokens = number(
        usage, "input_tokens", "inputTokens", "prompt_tokens",
        "promptTokens", "input",
    )
    output_tokens = number(
        usage, "output_tokens", "outputTokens", "completion_tokens",
        "completionTokens", "output",
    )
    codex_cache = number(usage, "cached_input_tokens", "cachedInputTokens")
    cache_read = codex_cache
    if cache_read is None:
        cache_read = number(
            usage, "cache_read_input_tokens", "cacheReadInputTokens",
            "cache_read_tokens", "cacheReadTokens", "cache_read",
        )
    cache_write = number(
        usage, "cache_creation_input_tokens", "cacheCreationInputTokens",
        "cache_write_tokens", "cacheWriteTokens", "cache_write",
    )
    billed_input = input_tokens
    # Codex 的 input_tokens 已包含 cached_input_tokens；其他 Runtime 单独上报的
    # cache read/write 字段需要计入计费输入。
    if codex_cache is None and input_tokens is not None:
        billed_input = input_tokens + (cache_read or 0) + (cache_write or 0)

    llm_duration_ms = 0.0
    has_llm_duration = False
    for turn_usage in latest_turn_usage.values():
        duration = number(
            turn_usage, "llm_duration_ms", "llmDurationMs",
            "model_duration_ms", "modelDurationMs",
        )
        if duration is not None:
            llm_duration_ms += duration
            has_llm_duration = True

    tokens_per_second = number(
        usage, "tokens_per_second", "tokensPerSecond", "tokens_per_s",
        "output_tokens_per_second", "throughput",
    )
    if tokens_per_second is None:
        samples = [
            value
            for turn_usage in latest_turn_usage.values()
            if (value := number(
                turn_usage, "tokens_per_second", "tokensPerSecond",
                "tokens_per_s", "output_tokens_per_second", "throughput",
            )) is not None
        ]
        if samples:
            tokens_per_second = sum(samples) / len(samples)

    result: dict[str, Any] = {
        "turn_count": len(turns),
        "step_count": step_count,
    }
    if total_duration_ms > 0:
        result["total_duration_ms"] = round(total_duration_ms)
    if has_llm_duration:
        result["llm_duration_ms"] = round(llm_duration_ms)
        result["llm_duration_source"] = "reported"
    elif total_duration_ms > 0:
        # 没有独立模型计时的 Runtime 仍能提供 Turn 总耗时和工具墙钟时间；
        # 两者之差是当前协议下可核查的模型阶段墙钟时间。
        result["llm_duration_ms"] = round(max(
            0.0, total_duration_ms - tool_wall_ms,
        ))
        result["llm_duration_source"] = "residual_estimate"
    if tool_duration_ms > 0:
        result["tool_duration_ms"] = round(tool_duration_ms)
        result["tool_duration_source"] = (
            next(iter(timing_sources)) if len(timing_sources) == 1 else "mixed"
        )
        result["tool_wall_duration_ms"] = round(tool_wall_ms)
    if ttft_samples:
        result["average_ttft_ms"] = round(sum(ttft_samples) / len(ttft_samples))
    if tokens_per_second is not None:
        result["tokens_per_second"] = round(tokens_per_second, 1)
    coverage = _usage_token_coverage(usage)
    if coverage != "missing":
        if billed_input is not None:
            result["input_tokens"] = round(billed_input)
        if output_tokens is not None:
            result["output_tokens"] = round(output_tokens)
        if billed_input and cache_read is not None:
            result["cache_hit_rate"] = round(
                min(1.0, max(0.0, cache_read / billed_input)), 4,
            )
    result["token_coverage"] = coverage
    return result


class ConversationError(ValueError):
    """Conversation 输入 / 状态错误（未知对象、模式非法等）。"""


class ConversationManager:
    """Conversation 领域的业务入口（Handler / Executor 共用）。"""

    def __init__(
        self,
        store: PlatformStore,
        conv: ConversationStore,
        binding_service: CapabilityBindingService,
        projection: ConversationProjection,
        *,
        memory_graph: Any = None,
        workspace_root: str | Path | None = None,
        adapter_registry: Any = None,
        sessions_root: str | Path | None = None,
    ) -> None:
        self._store = store
        self._conv = conv
        self._bindings = binding_service
        self._projection = projection
        # memory.timeline.v1 GraphService；缺省 None 时不写任何长期记忆。
        self._memory_graph = memory_graph
        self._adapter_registry = adapter_registry
        # Web 装配完成后注入；用于解析已启用的 Muteki 插件引用。
        self.extension_service: Any = None
        self._workspace_root = Path(
            workspace_root or Path(store.db_path).parent / "workspaces"
        ).expanduser().resolve()
        self._sessions_root = Path(
            sessions_root or Path(store.db_path).parent.parent
        ).expanduser().resolve()
        self._capability_snapshot_provider: Any = None
        self._capability_cache_writer: Any = None
        self._capability_refresh_trigger: Any = None
        self._capability_failure_provider: Any = None
        self._model_effort_validator: Any = None
        # C22 fixture captures: thread_id -> active pending + last capture
        self._user_input_fixtures: dict[str, dict[str, Any]] = {}
        self._user_input_fixture_captures: dict[str, dict[str, Any]] = {}

    def bind_capability_snapshot_provider(self, provider: Any) -> None:
        """让 Thread 视图与 composer 目录共用同一能力快照 revision。"""
        self._capability_snapshot_provider = provider

    def bind_model_effort_validator(self, validator: Any) -> None:
        self._model_effort_validator = validator

    def bind_capability_cache_writer(self, writer: Any) -> None:
        """CU fixture / refresh paths may write the shared capability cache."""
        self._capability_cache_writer = writer

    def bind_capability_refresh_trigger(self, trigger: Any) -> None:
        """Thread 视图在目录缺失/过期时主动后台刷新（#119）。"""
        self._capability_refresh_trigger = trigger

    def bind_capability_failure_provider(self, provider: Any) -> None:
        """Expose capability refresh failure snapshot to thread views (#188)."""
        self._capability_failure_provider = provider

    def emit_context_window_event(
        self,
        thread_id: str,
        *,
        total: Optional[int] = None,
        limit: Optional[int] = None,
        zones: Optional[list[dict[str, Any]]] = None,
        compacted: Optional[list[dict[str, Any]]] = None,
        compact_status: str = "idle",
        source: str = "event",
    ) -> None:
        """C27: Append a core.context.window event to the thread stream.

        The projection handler will update ``ConversationState.context_window``.
        """
        from . import events as conversation_events

        state = self._conv.get_state(thread_id)
        prev = state.context_window or {} if state else {}
        payload: dict[str, Any] = {
            "total": total if total is not None else prev.get("total"),
            "limit": limit if limit is not None else prev.get("limit"),
            "zones": zones if zones is not None else prev.get("zones", []),
            "compacted": compacted if compacted is not None else prev.get("compacted", []),
            "compact_status": compact_status,
            "source": source,
        }
        event = conversation_events.thread_event(
            thread_id, conversation_events.EV_CONTEXT_WINDOW, payload)
        self._store.append_events(event)

    def register_user_input_fixture(
        self, thread_id: str, request_id: str, *, pending: dict[str, Any]
    ) -> None:
        """Mark a synthetic pending user-input so resolve can skip the Runtime."""
        self._user_input_fixtures[thread_id] = {
            "request_id": str(request_id),
            "pending": dict(pending),
        }

    def clear_user_input_fixture(self, thread_id: str) -> None:
        self._user_input_fixtures.pop(thread_id, None)

    def capture_user_input_fixture(
        self,
        thread_id: str,
        request_id: str,
        *,
        decision: str,
        answers: dict[str, Any],
        text: str = "",
    ) -> bool:
        """Record fixture resolve payload. Returns True when this was a fixture."""
        row = self._user_input_fixtures.get(thread_id)
        if not row or str(row.get("request_id")) != str(request_id):
            return False
        self._user_input_fixture_captures[thread_id] = {
            "request_id": str(request_id),
            "decision": decision,
            "answers": dict(answers),
            "text": text,
        }
        self._user_input_fixtures.pop(thread_id, None)
        return True

    def get_user_input_fixture_capture(
        self, thread_id: str
    ) -> Optional[dict[str, Any]]:
        row = self._user_input_fixture_captures.get(thread_id)
        return dict(row) if row else None

    def has_user_input_fixture(self, thread_id: str, request_id: str) -> bool:
        row = self._user_input_fixtures.get(thread_id)
        return bool(row and str(row.get("request_id")) == str(request_id))

    @property
    def store(self) -> PlatformStore:
        return self._store

    @property
    def conv(self) -> ConversationStore:
        return self._conv

    @property
    def bindings(self) -> CapabilityBindingService:
        return self._bindings

    @property
    def projection(self) -> ConversationProjection:
        return self._projection

    # -- Project / Workspace ---------------------------------------------------

    def create_project(
        self, name: str, *, description: str = "",
        settings: Optional[dict[str, Any]] = None, project_id: str = "",
    ) -> Project:
        project = Project(
            **({"project_id": project_id} if project_id else {}),
            name=name, description=description,
            settings=dict(settings or {}))
        return self._store.save(project)

    def get_project(self, project_id: str) -> Optional[Project]:
        return self._store.get(Project, project_id)

    def plan_workspace(
        self,
        *,
        project_id: Optional[str] = None,
        kind: str = WORKSPACE_LOCAL,
        root_path: str = "",
        workspace_id: str = "",
        mode: str = "",
        branch: str = "",
        base_ref: str = "",
        parent_root: str = "",
        settings: Optional[dict[str, Any]] = None,
    ) -> Workspace:
        """Workspace 绑定的纯校验（无副作用）。

        - local/git：校验目录存在；git 要求 ``.git`` 存在（含 worktree 的 .git 文件）。
        - isolated：只计算目标路径（目录在 ``save_workspace`` 才创建）。
        - mode=new_worktree：计算 Muteki 托管路径，不立刻 ``git worktree add``。
        """
        from .git_workspace import resolve_git_common_dir, resolve_git_root

        kind = str(kind or WORKSPACE_LOCAL).strip()
        mode = str(mode or "").strip()
        branch = str(branch or "").strip()
        base_ref = str(base_ref or "").strip() or "HEAD"
        parent_root = str(parent_root or "").strip()
        meta = dict(settings or {})

        if mode == WS_MODE_NEW:
            workspace_id = workspace_id or new_id("ws")
            if not project_id:
                raise ConversationError("new_worktree 需要 project_id")
            if not branch:
                raise ConversationError("new_worktree 需要 branch")
            primary = parent_root or self.primary_project_root(project_id) or ""
            if not primary:
                raise ConversationError("项目尚未绑定可派生 worktree 的主检出")
            parent = resolve_git_root(primary)
            if parent is None:
                raise ConversationError(f"主检出不是 Git 仓库：{primary!r}")
            root_path = str(self._workspace_root / "worktrees" / workspace_id)
            common = resolve_git_common_dir(str(parent))
            meta.update({
                "mode": WS_MODE_NEW,
                "branch": branch,
                "base_ref": base_ref,
                "worktree_of": str(parent),
                "git_common_dir": str(common) if common else "",
                "created_via": "new_worktree",
                "pending_create": True,
            })
            kind = WORKSPACE_GIT
            return Workspace(
                workspace_id=workspace_id,
                project_id=project_id,
                kind=kind,
                root_path=root_path,
                settings=meta,
            )

        if mode == WS_MODE_EXISTING:
            path = Path(root_path).expanduser().resolve()
            if not path.is_dir():
                raise ConversationError(f"workspace 目录不存在：{root_path!r}")
            git_root = resolve_git_root(str(path))
            if git_root is None:
                raise ConversationError(f"不是 Git worktree：{root_path!r}")
            root_path = str(git_root)
            common = resolve_git_common_dir(root_path)
            primary = parent_root or (
                self.primary_project_root(project_id) if project_id else ""
            ) or ""
            if primary:
                primary_common = resolve_git_common_dir(primary)
                if primary_common is None or primary_common != common:
                    raise ConversationError("所选 worktree 不属于当前项目仓库")
            meta.update({
                "mode": WS_MODE_EXISTING,
                "branch": branch,
                "worktree_of": primary or str(common.parent if common else ""),
                "git_common_dir": str(common) if common else "",
                "created_via": "existing_worktree",
            })
            kind = WORKSPACE_GIT
            return Workspace(
                **({"workspace_id": workspace_id} if workspace_id else {}),
                project_id=project_id or None,
                kind=kind,
                root_path=root_path,
                settings=meta,
            )

        if kind in (WORKSPACE_LOCAL, WORKSPACE_GIT):
            path = Path(root_path).expanduser().resolve()
            if not path.is_dir():
                raise ConversationError(f"workspace 目录不存在：{root_path!r}")
            if kind == WORKSPACE_GIT and not (path / ".git").exists():
                raise ConversationError(f"不是 Git 仓库：{root_path!r}")
            root_path = str(path)
            if mode == WS_MODE_SHARED or not mode:
                meta.setdefault("mode", WS_MODE_SHARED if mode == WS_MODE_SHARED else meta.get("mode", ""))
                if mode == WS_MODE_SHARED:
                    meta["mode"] = WS_MODE_SHARED
                    meta["created_via"] = "shared_checkout"
        elif kind == WORKSPACE_ISOLATED:
            workspace_id = workspace_id or new_id("ws")
            root_path = str(self._workspace_root / workspace_id)
        else:
            raise ConversationError(f"未知 workspace 形态：{kind!r}")
        return Workspace(
            **({"workspace_id": workspace_id} if workspace_id else {}),
            project_id=project_id or None,
            kind=kind,
            root_path=root_path,
            settings=meta,
        )

    def save_workspace(self, workspace: Workspace) -> Workspace:
        """落库一个经 ``plan_workspace`` 校验的 Workspace。

        isolated 建目录；new_worktree 在此执行 ``git worktree add``，失败则清理。
        """
        from .git_workspace import GitWorkspaceError, add_worktree

        settings = dict(workspace.settings or {})
        if workspace.kind == WORKSPACE_ISOLATED:
            Path(workspace.root_path).mkdir(parents=True, exist_ok=True)
        elif settings.get("mode") == WS_MODE_NEW and settings.get("pending_create"):
            parent = str(settings.get("worktree_of") or "")
            branch = str(settings.get("branch") or "")
            base_ref = str(settings.get("base_ref") or "HEAD")
            try:
                created = add_worktree(
                    parent,
                    workspace.root_path,
                    branch=branch,
                    base_ref=base_ref,
                    create_branch=True,
                )
            except GitWorkspaceError as exc:
                raise ConversationError(str(exc)) from exc
            settings["pending_create"] = False
            settings["branch"] = created.get("current_branch") or branch
            settings["git_common_dir"] = created.get("git_common_dir") or settings.get(
                "git_common_dir", ""
            )
            workspace = workspace.model_copy(update={"settings": settings})
        return self._store.save(workspace)

    def bind_workspace(
        self,
        *,
        project_id: Optional[str] = None,
        kind: str = WORKSPACE_LOCAL,
        root_path: str = "",
        workspace_id: str = "",
        mode: str = "",
        branch: str = "",
        base_ref: str = "",
        parent_root: str = "",
        settings: Optional[dict[str, Any]] = None,
    ) -> Workspace:
        """绑定 Workspace（同步路径）：plan + save。"""
        return self.save_workspace(self.plan_workspace(
            project_id=project_id,
            kind=kind,
            root_path=root_path,
            workspace_id=workspace_id,
            mode=mode,
            branch=branch,
            base_ref=base_ref,
            parent_root=parent_root,
            settings=settings,
        ))

    def get_workspace(self, workspace_id: str) -> Optional[Workspace]:
        return self._store.get(Workspace, workspace_id)

    def primary_project_root(self, project_id: str) -> Optional[str]:
        """项目主检出路径：优先最早的 local Workspace（与目录列表一致）。"""
        project_id = str(project_id or "").strip()
        if not project_id:
            return None
        locals_: list[Workspace] = []
        others: list[Workspace] = []
        for workspace in self._store.list(Workspace):
            if workspace.project_id != project_id:
                continue
            mode = str((workspace.settings or {}).get("mode") or "")
            if workspace.kind == WORKSPACE_LOCAL or (
                workspace.kind == WORKSPACE_GIT and mode in ("", WS_MODE_SHARED)
            ):
                # 主检出：local，或显式 shared_checkout 的 git
                if workspace.kind == WORKSPACE_LOCAL:
                    locals_.append(workspace)
                elif mode == WS_MODE_SHARED:
                    others.append(workspace)
            elif workspace.kind == WORKSPACE_GIT and mode in (
                WS_MODE_NEW, WS_MODE_EXISTING,
            ):
                continue
            else:
                others.append(workspace)
        candidates = locals_ or others
        if not candidates:
            return None
        candidates.sort(key=lambda item: str(item.created_at or ""))
        root = str(candidates[0].root_path or "").strip()
        return root or None

    def primary_project_workspace(self, project_id: str) -> Optional[Workspace]:
        """返回项目主 Workspace 行（与 ``primary_project_root`` 同口径）。"""
        project_id = str(project_id or "").strip()
        if not project_id:
            return None
        locals_: list[Workspace] = []
        for workspace in self._store.list(Workspace):
            if workspace.project_id != project_id:
                continue
            mode = str((workspace.settings or {}).get("mode") or "")
            if workspace.kind == WORKSPACE_LOCAL:
                locals_.append(workspace)
            elif workspace.kind == WORKSPACE_GIT and mode == WS_MODE_SHARED:
                locals_.append(workspace)
        if not locals_:
            # 回退：任意非 worktree 派生行
            for workspace in self._store.list(Workspace):
                if workspace.project_id != project_id:
                    continue
                mode = str((workspace.settings or {}).get("mode") or "")
                if mode in (WS_MODE_NEW, WS_MODE_EXISTING):
                    continue
                locals_.append(workspace)
        if not locals_:
            return None
        locals_.sort(key=lambda item: str(item.created_at or ""))
        return locals_[0]

    def normalize_workspace_root(self, root_path: str) -> str:
        path = Path(root_path).expanduser()
        try:
            return str(path.resolve())
        except OSError:
            return str(path)

    def active_threads_for_workspace_root(
        self,
        root_path: str,
        *,
        exclude_thread_id: str = "",
    ) -> list[Any]:
        """派生占用：非 archived Thread 的 workspace.root_path 指向同一路径。"""
        target = self.normalize_workspace_root(root_path)
        exclude = str(exclude_thread_id or "").strip()
        occupied: list[Any] = []
        for thread in self.list_threads():
            if exclude and thread.thread_id == exclude:
                continue
            state = self._conv.get_state(thread.thread_id)
            if state.status == "archived":
                continue
            if not thread.workspace_id:
                continue
            workspace = self.get_workspace(str(thread.workspace_id))
            if workspace is None or not workspace.root_path:
                continue
            if self.normalize_workspace_root(workspace.root_path) == target:
                occupied.append(thread)
        return occupied

    def delete_worktree_workspace(
        self,
        workspace_id: str,
        *,
        force: bool = False,
    ) -> dict[str, Any]:
        """显式删除 Muteki 登记的 worktree；归档不会调用本方法。"""
        from .git_workspace import GitWorkspaceError, remove_worktree

        workspace = self.get_workspace(workspace_id)
        if workspace is None:
            raise ConversationError(f"unknown workspace: {workspace_id}")
        mode = str((workspace.settings or {}).get("mode") or "")
        if mode not in (WS_MODE_NEW, WS_MODE_EXISTING) and workspace.kind != WORKSPACE_GIT:
            raise ConversationError("仅 worktree 工作区支持显式删除")
        occupants = self.active_threads_for_workspace_root(workspace.root_path)
        if occupants:
            ids = ", ".join(item.thread_id for item in occupants[:5])
            raise ConversationError(
                f"worktree 仍被活动 Thread 占用：{ids}",
            )
        try:
            result = remove_worktree(
                workspace.root_path,
                force=force,
                allow_dirty=force,
            )
        except GitWorkspaceError as exc:
            raise ConversationError(str(exc)) from exc
        return {"workspace_id": workspace_id, **result}

    # -- Thread ------------------------------------------------------------------

    def create_thread(
        self,
        *,
        project_id: str = "",
        workspace_id: str = "",
        title: str = "",
        title_source: str = "user",
        mode: str = "conversation",
        principal_id: str = "local-user",
        thread_id: str = "",
        runtime: Optional[dict[str, Any]] = None,
    ) -> Thread:
        """创建 Thread 并按模式签发 CapabilityBinding。"""
        mode = str(mode or "conversation").strip()
        try:
            ThreadMode(mode)
        except ValueError as exc:
            raise ConversationError(f"未知 Thread 模式：{mode!r}") from exc
        if project_id and self.get_project(project_id) is None:
            raise ConversationError(f"unknown project: {project_id}")
        if workspace_id and self.get_workspace(workspace_id) is None:
            raise ConversationError(f"unknown workspace: {workspace_id}")
        thread = Thread(
            **({"thread_id": thread_id} if thread_id else {}),
            project_id=project_id or None,
            workspace_id=workspace_id or None,
            title=title,
            title_source=str(title_source or "user"),
            mode=mode)
        return self.activate_thread(thread, principal_id, runtime=runtime)

    def activate_thread(
        self,
        thread: Thread,
        principal_id: str = "local-user",
        *,
        runtime: Optional[dict[str, Any]] = None,
    ) -> Thread:
        """落库 Thread、按模式签发 CapabilityBinding、保存 Runtime 选择。"""
        # Validate and materialize the selection before creating any Thread or
        # Binding rows. A bad/missing credential must fail atomically at creation,
        # rather than leaving a Thread that only fails on its first Turn.
        selection = (
            self._build_runtime_selection(thread.thread_id, runtime)
            if runtime else None
        )
        self._store.save(thread)
        # 按模式生成 Thread 级 CapabilityBinding（CAP-01 唯一签发入口）。
        self._bindings.issue_binding(thread.thread_id, principal_id, thread.mode)
        if selection is not None:
            self._conv.save_runtime_selection(selection)
        return thread

    def get_thread(self, thread_id: str) -> Optional[Thread]:
        return self._store.get(Thread, thread_id)

    def list_threads(self, project_id: str = "") -> list[Thread]:
        if project_id:
            return self._store.list(Thread, project_id=project_id)
        return self._store.list(Thread)

    def save_runtime_selection(
        self,
        thread_id: str,
        runtime: dict[str, Any],
        *,
        validate_credential: bool = True,
    ) -> ThreadRuntimeSelection:
        """保存 / 更新 Thread 的 Runtime 选择。"""
        selection = self._build_runtime_selection(
            thread_id, runtime, validate_credential=validate_credential)
        return self._conv.save_runtime_selection(selection)

    def _build_runtime_selection(
        self,
        thread_id: str,
        runtime: dict[str, Any],
        *,
        validate_credential: bool = True,
    ) -> ThreadRuntimeSelection:
        """Normalize one selection and validate its global credential reference."""
        current = self._conv.get_runtime_selection(thread_id)
        base = current.model_dump() if current is not None else {}
        merged = {**base, **{k: v for k, v in runtime.items() if v is not None}}
        if runtime.get("effort") is None and any(
            key in runtime and runtime[key] is not None and runtime[key] != base.get(key)
            for key in ("adapter_id", "instance_id", "credential_id", "model")
        ):
            merged["effort"] = ""
        adapter_id = str(merged.get("adapter_id") or "").strip()
        engine = engine_for_adapter(adapter_id) if adapter_id else ""
        try:
            ensure_engine_supported(engine or adapter_id)
        except EngineTemporarilyUnsupportedError as exc:
            raise ConversationError(exc.reason) from exc
        if adapter_id and not engine:
            raise ConversationError(f"未知 Runtime adapter：{adapter_id!r}")
        raw_credential = (
            merged.get("credential_id")
            or merged.get("credential_ref")
            or ""
        )
        if not raw_credential and engine:
            raw_credential = system_credential_id(engine)
        try:
            credential_id = canonical_credential_id(
                raw_credential,
                engine=engine if validate_credential and engine else "",
            )
        except ValueError as exc:
            raise ConversationError(f"非法 credential_id：{raw_credential!r}") from exc
        model = str(merged.get("model") or "").strip()
        if validate_credential:
            self._validate_credential_binding(
                credential_id=credential_id, engine=engine, model=model)
        access_mode = str(
            merged.get("access_mode") or AccessMode.SUPERVISED.value
        ).strip()
        if access_mode not in ACCESS_MODE_VALUES:
            raise ConversationError(f"未知 Agent 访问模式：{access_mode!r}")
        selection = ThreadRuntimeSelection(
            thread_id=thread_id,
            adapter_id=adapter_id,
            instance_id=str(merged.get("instance_id") or "default"),
            credential_id=credential_id,
            model=model,
            effort="" if merged.get("effort") == "default" else str(merged.get("effort") or ""),
            access_mode=access_mode,
            permission_mode=str(merged.get("permission_mode") or ""),
            sandbox_mode=str(merged.get("sandbox_mode") or ""),
            updated_at=utcnow(),
        )
        if validate_credential and self._model_effort_validator is not None:
            try:
                self._model_effort_validator(selection)
            except ValueError as exc:
                raise ConversationError(str(exc)) from exc
        return selection

    def _validate_credential_binding(
        self, *, credential_id: str, engine: str, model: str
    ) -> None:
        """校验凭据与引擎绑定。

        对话路径不要求事先「真实测试」通过：模型是否可用由 Runtime 在 turn
        执行时报错；前端据此引导用户去凭据中心排查。
        """
        if not credential_id:
            if engine:
                raise ConversationError("该 Thread 尚未选择全局凭据")
            return
        if not engine:
            raise ConversationError("选择凭据前必须先选择 Runtime adapter")
        if not model:
            raise ConversationError("创建 Thread 前必须选择模型")

        store = CredentialAccountStore(account_store_root(self._sessions_root))
        system_engine = engine_from_system_credential_id(credential_id)
        if system_engine:
            if system_engine != engine:
                raise ConversationError(
                    f"凭据 {credential_id} 不能用于 {engine}")
            if detect_system_login(system_engine, fresh=True) != "present":
                raise ConversationError(
                    f"宿主 {system_engine} 登录态尚未配置或无法确认；"
                    "请前往设置 → 凭据中心检查登录态")
            return

        account_id = account_id_from_credential_id(credential_id)
        account = store.inspect(account_id)
        if account is None or not account.present:
            raise ConversationError(
                f"全局凭据账号不可用：{account_id or credential_id}；"
                "请前往设置 → 凭据中心配置")
        target_engine = str(
            (account.details or {}).get("target_engine") or "").strip().lower()
        account_engine = target_engine or str(account.engine or "").strip().lower()
        if account_engine not in {"", "api", "unknown", engine}:
            raise ConversationError(
                f"凭据 {credential_id} 属于 {account_engine}，不能用于 {engine}")

    def runtime_selection(self, thread_id: str) -> ThreadRuntimeSelection:
        return self._conv.get_runtime_selection(thread_id) or ThreadRuntimeSelection(
            thread_id=thread_id)

    def set_thread_mode(
        self, thread_id: str, mode: str, *, principal_id: str = "local-user"
    ) -> Any:
        """切换 Thread 模式：更新对象并在同一 binding_id 下轮换 Binding
        （CAP-01：新版本 + 撤销旧版本及其全部 Grant）。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        try:
            ThreadMode(str(mode))
        except ValueError as exc:
            raise ConversationError(f"未知 Thread 模式：{mode!r}") from exc
        self._store.save(thread.model_copy(update={"mode": str(mode)}))
        return self._bindings.issue_binding(thread_id, principal_id, str(mode))

    def rename_thread(self, thread_id: str, title: str) -> Thread:
        """更新 Thread 标题；标题属于对象本体，事件用于 SSE 和审计。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        title = str(title or "").strip()
        if not title:
            raise ConversationError("thread title cannot be empty")
        return self._store.save(thread.model_copy(update={
            "title": title[:160],
            "title_source": "user",
            "metadata_revision": thread.metadata_revision + 1,
            "updated_at": utcnow(),
        }))

    def reserve_thread_metadata(self, thread_id: str, turn_id: str) -> Thread:
        """为异步标题/摘要生成预留一个单调版本。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        turn_id = str(turn_id or "").strip()
        if not turn_id:
            raise ConversationError("metadata turn_id cannot be empty")
        return self._store.save(thread.model_copy(update={
            "metadata_revision": thread.metadata_revision + 1,
            "metadata_turn_id": turn_id,
        }))

    def apply_generated_thread_metadata(
        self,
        thread_id: str,
        *,
        expected_revision: int,
        turn_id: str,
        title: str = "",
        summary: str = "",
    ) -> Optional[Thread]:
        """带版本围栏写入模型元数据；迟到结果返回 ``None``。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            return None
        if (
            thread.metadata_revision != int(expected_revision)
            or str(thread.metadata_turn_id or "") != str(turn_id or "")
        ):
            return None
        active_turn_ids = {
            item.turn_id for item in self._conv.list_current_turns(thread_id)
        }
        basis_turn = self._conv.get_turn(turn_id)
        if (
            turn_id not in active_turn_ids
            or basis_turn is None
            or basis_turn.status != TURN_COMPLETED
        ):
            return None

        clean_title = str(title or "").strip()[:160]
        clean_summary = str(summary or "").strip()[:500]
        updates: dict[str, Any] = {}
        if clean_title and thread.title_source == "fallback":
            updates.update({"title": clean_title, "title_source": "model"})
        if clean_summary:
            updates["summary"] = clean_summary
        if not updates:
            return None
        updates["updated_at"] = utcnow()
        return self._store.save(thread.model_copy(update=updates))

    def archive_thread(self, thread_id: str) -> None:
        """归档：撤销该 Thread 的 CapabilityBinding（联动撤销全部 Grant）。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        for binding in self._store.list(CapabilityBinding, thread_id=thread_id):
            if binding.revoked_at is None:
                self._bindings.revoke_binding(binding.binding_id, binding.binding_version)

    def complete_fork(
        self,
        forked: Thread,
        source_thread_id: str,
        *,
        from_turn_id: str = "",
        principal_id: str = "local-user",
    ) -> Optional[Task]:
        """落库一个 fork 出的 Thread（命令 Handler 在 plan 已确定其身份）。

        分叉会继承选定 Turn 之前的可见消息、已附着 Artifact、Runtime 选择、
        Project 与 Workspace。继承消息同时用于新 Runtime 的首轮上下文，避免
        页面看起来完成了分叉、实际 Agent 却从空会话开始。
        """
        source = self.get_thread(source_thread_id)
        if source is None:
            raise ConversationError(f"unknown thread: {source_thread_id}")
        self.activate_thread(forked, principal_id)
        selection = self._conv.get_runtime_selection(source_thread_id)
        if selection is not None:
            self._conv.save_runtime_selection(selection.model_copy(
                update={"thread_id": forked.thread_id, "updated_at": utcnow()}))
        cutoff_seq: Optional[int] = None
        if from_turn_id:
            source_turn = self._conv.get_turn(from_turn_id)
            if source_turn is None or source_turn.thread_id != source_thread_id:
                raise ConversationError(
                    f"turn {from_turn_id} 不属于 thread {source_thread_id}")
            cutoff_seq = source_turn.seq

        current_turns = self._conv.list_current_turns(source_thread_id)
        source_turns = {
            item.turn_id: item.seq
            for item in current_turns
        }
        source_messages = []
        for message in self._conv.list_current_messages(source_thread_id):
            turn_seq = source_turns.get(str(message.turn_id or ""))
            if cutoff_seq is not None and turn_seq is not None and turn_seq > cutoff_seq:
                continue
            source_messages.append(message)

        inherited_messages = []
        for index, message in enumerate(source_messages):
            inherited = ConversationMessage(
                thread_id=forked.thread_id,
                turn_id=None,
                role=message.role,
                text=message.text,
                # 新 Thread 的 fork / artifact / turn 事件从 1 开始。继承历史
                # 使用非正序号，确保首条新消息始终排在完整历史之后。
                stream_seq=index - len(source_messages) + 1,
                created_at=message.created_at,
            )
            self._conv.save_message(inherited)
            inherited_messages.append(inherited)

        if inherited_messages:
            state = self._conv.get_state(forked.thread_id)
            self._conv.save_state(state.model_copy(update={
                "message_count": len(inherited_messages),
                "last_message_preview": inherited_messages[-1].text[:200],
            }))

        from . import events as conversation_events
        inherited_artifact_events = []
        current_turn_ids = set(source_turns)
        for event in self._store.read_events(
            "thread", source_thread_id, limit=10_000
        ):
            if event.event_type not in {
                conversation_events.EV_ARTIFACT_ATTACHED,
                conversation_events.EV_ARTIFACT_CREATED,
            }:
                continue
            event_turn_id = str(event.payload.get("turn_id") or "")
            if event_turn_id and event_turn_id not in current_turn_ids:
                continue
            if (
                cutoff_seq is not None
                and event_turn_id
                and source_turns.get(event_turn_id, cutoff_seq + 1) > cutoff_seq
            ):
                continue
            inherited_artifact_events.append(conversation_events.thread_event(
                forked.thread_id,
                event.event_type,
                {**dict(event.payload), "source_thread_id": source_thread_id},
            ))
        if inherited_artifact_events:
            self._store.append_events(inherited_artifact_events)
        task: Optional[Task] = None
        if from_turn_id:
            turn = self._conv.get_turn(from_turn_id)
            assert turn is not None
            task = Task(
                thread_id=forked.thread_id,
                project_id=forked.project_id,
                kind="conversation.fork",
                title=forked.title,
                input={
                    "source_thread_id": source_thread_id,
                    "source_turn_id": from_turn_id,
                    "text": turn.text,
                },
            )
            self._store.save(task)
        return task

    def fork_thread(
        self,
        source_thread_id: str,
        *,
        from_turn_id: str = "",
        title: str = "",
        principal_id: str = "local-user",
    ) -> tuple[Thread, Optional[Task]]:
        """fork Thread 的同步便捷路径（命令 Handler 之外使用）。"""
        source = self.get_thread(source_thread_id)
        if source is None:
            raise ConversationError(f"unknown thread: {source_thread_id}")
        forked = Thread(
            project_id=source.project_id,
            workspace_id=source.workspace_id,
            title=title or (f"{source.title}（fork）" if source.title else ""),
            mode=source.mode,
        )
        task = self.complete_fork(
            forked, source_thread_id,
            from_turn_id=from_turn_id, principal_id=principal_id)
        return forked, task

    # -- Turn ---------------------------------------------------------------------

    def request_turn(
        self,
        thread_id: str,
        *,
        text: str = "",
        kind: str = "message",
        command_id: str = "",
        idempotency_key: Optional[str] = None,
        attachments: Optional[list[str]] = None,
        capability_refs: Optional[list[dict[str, Any]]] = None,
        runtime_invocation: Optional[dict[str, Any]] = None,
        retry_of_turn_id: Optional[str] = None,
    ) -> tuple[TurnRecord, TurnRunRef, Task, bool]:
        """创建 Turn + Task + Run（幂等）。

        返回 (turn, run, task, created)；同 ``(thread_id, idempotency_key)``
        重复请求返回既有 Turn（``created=False``），不重复创建。
        """
        existing = self._conv.find_turn_by_idempotency(
            thread_id, idempotency_key or "")
        if existing is not None:
            run = self._conv.get_run(existing.run_id or "")
            task = self._store.get(Task, existing.task_id) if existing.task_id else None
            if run is not None and task is not None:
                return existing, run, task, False

        state = self._conv.get_state(thread_id)
        if state.status == "archived":
            raise ConversationError(
                "已归档的对话不能发送消息，请先取消归档")
        seq = self._conv.next_turn_seq(thread_id)
        task = Task(
            thread_id=thread_id,
            project_id=(self.get_thread(thread_id) or Thread()).project_id,
            kind="conversation.message",
            title=text[:80],
            input={
                "text": text,
                "turn_seq": seq,
                **({"retry_of_turn_id": retry_of_turn_id}
                   if retry_of_turn_id else {}),
            },
        )
        turn = TurnRecord(
            thread_id=thread_id,
            task_id=task.task_id,
            command_id=command_id,
            idempotency_key=idempotency_key or None,
            seq=seq,
            kind=kind,
            retry_of_turn_id=retry_of_turn_id,
            text=text,
            attachments=list(attachments or []),
            capability_refs=list(capability_refs or []),
            runtime_invocation=dict(runtime_invocation or {}),
        )
        run = TurnRunRef(
            turn_id=turn.turn_id,
            task_id=task.task_id,
            thread_id=thread_id,
            generation=state.current_generation,
        )
        turn = turn.model_copy(update={"run_id": run.run_id})
        self._conv.claim_active_turn(thread_id, turn.turn_id)
        try:
            self._store.save(task)
            self._conv.save_turn(turn)
            self._conv.save_run(run)
        except Exception:
            self._conv.release_active_turn(thread_id, turn.turn_id)
            raise
        return turn, run, task, True

    def retry_turn(
        self,
        thread_id: str,
        turn_id: str,
        *,
        command_id: str = "",
        idempotency_key: Optional[str] = None,
        text: Optional[str] = None,
    ) -> tuple[TurnRecord, TurnRunRef, Task, list[str], bool]:
        """Replace ``turn_id`` and every later turn on the active branch.

        The immutable event log and old Turn/Run rows remain available for
        audit.  Current views and future model context only expose records that
        have not been marked ``superseded``.

        When ``text`` is provided and differs from the original turn text, the
        replacement is an edit-resend (``kind=edit_resend``); otherwise classic
        retry with the original prompt.
        """
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        state = self._conv.get_state(thread_id)
        if state.status == "archived":
            raise ConversationError("archived thread cannot retry turns")
        if state.running_turn_id:
            raise ConversationError("当前仍有执行中的 Turn，结束后才能重试")

        existing = self._conv.find_turn_by_idempotency(
            thread_id, idempotency_key or "",
        )
        if existing is not None:
            run = self._conv.get_run(existing.run_id or "")
            task = (
                self._store.get(Task, existing.task_id)
                if existing.task_id else None
            )
            if run is not None and task is not None:
                return existing, run, task, [], False

        target = self._conv.get_turn(turn_id)
        if (
            target is None
            or target.thread_id != thread_id
            or target.status == TURN_SUPERSEDED
        ):
            raise ConversationError(f"turn {turn_id} 不在当前对话分支中")
        if target.status not in {
            TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED,
        }:
            raise ConversationError(
                f"turn {turn_id} 当前状态为 {target.status}，还不能重试"
            )

        replaced = [
            turn for turn in self._conv.list_current_turns(thread_id)
            if turn.seq >= target.seq
        ]
        superseded_turn_ids = [turn.turn_id for turn in replaced]
        now = utcnow()
        for old_turn in replaced:
            self._conv.save_turn(old_turn.model_copy(update={
                "status": TURN_SUPERSEDED,
                "completed_at": old_turn.completed_at or now,
            }))
            if old_turn.run_id:
                old_run = self._conv.get_run(old_turn.run_id)
                if old_run is not None:
                    self._conv.save_run(old_run.model_copy(update={
                        "status": TURN_SUPERSEDED,
                        "ended_at": old_run.ended_at or now,
                    }))
            self._conv.release_active_turn(thread_id, old_turn.turn_id)

        retained_messages = self._conv.list_current_messages(thread_id)
        next_generation = state.current_generation + 1
        self._conv.save_state(state.model_copy(update={
            "running_turn_id": None,
            "current_generation": next_generation,
            "pending_approvals": {},
            "pending_approval": None,
            "pending_user_input": None,
            "last_error": {},
            "usage": {},
            "message_count": len(retained_messages),
            "last_message_preview": (
                retained_messages[-1].text[:200] if retained_messages else ""
            ),
        }))

        replacement_text = target.text if text is None else str(text)
        is_edit = (
            text is not None
            and str(text) != target.text
        )
        turn, run, task, created = self.request_turn(
            thread_id,
            text=replacement_text,
            kind=TURN_KIND_EDIT_RESEND if is_edit else TURN_KIND_RETRY,
            command_id=command_id,
            idempotency_key=idempotency_key,
            attachments=list(target.attachments),
            capability_refs=list(target.capability_refs),
            runtime_invocation=dict(target.runtime_invocation),
            retry_of_turn_id=target.turn_id,
        )
        return turn, run, task, superseded_turn_ids, created

    def preview_turn_impact(
        self,
        thread_id: str,
        turn_id: str,
        *,
        mode: str = "retry",
        text: str = "",
        file_mode: str = "keep_files",
        runtime_connection: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """Dry-run ImpactPreview for Retry / Edit-resend / Fork / native rewind."""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        target = self._conv.get_turn(turn_id)
        if (
            target is None
            or target.thread_id != thread_id
            or target.status == TURN_SUPERSEDED
        ):
            raise ConversationError(f"turn {turn_id} 不在当前对话分支中")
        current_turns = self._conv.list_current_turns(thread_id)
        workspace = (
            self.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        root = str(workspace.root_path or "").strip() if workspace else ""
        artifact_rows: dict[str, dict[str, Any]] = {}
        for event in self._store.read_events("thread", thread_id, limit=10_000):
            if event.event_type not in {
                "core.artifact.attached", "core.artifact.created",
            }:
                continue
            payload = dict(event.payload)
            digest = str(payload.get("sha256") or "")
            if digest:
                artifact_rows[digest] = payload
        connection = dict(runtime_connection or {})
        if not connection:
            # Soft-read #16 matrix from probe cache when present on registry.
            connection = {"matrix": None, "capabilities": {}}
            if self._adapter_registry is not None:
                try:
                    selection = self.runtime_selection(thread_id)
                    record = self._adapter_registry.record(
                        selection.adapter_id, selection.instance_id)
                    report = record.last_probe if record is not None else None
                    if report is not None:
                        connection["capabilities"] = (
                            report.capabilities.model_dump(mode="json")
                        )
                    # Prefer matrix already attached by #16 snapshot path.
                    snapshot = getattr(record, "capability_snapshot", None) \
                        if record is not None else None
                    matrix = getattr(snapshot, "matrix", None) if snapshot else None
                    if matrix is not None:
                        if hasattr(matrix, "model_dump"):
                            connection["matrix"] = matrix.model_dump(mode="json")
                        elif isinstance(matrix, dict):
                            connection["matrix"] = matrix
                except Exception:
                    pass
        normalized = str(mode or "retry").strip()
        if normalized not in {
            "retry", "edit_resend", "fork", "native_rewind",
        }:
            raise ConversationError(f"unknown impact mode: {normalized}")
        return build_impact_preview(
            mode=normalized,  # type: ignore[arg-type]
            target=target,
            current_turns=current_turns,
            artifact_rows=artifact_rows,
            workspace_root=root,
            file_mode=str(file_mode or "keep_files"),
            runtime_connection=connection,
            edited_text=str(text or ""),
        )

    def native_rewind_turn(
        self,
        thread_id: str,
        turn_id: str,
        *,
        command_id: str = "",
        idempotency_key: Optional[str] = None,
        file_mode: str = "keep_files",
        provider_rewind: Optional[Any] = None,
        capability_override: Optional[dict[str, Any]] = None,
        runtime_connection: Optional[dict[str, Any]] = None,
    ) -> tuple[list[str], bool]:
        """Provider-native rewind with atomic failure semantics (C11).

        Order: verify capability → call Provider rewind → only then supersede
        Muteki turns. On Provider failure, conversation and files are untouched.
        ``sync_files`` is refused unless a workspace checkpoint capability is
        verified (not available on current adapters → refuse).

        Returns ``(superseded_turn_ids, applied)``. Idempotent replays return
        ``([], False)`` without re-calling the Provider.
        """
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        state = self._conv.get_state(thread_id)
        if state.status == "archived":
            raise ConversationError("archived thread cannot rewind turns")
        if state.running_turn_id:
            raise ConversationError("当前仍有执行中的 Turn，结束后才能回退")

        # Lightweight idempotency: a prior successful rewind for this key is
        # recorded as a thread event; duplicate commands no-op.
        if idempotency_key:
            for event in self._store.read_events("thread", thread_id, limit=500):
                if (
                    event.event_type == "core.turn.rewound"
                    and str(event.idempotency_key or "") == idempotency_key
                ):
                    return list(event.payload.get("superseded_turn_ids") or []), False

        target = self._conv.get_turn(turn_id)
        if (
            target is None
            or target.thread_id != thread_id
            or target.status == TURN_SUPERSEDED
        ):
            raise ConversationError(f"turn {turn_id} 不在当前对话分支中")
        if target.status not in {
            TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED,
        }:
            raise ConversationError(
                f"turn {turn_id} 当前状态为 {target.status}，还不能回退"
            )

        if capability_override is not None:
            provider = dict(capability_override)
        else:
            preview = self.preview_turn_impact(
                thread_id,
                turn_id,
                mode="native_rewind",
                file_mode=file_mode,
                runtime_connection=runtime_connection,
            )
            provider = dict(preview.get("provider") or {})
        if not provider.get("invocable"):
            raise ConversationError(
                provider.get("reason")
                or "当前 Runtime 不支持原生回退；请改用 Fork 或 Retry"
            )
        if str(file_mode or "keep_files") == "sync_files":
            # No Adapter currently exposes a verified workspace checkpoint API.
            raise ConversationError(
                "工作区文件同步回退尚未通过能力校验；请改用 keep_files 或 Fork"
            )

        # Call Provider rewind BEFORE any Muteki supersede.
        rewind_fn = provider_rewind
        if rewind_fn is None and self._adapter_registry is not None:
            selection = self.runtime_selection(thread_id)
            adapter = self._adapter_registry.get(
                selection.adapter_id, selection.instance_id)
            rewind_fn = getattr(adapter, "rewind_session", None) if adapter else None
        if rewind_fn is None:
            raise ConversationError(
                "当前 Runtime 未暴露 rewind_session；请改用 Fork 或 Retry"
            )
        try:
            result = rewind_fn(thread_id=thread_id, turn_id=turn_id)
            if hasattr(result, "__await__"):
                raise ConversationError(
                    "异步 rewind_session 需由命令层 await；当前路径仅支持同步夹具"
                )
            if result is False:
                raise ConversationError("Provider rewind 失败")
        except ConversationError:
            raise
        except Exception as exc:
            raise ConversationError(
                f"Provider rewind 失败，对话与文件未改动：{exc}"
            ) from exc

        # Provider succeeded — align Muteki projection only (no new model turn).
        replaced = [
            turn for turn in self._conv.list_current_turns(thread_id)
            if turn.seq >= target.seq
        ]
        superseded_turn_ids = [turn.turn_id for turn in replaced]
        now = utcnow()
        for old_turn in replaced:
            self._conv.save_turn(old_turn.model_copy(update={
                "status": TURN_SUPERSEDED,
                "completed_at": old_turn.completed_at or now,
            }))
            if old_turn.run_id:
                old_run = self._conv.get_run(old_turn.run_id)
                if old_run is not None:
                    self._conv.save_run(old_run.model_copy(update={
                        "status": TURN_SUPERSEDED,
                        "ended_at": old_run.ended_at or now,
                    }))
            self._conv.release_active_turn(thread_id, old_turn.turn_id)

        retained_messages = self._conv.list_current_messages(thread_id)
        next_generation = state.current_generation + 1
        self._conv.save_state(state.model_copy(update={
            "running_turn_id": None,
            "current_generation": next_generation,
            "pending_approval": None,
            "pending_user_input": None,
            "last_error": {},
            "usage": {},
            "message_count": len(retained_messages),
            "last_message_preview": (
                retained_messages[-1].text[:200] if retained_messages else ""
            ),
        }))
        # Silence unused command_id for now (event layer stamps it).
        _ = command_id
        return superseded_turn_ids, True

    # -- Artifact -----------------------------------------------------------------

    def attach_artifact(
        self,
        thread_id: str,
        *,
        name: str,
        content: bytes = b"",
        content_base64: str = "",
        path: str = "",
        kind: str = "conversation.upload",
        media_type: str = "",
        run_id: Optional[str] = None,
    ) -> Artifact:
        """附着 Artifact：内容寻址（sha256），内容本体写 conversation_artifacts/。

        内容来源三选一：``content`` 字节、``content_base64`` 或 ``path``
        （本机文件路径，仅 operator 本地使用）。
        """
        if not content and content_base64:
            content = base64.b64decode(content_base64)
        if not content and path:
            content = Path(path).expanduser().read_bytes()
        if not content:
            raise ConversationError("artifact.attach 需要 content/content_base64/path")
        sha256 = hashlib.sha256(content).hexdigest()
        self._conv.write_artifact_content(sha256, content)
        artifact = Artifact(
            sha256=sha256,
            name=name,
            kind=kind,
            media_type=media_type,
            size=len(content),
            run_id=run_id,
        )
        return self._store.save(artifact)

    # -- 长期记忆（memory.timeline.v1，需用户允许） --------------------------------

    async def record_memory(
        self,
        thread_id: str,
        content: str,
        *,
        kind: str = "note",
        consent: bool = False,
        actor_id: str = "local-user",
        memory_id: str = "",
    ) -> Any:
        """写入一条长期记忆；只有用户显式允许（consent=True）才落图。"""
        if not consent:
            raise ConversationError("写入长期记忆需要用户明确允许（consent=True）")
        if self._memory_graph is None:
            raise ConversationError("未绑定 memory.timeline.v1 GraphService")
        return await self._memory_graph.append(GraphEvent(
            graph_id="memory.timeline.v1",
            event_type="memory.recorded",
            actor_id=actor_id,
            payload={
                "memory_id": memory_id or new_id("mem"),
                "thread_id": thread_id,
                "kind": kind,
                "content": content,
                "consent": True,
                "source": {
                    "actor": actor_id,
                    "thread_id": thread_id,
                    "via": "builtin.conversation",
                },
            },
        ))

    async def delete_memory(
        self,
        thread_id: str,
        memory_id: str,
        *,
        reason: str = "user_requested",
        actor_id: str = "local-user",
    ) -> Any:
        """用 tombstone 删除一条属于该 Thread 的长期记忆。"""
        if self._memory_graph is None:
            raise ConversationError("未绑定 memory.timeline.v1 GraphService")
        snapshot = await self.memory_snapshot(thread_id, include_deleted=False)
        if not any(
            item.get("memory_id") == memory_id
            for item in snapshot.get("memories", [])
        ):
            raise ConversationError(f"unknown active memory: {memory_id}")
        return await self._memory_graph.append(GraphEvent(
            graph_id="memory.timeline.v1",
            event_type="memory.deleted",
            actor_id=actor_id,
            payload={
                "memory_id": memory_id,
                "thread_id": thread_id,
                "reason": str(reason or "user_requested"),
            },
        ))

    async def memory_snapshot(
        self,
        thread_id: str,
        *,
        include_deleted: bool = False,
        query: str = "",
    ) -> dict[str, Any]:
        """读取单个 Thread 的长期记忆，并提供限定范围的文本检索。"""
        if self.get_thread(thread_id) is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        if self._memory_graph is None:
            raise ConversationError("未绑定 memory.timeline.v1 GraphService")
        snapshot = await self._memory_graph.snapshot(GraphScope(
            graph_id="memory.timeline.v1",
            scope={
                "thread_id": thread_id,
                "include_deleted": include_deleted,
            },
        ))
        state = dict(snapshot.state)
        needle = str(query or "").strip().casefold()
        if needle:
            state["memories"] = [
                item for item in state.get("memories", [])
                if needle in str(item.get("content") or "").casefold()
                or needle in str(item.get("kind") or "").casefold()
            ]
        state.update({
            "thread_id": thread_id,
            "scope": f"thread:{thread_id}",
            "watermark": snapshot.watermark,
            "query": str(query or ""),
        })
        return state

    # -- 视图 ----------------------------------------------------------------------

    def thread_view(
        self,
        thread_id: str,
        *,
        mark_read: bool = False,
        messages_limit: Any = None,
        before_stream_seq: Optional[int] = None,
        after_stream_seq: Optional[int] = None,
        include_events: bool = False,
        include_superseded: bool = False,
    ) -> dict[str, Any]:
        """Thread 页面快照：元数据 + 消息页（默认页大小，非整段历史）。"""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        state = (self._projection.mark_read(thread_id) if mark_read
                 else self._conv.get_state(thread_id))
        selection = self.runtime_selection(thread_id)
        workspace = (
            self.get_workspace(thread.workspace_id)
            if thread.workspace_id else None
        )
        bindings = self._store.list(CapabilityBinding, thread_id=thread_id)
        active = [b for b in bindings if b.revoked_at is None]
        binding = max(active, key=lambda b: b.binding_version) if active else None
        grants = (
            self._store.list(CapabilityGrant, binding_id=binding.binding_id)
            if binding is not None else []
        )
        session = (
            self._store.get(AgentSession, state.agent_session_id)
            if state.agent_session_id else None
        )
        runtime_connection: dict[str, Any] = {
            "configured": False,
            "connected": False,
            "injection_kind": "",
            "binding_version": binding.binding_version if binding else None,
            "grant_expires_at": None,
            "degradation": "Runtime session has not started",
            "capabilities": {},
            "capability_revision": 0,
            "capability_stale": True,
            "matrix": None,
            "matrix_diagnostics": [],
        }
        snapshot = None
        if self._adapter_registry is not None:
            record = self._adapter_registry.record(
                selection.adapter_id, selection.instance_id)
            report = record.last_probe if record is not None else None
            if report is not None:
                runtime_connection["capabilities"] = (
                    report.capabilities.model_dump(mode="json")
                )
            adapter = self._adapter_registry.get(
                selection.adapter_id, selection.instance_id)
            runtime_connection["configured"] = adapter is not None
            provider = self._capability_snapshot_provider
            if callable(provider):
                snapshot = provider(thread_id)
            if snapshot is not None and (
                snapshot.adapter_id != selection.adapter_id
                or (snapshot.instance_id or "default") != (selection.instance_id or "default")
            ):
                snapshot = None
            if snapshot is not None and snapshot.matrix is not None:
                matrix_payload = snapshot.public_matrix() or {}
                runtime_connection["capability_revision"] = int(
                    snapshot.revision)
                runtime_connection["capability_stale"] = bool(snapshot.stale)
                runtime_connection["matrix"] = matrix_payload
                runtime_connection["matrix_diagnostics"] = list(
                    matrix_payload.get("diagnostics") or snapshot.diagnostics
                )
            else:
                matrix = build_matrix_from_probe(
                    report,
                    revision=int(snapshot.revision) if snapshot else 0,
                    stale=True if snapshot is None else bool(snapshot.stale),
                    snapshot=snapshot or RuntimeCapabilitySnapshot(
                        adapter_id=selection.adapter_id,
                        instance_id=selection.instance_id or "default",
                        stale=True,
                    ),
                    diagnostics=(
                        list(snapshot.diagnostics) if snapshot is not None
                        else ["会话能力目录尚未加载"]
                    ),
                )
                runtime_connection["capability_revision"] = matrix.revision
                runtime_connection["capability_stale"] = matrix.stale
                runtime_connection["matrix"] = matrix.model_dump(mode="json")
                runtime_connection["matrix_diagnostics"] = list(
                    matrix.diagnostics)
            if session is not None and adapter is not None:
                plan = getattr(adapter, "injection_plan", lambda _id: None)(
                    session.agent_session_id)
                runtime_connection.update({
                    "connected": bool(
                        session.external_session_id and session.closed_at is None),
                    "session_state": (
                        "closed" if session.closed_at is not None
                        else "active" if session.external_session_id
                        else "preparing"),
                    "injection_kind": (
                        plan.injection_kind.value if plan is not None else ""),
                    "gateway_endpoint": (
                        plan.gateway_endpoint if plan is not None else ""),
                    "degradation": (
                        "Agent Plugin is delivered through its skills-only component"
                        if plan is not None
                        and plan.injection_kind.value == "agent_plugin"
                        else "" if session.external_session_id
                        else "configuration exists but Runtime is not connected"),
                })
        fixture_snapshot = apply_cu_fixture_to_runtime_connection(
            thread.title or "", runtime_connection,
        )
        if fixture_snapshot is not None and callable(
            self._capability_snapshot_provider
        ):
            # Keep executor cache aligned so composer menus share revision.
            cache = getattr(self, "_capability_cache_writer", None)
            if callable(cache):
                cache(thread_id, fixture_snapshot)
        # #119: 已配置 Runtime 且尚未拿到会话目录时主动刷新，勿依赖命令菜单。
        # #188: 失败快照存在时也触发；真正是否重试由 executor 退避门闩决定，
        # 避免详情重复读取无限重启同一失败。成功但 stale 的目录不在此重刷，
        # 以免与 SSE capabilities_updated 形成循环。
        failure = None
        if callable(self._capability_failure_provider):
            try:
                failure = self._capability_failure_provider(thread_id)
            except Exception:  # noqa: BLE001 — 失败诊断缺失不阻断视图
                failure = None
        if isinstance(failure, dict) and failure:
            runtime_connection["capability_refresh_status"] = "failed"
            runtime_connection["capability_last_error"] = str(
                failure.get("error") or ""
            )
            runtime_connection["capability_refresh_attempts"] = int(
                failure.get("attempts") or 0
            )
            runtime_connection["capability_retry_after_seconds"] = float(
                failure.get("retry_after_seconds") or 0.0
            )
            if not runtime_connection.get("matrix_diagnostics"):
                runtime_connection["matrix_diagnostics"] = [
                    str(failure.get("error") or "Runtime 能力刷新失败"),
                ]
            elif str(failure.get("error") or "") not in runtime_connection[
                "matrix_diagnostics"
            ]:
                runtime_connection["matrix_diagnostics"] = [
                    str(failure.get("error") or "Runtime 能力刷新失败"),
                    *list(runtime_connection["matrix_diagnostics"]),
                ]
        elif snapshot is None:
            runtime_connection["capability_refresh_status"] = "missing"
        elif bool(getattr(snapshot, "stale", False)):
            runtime_connection["capability_refresh_status"] = "stale"
        else:
            runtime_connection["capability_refresh_status"] = "fresh"
        if (
            runtime_connection.get("configured")
            and callable(self._capability_refresh_trigger)
            and (snapshot is None or bool(failure))
        ):
            try:
                self._capability_refresh_trigger(thread_id)
            except Exception:  # noqa: BLE001 — 视图仍返回当前 stale 矩阵
                pass
        active_grants = [item for item in grants if item.revoked_at is None]
        if active_grants:
            latest_grant = max(active_grants, key=lambda item: item.issued_at)
            runtime_connection["grant_expires_at"] = (
                latest_grant.expires_at.isoformat()
                if latest_grant.expires_at else None)

        turns = self._conv.list_current_turns(thread_id)
        catalog_turns = self._conv.list_turns(thread_id)
        superseded_turns = [
            turn for turn in catalog_turns if turn.status == TURN_SUPERSEDED
        ]
        all_turns = turns
        active_turn_ids = {turn.turn_id for turn in all_turns}
        for turn in all_turns:
            if turn.status in (TURN_INTERRUPTED, TURN_FAILED):
                self._projection.ensure_assistant_from_deltas(
                    thread_id, turn.turn_id)

        page_limit = _parse_messages_limit(messages_limit)
        effective_limit = (
            page_limit
            if page_limit >= FULL_MESSAGES_HARD_CAP
            else min(page_limit, MAX_MESSAGES_PAGE_LIMIT)
        )
        before = (
            int(before_stream_seq) if before_stream_seq is not None else None
        )
        after = (
            int(after_stream_seq) if after_stream_seq is not None else None
        )
        page = self._conv.list_messages_page(
            thread_id,
            limit=effective_limit,
            before_stream_seq=before,
            after_stream_seq=after,
            current_branch_only=True,
        )
        messages = page["messages"]
        page_turn_ids = {
            msg.turn_id for msg in messages if msg.turn_id
        }
        running_id = state.running_turn_id
        visible_turns = [
            turn for turn in all_turns
            if turn.turn_id in page_turn_ids
            or (running_id and turn.turn_id == running_id)
        ]
        if not visible_turns and all_turns:
            visible_turns = all_turns[-min(len(all_turns), effective_limit):]

        artifact_rows: dict[str, dict[str, Any]] = {}
        head = self._store.stream_head("thread", thread_id)
        if include_events:
            events = self._store.read_events(
                "thread", thread_id, after_seq=0, limit=10_000)
        else:
            after_seq = max(0, head - SLIM_EVENT_SCAN_LIMIT)
            events = self._store.read_events(
                "thread", thread_id, after_seq=after_seq,
                limit=SLIM_EVENT_SCAN_LIMIT)
        current_events = []
        for event in events:
            event_turn_id = str(event.payload.get("turn_id") or "")
            if event_turn_id and event_turn_id not in active_turn_ids:
                continue
            current_events.append(event)
            if event.event_type not in (
                "core.artifact.attached", "core.artifact.created"
            ):
                continue
            payload = dict(event.payload)
            digest = str(payload.get("sha256") or "")
            if digest:
                artifact_rows[digest] = payload
        ledger_usage = self._projection.usage.query(thread_id=thread_id)["totals"]
        visible_usage = (
            ledger_usage if ledger_usage["records"] else dict(state.usage or {})
        )
        if include_events or head <= SLIM_EVENT_SCAN_LIMIT:
            statistics = _conversation_statistics(
                current_events, all_turns, visible_usage)
        else:
            statistics = _lightweight_statistics(all_turns, visible_usage)

        superseded_messages: list[dict[str, Any]] = []
        if include_superseded and superseded_turns:
            superseded_ids = {turn.turn_id for turn in superseded_turns}
            for message in self._conv.list_messages(thread_id):
                if message.turn_id and message.turn_id in superseded_ids:
                    superseded_messages.append(message.model_dump(mode="json"))

        payload = {
            "thread": thread.model_dump(mode="json"),
            "workspace": (
                workspace.model_dump(mode="json")
                if workspace is not None else None
            ),
            "runtime": selection.model_dump(mode="json"),
            "state": {
                **state.model_dump(mode="json"),
                "unread": state.unread,
                "usage": visible_usage,
            },
            "binding": binding.model_dump(mode="json") if binding else None,
            "grants": [grant.model_dump(mode="json") for grant in grants],
            "agent_session": (
                session.model_dump(mode="json") if session is not None else None
            ),
            "runtime_connection": runtime_connection,
            "artifacts": list(artifact_rows.values()),
            "messages": [m.model_dump(mode="json") for m in messages],
            "messages_page": _messages_page_meta(page),
            "turns": [t.model_dump(mode="json") for t in visible_turns],
            "queue": [
                item.model_dump(mode="json")
                for item in self._conv.list_queue(thread_id)
            ],
            "statistics": statistics,
            "runs": [
                r.model_dump(mode="json")
                for r in self._conv.list_current_runs(thread_id)
            ],
            "watermark": head,
            "event_watermark": self._store.event_watermark(),
            "rewind_capability": resolve_rewind_capability(runtime_connection),
            # C27: context-window fuel gauge — top-level for frontend convenience.
            "context_window": state.context_window,
        }
        if include_superseded:
            payload["superseded_turns"] = [
                t.model_dump(mode="json") for t in superseded_turns
            ]
            payload["superseded_messages"] = superseded_messages
        return payload

    def messages_page(
        self,
        thread_id: str,
        *,
        limit: Any = None,
        before_stream_seq: Optional[int] = None,
        after_stream_seq: Optional[int] = None,
        around_message_id: Optional[str] = None,
    ) -> dict[str, Any]:
        """Paged current-branch messages for history scroll / expand."""
        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        page_limit = _parse_messages_limit(
            limit if limit is not None else DEFAULT_MESSAGES_PAGE_LIMIT
        )
        effective_limit = min(page_limit, MAX_MESSAGES_PAGE_LIMIT)
        before = (
            int(before_stream_seq) if before_stream_seq is not None else None
        )
        after = (
            int(after_stream_seq) if after_stream_seq is not None else None
        )
        around_id = str(around_message_id or "").strip()
        if around_id:
            if before is not None or after is not None:
                raise ConversationError(
                    "around_message_id cannot combine with before/after cursors"
                )
            target_seq = self._conv.get_message_stream_seq(thread_id, around_id)
            if target_seq is None:
                raise ConversationError(f"unknown message: {around_id}")
            # Search hits may be superseded; current-branch pages omit them.
            # Include non-current turns for around windows so jump can land.
            branch_only = False
            # Load a window centered on the target: older half + newer half.
            older = self._conv.list_messages_page(
                thread_id,
                limit=max(1, effective_limit // 2),
                before_stream_seq=target_seq + 1,
                current_branch_only=branch_only,
            )
            newer = self._conv.list_messages_page(
                thread_id,
                limit=max(1, effective_limit - len(older["messages"])),
                after_stream_seq=target_seq,
                current_branch_only=branch_only,
            )
            by_id: dict[str, Any] = {}
            for msg in older["messages"] + newer["messages"]:
                by_id[msg.message_id] = msg
            # Guarantee the around target is present even if paging clipped it.
            if around_id not in by_id:
                target_msg = self._conv.get_message(around_id)
                if target_msg is not None and target_msg.thread_id == thread_id:
                    by_id[around_id] = target_msg
            merged = sorted(
                by_id.values(),
                key=lambda m: (m.stream_seq, m.message_id),
            )
            page = {
                "messages": merged,
                "messages_page": {
                    "limit": effective_limit,
                    "oldest_stream_seq": merged[0].stream_seq if merged else None,
                    "newest_stream_seq": merged[-1].stream_seq if merged else None,
                    "oldest_message_id": merged[0].message_id if merged else None,
                    "newest_message_id": merged[-1].message_id if merged else None,
                    "has_more_before": bool(
                        (older.get("messages_page") or {}).get("has_more_before")
                    ),
                    "has_more_after": bool(
                        (newer.get("messages_page") or {}).get("has_more_after")
                    ),
                    "around_message_id": around_id,
                },
            }
        else:
            page = self._conv.list_messages_page(
                thread_id,
                limit=effective_limit,
                before_stream_seq=before,
                after_stream_seq=after,
                current_branch_only=True,
            )
        messages = page["messages"]
        page_turn_ids = {msg.turn_id for msg in messages if msg.turn_id}
        # Around windows may include superseded turns; prefer all turns for the page.
        turn_source = (
            self._conv.list_turns(thread_id)
            if around_id
            else self._conv.list_current_turns(thread_id)
        )
        turns = [
            turn for turn in turn_source
            if turn.turn_id in page_turn_ids
        ]
        return {
            "messages": [m.model_dump(mode="json") for m in messages],
            "messages_page": _messages_page_meta(page),
            "turns": [t.model_dump(mode="json") for t in turns],
        }


    def search_messages(
        self,
        query: str,
        *,
        project_id: str = "",
        include_archived: bool = False,
        include_superseded: bool = False,
        limit: int = 30,
    ) -> dict[str, Any]:
        """Full-text search across Conversation message bodies (C07)."""
        q = str(query or "").strip()
        # Min length: 1 CJK or 2 ASCII (T3-aligned for Latin).
        if not q:
            return {"query": q, "count": 0, "hits": [], "reason": "query_too_short"}
        has_cjk = any("\u4e00" <= ch <= "\u9fff" for ch in q)
        if (has_cjk and len(q) < 1) or (not has_cjk and len(q) < 2):
            return {"query": q, "count": 0, "hits": [], "reason": "query_too_short"}

        threads = self.list_threads(project_id)
        visible_ids: list[str] = []
        thread_meta: dict[str, Any] = {}
        for thread in threads:
            state = self._conv.get_state(thread.thread_id)
            archived = state.status == "archived"
            if archived and not include_archived:
                continue
            visible_ids.append(thread.thread_id)
            thread_meta[thread.thread_id] = {
                "thread_title": thread.title or "",
                "project_id": thread.project_id or "",
                "archived": archived,
            }

        raw_hits = self._conv.search_messages(
            q,
            limit=max(1, min(int(limit), 100)),
            include_superseded=include_superseded,
            thread_ids=visible_ids,
        )
        hits: list[dict[str, Any]] = []
        for hit in raw_hits:
            meta = thread_meta.get(hit["thread_id"]) or {}
            hits.append({
                "thread_id": hit["thread_id"],
                "message_id": hit["message_id"],
                "turn_id": hit.get("turn_id"),
                "role": hit.get("role") or "user",
                "kind": "message",
                "stream_seq": hit.get("stream_seq") or 0,
                "thread_title": meta.get("thread_title") or "",
                "project_id": meta.get("project_id") or "",
                "archived": bool(meta.get("archived")),
                "superseded": bool(hit.get("superseded")),
                "snippet": hit.get("snippet") or "",
            })
        return {"query": q, "count": len(hits), "hits": hits}

    def turn_process(
        self,
        thread_id: str,
        turn_id: str,
        *,
        limit: int = TURN_PROCESS_EVENT_LIMIT,
    ) -> dict[str, Any]:
        """On-demand process/tool events for one turn (not full thread log)."""
        from muteki.platform.contracts.events import EventEnvelope

        thread = self.get_thread(thread_id)
        if thread is None:
            raise ConversationError(f"unknown thread: {thread_id}")
        turn = self._conv.get_turn(turn_id)
        if turn is None or turn.thread_id != thread_id:
            raise ConversationError(f"unknown turn: {turn_id}")
        if turn.status == TURN_SUPERSEDED:
            raise ConversationError(f"turn superseded: {turn_id}")
        fetch_limit = max(1, min(int(limit), TURN_PROCESS_EVENT_LIMIT))
        with self._store.lock:
            rows = self._store.conn.execute(
                "SELECT payload FROM domain_events "
                "WHERE aggregate_type = ? AND aggregate_id = ? "
                "AND json_extract(payload, '$.payload.turn_id') = ? "
                "ORDER BY stream_seq LIMIT ?",
                ("thread", thread_id, turn_id, fetch_limit),
            ).fetchall()
        events = [
            EventEnvelope.model_validate_json(row["payload"]).model_dump(
                mode="json"
            )
            for row in rows
        ]
        return {
            "thread_id": thread_id,
            "turn_id": turn_id,
            "turn": turn.model_dump(mode="json"),
            "events": events,
            "count": len(events),
            "truncated": len(events) >= fetch_limit,
        }



__all__ = [
    "ConversationError",
    "ConversationManager",
    "WORKSPACE_GIT",
    "WORKSPACE_ISOLATED",
    "WORKSPACE_LOCAL",
    "WS_MODE_EXISTING",
    "WS_MODE_NEW",
    "WS_MODE_SHARED",
]
