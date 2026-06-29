"""BTW side-query observer — worker-backed read-only Q&A over a run.

This module powers `POST /api/runs/{run_id}/btw`: the operator asks a quick
side question ("summarize progress", "which worker is on which line", ...) and
gets a streamed answer from a short-lived CLI worker that reads the run's durable
files directly.

The current default path is **worker-backed but swarm-isolated**:

  * starts the selected CLI agent through `run_cli_streaming`, not `CliSolver`;
  * never attaches a run bus, cost ledger, HITL queue, InsightBus, or shared graph
    writer, so the worker is not a swarm member;
  * points the worker at JSONL / shared_graph.db / winner.json / artifacts and
    asks it to inspect those files itself;
  * forbids blackboard writes and provenance markers (`FOUND_FLAG=`,
    `VERIFIED_FACT=`, etc.) in the prompt, so `/btw` cannot become a flag source.

The older flash-model helpers remain below for tests and compatibility: they
build a read-only context snapshot and LLM messages, but the web route no longer
uses them by default.

Multi-turn: the caller passes the accumulated `transcript`; we re-snapshot the
graph EVERY turn so a follow-up "and now?" reflects the latest blackboard. The
transcript lives only in the frontend component state + the request body — it
is never persisted server-side.

The system prompt forbids asserting flag candidates or inventing facts not in
the context: btw is an observer, never a flag source (the provenance gate is
unchanged and never sees btw output anyway).
"""

from __future__ import annotations

import asyncio
import inspect
import json
import threading
from collections.abc import AsyncIterator, Callable
from dataclasses import dataclass
from typing import Any, Optional

# Cheap/fast tier — same as summarizer.py's SUMMARY_MODEL. Hardcoded (not read
# from llm_profiles) so even if the operator configures the Reason planner to a
# pro model, btw stays on the cheap observation channel: predictable cost + no
# surprise spend on what is fundamentally a summarization/Q&A task.
BTW_MODEL = "deepseek-v4-flash"

_BTW_SYSTEM = """你是 Muteki swarm 的只读观察员。操作员问了一个"顺嘴"问题,你要基于当前
run 的真实状态回答。

你可以看到(由系统注入的 CONTEXT 块):
- challenge 描述与类别
- Run 维度统计: 运行时长、总花费(USD)、总 tokens(输入/输出)、按 solver 拆分的花费、worker 名单与状态、活动强度
- shared_graph 的当前快照(facts / open intents / dead-ends / 候选证据 / 误报 flags)
- 最近事件时间线

当操作员问"用了多久/花了多少钱/多少 token/哪些 worker 跑过"时,从"Run 维度统计"块回答。
当操作员问"进展/线索/dead-end"时,从 shared_graph 快照回答。

绝对禁止:
- 你不能执行任何动作,不能建议 spawn/pause/redirect worker
- 你不能产出 flag 候选(即使你在上下文里看到疑似 flag 字符串,只能引用不能断言)
- 你不能编造没有出现在上下文里的事实

回答风格: 简洁、中文、直接。如果信息不足,直说"blackboard 里目前没有 X"或"统计块里没有 X"。
"""

# Rough per-message char cap before we even send — keeps a runaway transcript
# from blowing the flash model's window. The frontend truncates too; this is a
# server-side second line of defense.
_MAX_TRANSCRIPT_TURNS = 24
_MAX_TRANSCRIPT_CHARS = 60000
_MAX_QUESTION_CHARS = 2000


@dataclass(frozen=True)
class BtwWorkerPaths:
    """Paths that the `/btw` worker should inspect.

    These are already mapped for the worker's execution environment: host paths
    for local workers, container paths for container workers.
    """

    workspace: str
    jsonl: str
    graph_db: str
    board: str
    winner: str
    arts: str
    uploads: str = ""


_BTWWORKER_RULES = """你是 Muteki `/btw` 的只读旁路 worker。你不是正在解题的 swarm worker,
也不是 coordinator。你的唯一任务是回答操作员这个 side question。

工作方法:
- 直接读取下面列出的 run 文件、SQLite graph、JSONL event log、artifacts。
- 如果要总结解题路径,优先使用 `flag_found` / `verified` evidence / worker 实际输出。
- 明确区分 `intent_proposed`、open intent、dead_end、operator hint、候选证据、误报 flag。
- 不要把计划、猜测、失败方向写成已经完成的步骤。
- 信息不足就说不足;不要补剧情。

绝对禁止:
- 不要调用或写入 blackboard skill / blackboard.py / shared_graph 写接口。
- 不要修改 `shared_graph.db`、JSONL、winner.json、artifacts 或 run workspace 里的共享状态。
- 只允许在当前 cwd 下写临时 scratch 文件。
- 不要继续攻击目标、不要提交 flag、不要 spawn/kill/pause/redirect worker。
- 不要输出 `FOUND_FLAG=`, `VERIFIED_FACT=`, `BB_FACT=`, `BB_INTENT=`, `HITL_REQUEST=`
  等任何 solver 协议标记。

回答风格:
- 用中文,简洁直接。
- 若回答"整体解题路径",按已验证事实给出,并标出哪些只是尝试/失败方向。
"""


def _fmt_transcript(transcript: list[dict[str, str]]) -> str:
    if not transcript:
        return "(无历史 /btw 对话)"
    lines: list[str] = []
    for t in transcript[-12:]:
        role = "操作员" if t.get("role") == "user" else "btw"
        content = (t.get("content") or "").strip()
        if content:
            lines.append(f"{role}: {content[:2000]}")
    return "\n".join(lines) if lines else "(无历史 /btw 对话)"


def build_btw_worker_prompt(
    *,
    question: str,
    paths: BtwWorkerPaths,
    challenge_id: str,
    challenge_name: str,
    challenge_category: str,
    run_state: str = "",
    context_hint: Optional[str] = "",
    transcript: Optional[list[dict[str, str]]] = None,
) -> str:
    """Prompt for the short-lived `/btw` worker.

    The prompt deliberately points at durable run artifacts instead of injecting
    a lossy graph summary. This is the main anti-distortion change: the worker can
    query SQLite/JSONL directly and decide which events are evidence versus leads.
    """
    q = (question or "").strip()[:_MAX_QUESTION_CHARS]
    hint = (context_hint or "").strip()[:400]
    transcript = sanitize_transcript(transcript or [])

    path_lines = [
        f"- workspace: {paths.workspace}",
        f"- event log JSONL: {paths.jsonl}",
        f"- shared graph SQLite DB: {paths.graph_db}",
        f"- markdown board: {paths.board}",
        f"- winner snapshot: {paths.winner}",
        f"- artifacts dir: {paths.arts}",
    ]
    if paths.uploads:
        path_lines.append(f"- uploaded challenge files: {paths.uploads}")

    meta_lines = [
        f"- run_id / challenge_id: {challenge_id}",
        f"- name: {challenge_name or challenge_id}",
        f"- category: {challenge_category or 'web'}",
    ]
    if run_state:
        meta_lines.append(f"- run state: {run_state}")
    if hint:
        meta_lines.append(f"- operator focus: {hint}")

    return "\n\n".join([
        _BTWWORKER_RULES,
        "=== Run metadata ===\n" + "\n".join(meta_lines),
        "=== Files you may inspect ===\n" + "\n".join(path_lines),
        "=== Previous /btw transcript ===\n" + _fmt_transcript(transcript),
        "=== Current question ===\n" + q,
        "请现在读取必要文件后回答。最终只输出给操作员看的答案,不要输出协议标记。",
    ])


async def _maybe_disconnected(fn: Callable[[], Any] | None) -> bool:
    if fn is None:
        return False
    try:
        v = fn()
        if inspect.isawaitable(v):
            v = await v
        return bool(v)
    except Exception:
        return False


async def stream_btw_worker_deltas(
    *,
    driver: Any,
    prompt: str,
    cwd: str,
    timeout: int,
    env: Optional[dict[str, str]] = None,
    container: Any = None,
    web_access: bool = False,
    kb_access: bool = False,
    request_disconnected: Callable[[], Any] | None = None,
) -> AsyncIterator[str]:
    """Run one CLI worker turn and yield assistant text chunks for `/btw`.

    This is intentionally lower-level than `CliSolver`: it has no EventBus, no
    CostController, no SharedGraph writer, and no flag provenance gate. The worker
    subprocess may use its normal tools to inspect files, but only assistant prose
    is forwarded to the HTTP caller.
    """
    from muteki.solver.cli_driver import run_cli_streaming

    loop = asyncio.get_running_loop()
    queue: "asyncio.Queue[object]" = asyncio.Queue()
    done = object()
    cancel_event = threading.Event()
    streamed = {"count": 0}
    result_holder: list[Any] = []
    error_holder: list[BaseException] = []

    def _put(item: object) -> None:
        try:
            loop.call_soon_threadsafe(queue.put_nowait, item)
        except RuntimeError:
            pass

    def _on_step(step: Any) -> None:
        if getattr(step, "kind", "") != "reasoning":
            return
        text = str(getattr(step, "text", "") or "").strip()
        if not text:
            return
        streamed["count"] += 1
        _put(text)

    def _run() -> None:
        try:
            session = driver.new_session()
            argv = driver.build_execute(
                prompt,
                session,
                web_access=web_access,
                kb_access=kb_access,
                stream=True,
            )
            res = run_cli_streaming(
                driver,
                argv,
                cwd=cwd,
                timeout=timeout,
                on_step=_on_step,
                env=env,
                cancel_event=cancel_event,
                container=container,
            )
            result_holder.append(res)
        except BaseException as exc:  # noqa: BLE001
            error_holder.append(exc)
        finally:
            _put(done)

    task = asyncio.create_task(asyncio.to_thread(_run))
    try:
        while True:
            if await _maybe_disconnected(request_disconnected):
                cancel_event.set()
                break
            try:
                item = await asyncio.wait_for(queue.get(), timeout=0.25)
            except asyncio.TimeoutError:
                if task.done() and queue.empty():
                    break
                continue
            if item is done:
                break
            yield str(item)
    except asyncio.CancelledError:
        cancel_event.set()
        raise
    finally:
        if cancel_event.is_set() and not task.done():
            try:
                await asyncio.wait_for(task, timeout=2.0)
            except asyncio.TimeoutError:
                pass

    if not task.done():
        await task
    if error_holder:
        raise error_holder[0]
    if not result_holder:
        return
    res = result_holder[0]
    if getattr(res, "cancelled", False):
        return
    if getattr(res, "timed_out", False):
        raise TimeoutError(f"btw worker timed out after {timeout}s")
    final_text = str(getattr(res, "text", "") or "").strip()
    if streamed["count"] == 0 and final_text:
        yield final_text


def sanitize_transcript(transcript: Any) -> list[dict[str, str]]:
    """Validate + cap the caller-supplied multi-turn transcript.

    Returns a clean list of {role, content} dicts. Drops anything that isn't a
    user/assistant turn, caps total turns and chars, and trims over-long single
    messages. Never raises — a malformed transcript degrades to [] so the turn
    still works as a one-shot.
    """
    if not isinstance(transcript, list):
        return []
    out: list[dict[str, str]] = []
    total = 0
    for t in transcript:
        if not isinstance(t, dict):
            continue
        role = str(t.get("role") or "").lower()
        if role not in ("user", "assistant"):
            continue
        content = str(t.get("content") or "")
        if not content:
            continue
        if len(content) > _MAX_TRANSCRIPT_CHARS // 4:
            content = content[: _MAX_TRANSCRIPT_CHARS // 4] + "…"
        out.append({"role": role, "content": content})
        total += len(content)
        if len(out) >= _MAX_TRANSCRIPT_TURNS or total >= _MAX_TRANSCRIPT_CHARS:
            break
    # keep the first user turn (anchors the conversation) then the most recent
    # turns — drop from the middle if we capped.
    if len(out) > _MAX_TRANSCRIPT_TURNS:
        head = out[:1]
        tail = out[-(_MAX_TRANSCRIPT_TURNS - 1):]
        out = head + tail
    return out


def btw_messages(
    question: str,
    ctx_text: str,
    transcript: list[dict[str, str]],
    context_hint: Optional[str] = "",
) -> list[dict[str, Any]]:
    """Assemble the LLM messages: system(+ctx) + transcript + this turn.

    `ctx_text` is the read-only graph snapshot assembled by
    `build_btw_context_sync` — it is refreshed every turn so a follow-up
    "and now?" sees the latest blackboard, not the first turn's stale view.
    """
    sys_content = _BTW_SYSTEM
    if context_hint:
        sys_content += f"\n操作员关注点: {context_hint[:400]}\n"
    sys_content += "\n=== 当前 run 状态快照 ===\n"
    sys_content += ctx_text or "(尚无 blackboard 数据 / graph 不可读)"
    sys_content += "\n=== 快照结束 ===\n"

    msgs: list[dict[str, Any]] = [{"role": "system", "content": sys_content}]
    msgs.extend(transcript)
    msgs.append({"role": "user", "content": question[:_MAX_QUESTION_CHARS]})
    return msgs


def _human_duration(seconds: float) -> str:
    """Render seconds as a Chinese human-readable duration (e.g. '12分34秒' / '2时05分')."""
    s = max(0, int(seconds))
    if s < 60:
        return f"{s}秒"
    m, sec = divmod(s, 60)
    if m < 60:
        return f"{m}分{sec:02d}秒"
    h, m = divmod(m, 60)
    return f"{h}时{m:02d}分"


def build_btw_run_stats_sync(jsonl_path: str) -> str:
    """Synchronous, read-only scan of a run's JSONL event log to aggregate
    run-level statistics the graph doesn't hold: timing, cost, worker roster,
    event-type counts.

    This is the ONLY reliable source for cost/timing on a historical run (after
    a server restart the in-memory `run.cost` CostController is freshly empty —
    it does NOT rehydrate from JSONL). The JSONL is the persistent source of
    truth, so we read it directly: a single sequential pass, O(n), no locks
    (append-only file, OS sequential read never blocks the writer).

    Returns a text block for the btw system prompt. Never raises — a missing /
    unreadable / malformed JSONL degrades to an empty string (the rest of the
    context still renders).
    """
    import json as _json
    import os as _os

    if not _os.path.exists(jsonl_path):
        return ""

    started_ts: float | None = None
    finished_ts: float | None = None
    last_ts: float = 0.0
    # cost aggregation
    total_usd = 0.0
    total_tokens = 0
    total_in = 0
    total_out = 0
    by_solver: dict[str, dict[str, float]] = {}
    # worker roster
    workers: dict[str, dict[str, Any]] = {}
    # event-type counts
    type_counts: dict[str, int] = {}

    try:
        with open(jsonl_path, "r", encoding="utf-8") as f:
            for raw in f:
                raw = raw.strip()
                if not raw:
                    continue
                try:
                    ev = _json.loads(raw)
                except _json.JSONDecodeError:
                    continue
                et = ev.get("event_type") or ev.get("type") or ""
                ts = float(ev.get("ts") or 0.0)
                if ts:
                    last_ts = max(last_ts, ts)
                type_counts[et] = type_counts.get(et, 0) + 1
                p = ev.get("payload") or {}
                if not isinstance(p, dict):
                    p = {}
                sid = ev.get("solver_id") or ""

                if et == "run.started" and started_ts is None:
                    started_ts = ts
                elif et == "run.finished":
                    finished_ts = ts
                elif et == "cost.update":
                    total_usd += float(p.get("usd") or 0.0)
                    total_tokens += int(p.get("tokens") or 0)
                    total_in += int(p.get("input_tokens") or 0)
                    total_out += int(p.get("output_tokens") or 0)
                    if sid:
                        slot = by_solver.setdefault(sid, {"usd": 0.0, "tokens": 0})
                        slot["usd"] += float(p.get("usd") or 0.0)
                        slot["tokens"] += int(p.get("tokens") or 0)
                elif et == "worker.status":
                    wid = sid or ""
                    if wid:
                        w = workers.setdefault(wid, {"engine": "", "online": False,
                                                     "status": "", "reason": ""})
                        w["online"] = bool(p.get("online"))
                        w["engine"] = p.get("engine") or w["engine"]
                        w["status"] = p.get("status") or w["status"]
                        w["reason"] = p.get("reason") or w["reason"]
                elif et == "worker.lifecycle":
                    wid = sid or ""
                    phase = p.get("phase") or ""
                    if wid and phase == "spawned":
                        w = workers.setdefault(wid, {"engine": "", "online": True,
                                                     "status": "spawned", "reason": ""})
                        w["engine"] = w["engine"] or ""
                elif et == "worker.finished":
                    wid = sid or ""
                    if wid:
                        w = workers.setdefault(wid, {"engine": "", "online": False,
                                                     "status": "finished", "reason": ""})
                        w["online"] = False
                        w["status"] = "finished"
                        w["reason"] = p.get("reason") or w["reason"]
                        if p.get("flag"):
                            w["found_flag"] = True
    except Exception:
        return ""

    parts: list[str] = []
    # ── timing ──
    if started_ts:
        end_ts = finished_ts if finished_ts else last_ts
        dur = max(0.0, end_ts - started_ts)
        parts.append(f"运行时长: {_human_duration(dur)}"
                     + ("" if finished_ts else " (仍在运行)"))
        parts.append(f"开始时间: {started_ts:.0f}")
        if finished_ts:
            parts.append(f"结束时间: {finished_ts:.0f}")
    else:
        parts.append("运行时长: 尚未启动")

    # ── cost ──
    parts.append(f"总花费: ${total_usd:.4f}")
    parts.append(f"总 tokens: {total_tokens} (输入 {total_in} / 输出 {total_out})")
    if by_solver:
        parts.append("按 solver 拆分:")
        for sid, slot in sorted(by_solver.items(), key=lambda kv: -kv[1]["usd"]):
            parts.append(f"  - {sid}: ${slot['usd']:.4f}, {int(slot['tokens'])} tokens")

    # ── workers ──
    if workers:
        parts.append(f"Worker 总数: {len(workers)}")
        for wid, w in list(workers.items())[:12]:
            flag_mark = " ★找到flag" if w.get("found_flag") else ""
            parts.append(
                f"  - {wid} [{w.get('engine','?')}] "
                f"状态={w.get('status','?')}{'(在线)' if w.get('online') else '(已退出)'}"
                f"{flag_mark}"
            )

    # ── event activity ──
    # surface a few high-signal counts so the model can describe activity intensity
    hot = [(et, c) for et, c in type_counts.items()
           if et in ("text.delta", "tool.start", "tool.result", "reason.intent",
                     "blackboard.delta", "sharedgraph.delta", "cost.update")]
    if hot:
        parts.append("活动强度:")
        for et, c in sorted(hot, key=lambda kv: -kv[1]):
            parts.append(f"  - {et}: {c}")

    return "\n".join(parts)


def build_btw_context_sync(
    *,
    graph_db_path: str,
    challenge_id: str,
    challenge_name: str,
    challenge_category: str,
    run_meta: dict[str, Any],
    context_hint: Optional[str] = "",
    jsonl_path: str = "",
) -> str:
    """Synchronous, read-only graph snapshot for the btw system prompt.

    Runs inside `asyncio.to_thread` from the route so it never blocks the
    uvicorn event loop. Opens the graph TRUE read-only; on ANY SQLite
    operational error (missing file, stale schema, WAL sidecar absent on a
    live run) it degrades to a minimal context — never attempts migration and
    never opens read-write.

    `run_meta` carries the live run's worker/status fields (from the Run
    object) so we can describe worker state without reading worker stdout.
    """
    from muteki.models.solve_graph import Challenge
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    parts: list[str] = []
    parts.append(f"Challenge: {challenge_name or challenge_id}")
    parts.append(f"类别: {challenge_category or '未知'}")
    parts.append(f"Run状态: {run_meta.get('state', '未知')}")
    if run_meta.get("solved"):
        parts.append("已解出 flag")
    if run_meta.get("flags"):
        parts.append(f"已收集 flags 数: {len(run_meta['flags'])}")
    if run_meta.get("awaiting_help"):
        parts.append(f"有 worker 举手求助: {run_meta.get('help_text', '')[:200]}")
    workers = run_meta.get("workers") or []
    if workers:
        parts.append("Workers:")
        for w in workers[:12]:
            parts.append(
                f"  - {w.get('id','?')} [{w.get('engine','?')}] "
                f"状态={w.get('state','?')}"
            )

    # Run-level statistics from the JSONL event log (timing / cost / worker
    # roster / activity). This is the ONLY reliable source for cost+timing on
    # a historical run — the in-memory CostController is empty after a server
    # restart, but the JSONL is persistent. Read-only single pass.
    if jsonl_path:
        try:
            stats = build_btw_run_stats_sync(jsonl_path)
            if stats:
                parts.append("=== Run 维度统计 (时长/花费/worker/活动) ===")
                parts.append(stats)
        except Exception:
            pass

    import os
    if not os.path.exists(graph_db_path):
        parts.append("Blackboard: 尚未建立 (run 刚启动或未进入协调阶段)")
        return "\n".join(parts)

    challenge = Challenge(
        id=challenge_id,
        name=challenge_name,
        category=challenge_category or "web",
    )
    graph = None
    try:
        graph = SQLiteSharedGraph.open_readonly(
            db_path=graph_db_path, challenge=challenge,
        )
        try:
            summary = graph.to_summary(max_evidence=24, max_dead_ends=8)
            if summary and summary.strip():
                parts.append("=== Facts / Dead-ends 摘要 ===")
                parts.append(summary)
        except Exception:
            parts.append("(facts 摘要读取失败 — 可能 schema 过旧)")
        try:
            opens = graph.open_goal_texts()
            if opens:
                parts.append("=== Open Intents ===")
                for g in opens[:10]:
                    parts.append(f"  - {g[:200]}")
        except Exception:
            pass
        try:
            barren = graph.barren_concluded_goal_texts()
            if barren:
                parts.append("=== 已尝试但无产出的方向 (dead-ends) ===")
                for g in barren[:8]:
                    parts.append(f"  - {g[:200]}")
        except Exception:
            pass
        try:
            cands = graph.active_candidates()
            if cands:
                parts.append(f"=== 候选证据 (未验证) : {len(cands)} 条 ===")
                for c in cands[:8]:
                    f = (c.get("fact") or "")[:160]
                    parts.append(f"  - {f}")
        except Exception:
            pass
        try:
            verified = graph.verified_evidence()
            if verified:
                parts.append(f"=== 已验证证据 : {len(verified)} 条 ===")
                for v in verified[:6]:
                    f = (v.get("fact") or "")[:160]
                    parts.append(f"  - {f}")
        except Exception:
            pass
        try:
            invalid = graph.invalidated_flags()
            if invalid:
                parts.append(f"=== 被标误报的 flags : {len(invalid)} ===")
        except Exception:
            pass
        try:
            recent = graph.recent_events(limit=40)
            if recent:
                parts.append("=== 最近事件时间线 (最近 40 条,精简) ===")
                for e in recent[-30:]:
                    kind = e.get("kind", "?")
                    actor = e.get("actor", "?")
                    p = e.get("payload") or {}
                    tag = ""
                    if isinstance(p, dict):
                        tag = str(p.get("fact") or p.get("goal") or
                                  p.get("text") or p.get("summary") or "")[:120]
                    parts.append(f"  [{e.get('seq','?')}] {kind} by {actor}: {tag}")
        except Exception:
            pass
    except Exception:
        # open_readonly failed (missing WAL sidecar on a live run, stale schema,
        # etc.) — degrade, do NOT fall back to read-write.
        parts.append("Blackboard: 暂时不可读 (graph 正在被写入或 schema 不兼容)")
    finally:
        if graph is not None:
            try:
                graph.close()
            except Exception:
                pass
    return "\n".join(parts)


def run_meta_dict(run: Any) -> dict[str, Any]:
    """Pull the lightweight status fields off a Run object for the context.

    Reads ONLY the metadata surface (name/category/flags/workers) — never the
    bus, never the event stream, never worker stdout. The worker list comes
    from the deck's already-derived view if available; we don't re-derive.
    """
    if run is None:
        return {"state": "unknown"}
    state = "finished" if getattr(run, "finished", False) else (
        "running" if getattr(run, "task", None) is not None and
        not run.task.done() else "idle"
    )
    return {
        "state": state,
        "solved": bool(getattr(run, "solved", False)),
        "flags": list(getattr(run, "flags", []) or []),
        "awaiting_help": bool(getattr(run, "awaiting_help", False)),
        "help_text": str(getattr(run, "help_text", "") or ""),
        "workers": [],  # the route fills this from the deck if available
    }


# ── per-run limiter ──────────────────────────────────────────────────────────

class BtwLimiter:
    """Per-run single-slot limiter + active-stream cancellation.

    Policy: each run gets at most ONE active btw stream. A new request for the
    same run CANCELS the previous stream's generation (the caller's
    `iter_chat_deltas` loop observes CancelledError / the disconnected client
    stops yielding). We do NOT queue — a queue would make the second request
    wait for the first, which is worse UX than cancel-and-replace.

    The cancellation is cooperative: we store the active task and cancel it;
    the route's `finally` releases the slot. A client disconnect (Esc / close
    / run switch) also frees the slot via the same finally path.

    This object is intentionally not tied to any run's bus/cost — it is a
    standalone observer-side bookkeeping dict on app.state.
    """

    def __init__(self) -> None:
        self._active: dict[str, "asyncio.Task[Any]"] = {}

    def acquire(self, run_id: str, new_task: "asyncio.Task[Any]") -> "asyncio.Task[Any] | None":
        """Register `new_task` as the active btw for `run_id`.

        Returns the previous task (now cancelled) if one was active, else None.
        The caller (route) is responsible for awaiting/cleanup of the returned
        task — we just cancel it here.
        """
        prev = self._active.get(run_id)
        if prev is not None and not prev.done():
            prev.cancel()
        self._active[run_id] = new_task
        return prev

    def release(self, run_id: str, task: "asyncio.Task[Any]") -> None:
        """Free the slot iff `task` is still the one we registered.

        Idempotent + safe against a late release after a replacement already
        took the slot.
        """
        cur = self._active.get(run_id)
        if cur is task:
            self._active.pop(run_id, None)


def sse_frame(obj: dict[str, Any]) -> str:
    return f"data: {json.dumps(obj, ensure_ascii=False)}\n\n"
