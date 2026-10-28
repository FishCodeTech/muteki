"""Minimal Pi-backed Reason adapter for CTF runs."""

from __future__ import annotations

import asyncio
import hashlib
import json
import math
import os
import signal
import sys
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Mapping

if TYPE_CHECKING:
    from muteki.core.cost import CostController
    from muteki.solver.cli_engines.types import CliResult

from muteki.solver.cli_engines.engines.pi import PiDriver
from muteki.solver.cli_engines.argv import apply_reasoning_effort
from muteki.solver.cli_launch_check import check_process_launch
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.solver.ctf_fgs import (
    closed_reopen_rows,
)
from muteki.solver.reason import (
    Intent,
    PlannerFailure,
    PlannerFailureKind,
    ReasonDiagnostics,
    ReasonResult,
    VERDICT_COMPLETE,
    VERDICT_EXPLORE,
)


CTF_PI_DECIDE_SYSTEM = """在 Fact-Goal-Step 图上做面向目标的判断,不做任何执行。

下方 user 消息里是整张图的 YAML:facts 是已确认的客观事实,steps 是从若干事实出发的探索方向,goals 是达成标准;图总是从事实经由 step 产出新事实。先读懂全图、把握整体进展,再判断。

判断两件事:
1. 已有事实是否已满足目标。满足就 satisfy_goal 置达成,引用支撑事实的 id,并在 reason 写清为何这些事实足以证明。
2. 未满足则据当前事实决定下一步。只规划眼前这一步,简单的通过 Step 铺开方向去并行,后续随新事实每轮再定,不预先铺开整条路线。

step:
- step 是即时的一步动作:action 一句话点明方向,不写分步方法,不复述 origin 或 goal 已有的信息。它是眼下要取的那条信息,不是一份剧本。

纪律:
- 目标分两类,判定方式不同:能出示见证的(拿到 flag、找到那个漏洞),见证摆在图上即达成;只能论证覆盖的(全面排查、普查),看 sub 覆盖是否齐、是否还有未探线索,不要去等一个能"证明穷尽"的产物。
- 只依据图中事实判断,不臆造。只针对 fact_origin 中 container_addr 声明的远程目标服务规划 step(不扫描同网段其他邻近题目 IP,不规划盘点评测容器本机 /proc、/var/lib/muteki-hosted、/opt 或本机控制面;注意 502 也可能是题目本身的反代/路由设计而非环境故障,需先排查确认题目环境真的挂死后才可规划调用 restart_target 重建靶机)。
- 无 open step 时(冷启动或管道空转),开多条相邻但互补的侦察方向快速起量;已有事实可据分化时,各 step 覆盖不同维度、不重叠。
- 方向失效(已被新事实解决或前提被证伪)用 drop_step 并说明原因;对在途 step 拿不准实时进展时可调用 peek_step 查看该 worker 的当前上下文与工作情况,不要仅为了换个更细的措辞而把正在正常推进的 step 删掉重开。
- 汇总/报告类 step(把多个事实并成一份交付物)放到最后:仅当没有其他实质探索方向在途或待开时才开,避免边探边反复重写同一份产物。
- 可以质疑图上事实的可靠性,可以重新规划 step 重新尝试某些疑点。

图操作先记入草稿,最后必须调用 commit 一次性提交(不调用则本轮作废)。操作简单、合法性一眼可辨时直接 commit;仅当批次含判达成、退回或跨多事实等可能被剔除的操作时,先用 preview 复核叠加草稿后的投影(只出 step/goal 结构与事实标题,校验不过的条目会标注、提交时剔除)。"""

PENTEST_PI_DECIDE_SYSTEM = """在 Fact-Goal-Step 图上做面向目标的判断,不做任何执行。

下方 user 消息里是整张图的 YAML：facts 是已确认的客观事实，observations 是未获 Fact 身份的原始记录，deadEnds 是有边界的排除，resources 是已登记的文件资源，steps 是探索方向，goals 是达成标准。Observation 不能作为 from 引用；先读懂全图和结果，再判断。

判断两件事:
1. 可用 add_sub_goal(criterion) 记录有用的中间目标，以 satisfy_goal(goal_id='goal_001', reason=..., from=[...]) 标记它完成；只有默认 goal_final 的完成代表用户最终目标达成。
已有事实是否已满足目标。满足就 satisfy_goal 置达成,引用支撑事实的 id,并在 reason 写清为何这些事实足以证明。
2. 未满足则据当前事实决定下一步。只规划眼前这一步,简单的通过 Step 铺开方向去并行,后续随新事实每轮再定,不预先铺开整条路线。

step:
- action 一句话点明眼下要取的信息，不写分步剧本，不复述 origin/goal 已有的背景；from 只引用真实 Fact id。
- 后继必须使用某个已登记 PoC 的精确版本时，在 requires 写该 Resource id；普通方向省略 requires。
- 每个 Step 写 expected_observable、stop_condition 和稳定的 coverage_key；同一范围与假设沿用 coverage_key。
- 方向失效用 drop_step 并说明原因。
- 当图中某条已有 Fact 已被后续实验证伪、属于假阳性/诱饵或已被更准确的新 Fact 取代时,调用 supersede_fact(fact_id='fact_xxx', reason='...', by_fact_id='fact_yyy') 将其淘汰。

图操作先记入草稿,最后必须调用 commit 一次性提交(不调用则本轮作废)。操作简单、合法性一眼可辨时直接 commit;仅当批次含判达成、退回或跨多事实等可能被剔除的操作时,先用 preview 复核叠加草稿后的投影(只出 step/goal 结构与事实标题,校验不过的条目会标注、提交时剔除)。

本任务的授权测试边界见 engagement。公开资料可以帮助推理，但不能替代目标环境的工具证据。你负责判断 Fact 是否已经满足用户原始测试目标；满足时用 satisfy_goal 的 from 引用支撑结论的 Fact，并在 reason 解释。若证据不足，继续规划当前最有信息增益的 Step。是否需要复核、复测或扩大授权范围内的探索，由你依据事实决定。

规划 Pentest Step 时，一条 Step 聚焦一个可独立核验的证据缺口；若指向潜在漏洞，让预期证据足以支持或排除一份独立漏洞报告。避免在一条 Step 中混合多个不同漏洞假设，也不把同一成因拆成多份报告。登录、会话等共用前置证据取得后，通过共享图中的 Fact 及其 sourceArtifacts 复用；后续 Step 的 from 引用相关 Fact，必须精确复用已登记 PoC 时才在 requires 引用 Resource。仅当证据不足、失效或目标上下文变化时再复核这些前置条件。Step 的具体划分和并行方向仍由你根据当前事实、信息增益与授权边界决定。

终局报告另由模型从共享图生成，不需要为撰写报告创建 Worker Step。"""
PENTEST_PI_DECIDE_SYSTEM += "\n当 engagement.reportGoalMode=count 时，宿主依据有证据且经过审阅的独立报告数量停止；不要用 satisfy_goal 抢先结束。继续规划有信息增益的漏洞验证 Step，直到 reportsAccepted 达到 reportsExpected。报告由 Worker 随每个 Step 实时提交；审阅中的报告仍可在共享图看到。"
PENTEST_PI_DECIDE_SYSTEM += "\nPentest 不支持 yield_visit；Goal 未满足且没有在途 Step 时，应提交可执行的授权范围内验证 Step。"

_TOOLS = "read,add_sub_goal,open_step,peek_step,drop_step,change_step_priority,supersede_fact,satisfy_goal,preview,commit"
_CHALLENGER_TOOLS = "read,peek_step,supersede_fact,open_step,satisfy_goal,preview,commit"
_CHALLENGER_PLAN_OPS = frozenset({
    "drop_step",
    "change_step_priority",
    "add_sub_goal",
})
_REPO_ROOT = Path(__file__).resolve().parents[2]
_EXTENSION = Path(__file__).with_name("pi_decide_extension.ts")

CTF_PI_CHALLENGER_SYSTEM = """你是 Fact-Goal-Step 图的盲点破局官（Blindspot Challenger）,在图上做破局审计与规划,不直接对靶机发包执行。

当前团队已连续完成多个探索 Step 仍未取得目标 Flag,说明图中极可能存在至少一条核心 Fact 把未证实的“黑盒脑补假说”或“超范围局部阴性结论”写成了既定事实,锁死了后续探索方向。

工作流程:
1. 若已有事实已满足目标(已取得有效 Flag),调 satisfy_goal 置达成并 commit。
2. 否则,先审视全图中阻碍核心高价值攻击面推进的关键 Fact(尤其是给某个入口的后端语言/模板引擎/AST或语法白名单实现下定论,或声称某个请求头/参数/方法全局无效的 Fact)。 Only target the remote service declared in fact_origin.container_addr (never scan neighbor challenge IPs or local host/platform control plane).
3. 必须先调用 `peek_step(intent_id)`(可传 `step_xxx` 或直接传 `fact_xxx`)查看产出这些关键 Fact 的**已结案(done)或在途 Step 的真实执行命令与输出**(必要时也可用 `read` 读取 `shared/` 下的测试脚本或结果文件),重点核对:
   - Worker 真的读到了后端源码/配置吗?还是仅凭黑盒回显脑补了后端内部语言/模板引擎/解析机制?
   - Worker 的实测输入输出是否与其声称的机制自相矛盾(例如声称是某语言,但基础算术/类型/布尔/字面量求值结果不符合该语言标准语义)?
   - Worker 测试所用的样本或判别器是否局限在单一语言语法或单一靶点(如误把跨语言通用的 `${...}` 插值当成某一种特定语言,或仅在对某请求头/方法无区分力的靶点上做了测试)?
4. 定位被错误假说或片面测试锁死的盲点后:
   - 对包含未证实黑盒定性或超范围否定结论的 Fact 调用 `supersede_fact(fact_id='fact_xxx', reason='...')` 将其修正/淘汰(在 reason 中写清其假说矛盾与测试盲区,同时保留其中仍有效的实测现象供新 Step 参考);
   - 调用 `open_step(action=..., from=[...], priority='high')` 开启 1~2 条跳出原假说框架的异构验证 Step(action 一句话直指被前序测试遗漏的真实假设维度与验证方向);
   - 最后必须调用 `commit(summary=...)` 一次性提交本轮全部破局图操作。"""


PENTEST_PI_CHALLENGER_SYSTEM = """你是授权渗透测试 Fact-Goal-Step 图的盲点破局者。审查当前图与真实执行证据，判断是否有未经验证的假设、过宽的阴性结论或遗漏的验证方向；不要预设已有 Fact 一定错误。
用 peek_step 查看相关在途或已完成 Step，也可按 Fact id 追溯原始工具命令与输出，用 read 读取相关共享产物。证据确实推翻或修正已有 Fact 时用 supersede_fact，并在 reason 中保留仍然成立的观察和适用范围；需要新验证时用 open_step 创建当前可执行、互补的方向。不要取消或重排其他在途 Step。
本任务以 engagement 中的用户目标、授权范围及报告契约为准，不以 Flag 为目标。每条新 Step 写 expected_observable、stop_condition 和 coverage_key，引用已有 Fact；不得用公开资料替代目标环境证据。已有证据满足目标时可用 satisfy_goal 并引用依据；reportGoalMode=count 时保留宿主按已审阅报告数量判定完成的规则。报告仍由 Worker 提交，不另开报告写作 Step。
所有图操作写入草稿，最后 commit。没有证据支持修订或新工作时可以提交空操作，不制造事实或任务。"""


def _pi_system_prompt(*, mode: str, challenger: bool) -> str:
    if challenger:
        return PENTEST_PI_CHALLENGER_SYSTEM if mode == "pentest" else CTF_PI_CHALLENGER_SYSTEM
    return PENTEST_PI_DECIDE_SYSTEM if mode == "pentest" else CTF_PI_DECIDE_SYSTEM


def build_ctf_pi_decide_prompt(
    graph_summary: str,
    *,
    max_intents: int = 4,
    mode: str = "ctf",
    challenger: bool = False,
) -> list[dict[str, str]]:
    """Build the same two-message prompt used by the Pi subprocess."""
    del max_intents
    system_content = _pi_system_prompt(mode=mode, challenger=challenger)
    return [
        {"role": "system", "content": system_content},
        {"role": "user", "content": str(graph_summary)},
    ]


def _failure(
    kind: PlannerFailureKind,
    detail: str,
    *,
    status: str,
    timed_out: bool = False,
    raw_response: str = "",
    finish_reason: str = "",
) -> ReasonResult:
    return ReasonResult(
        goal_met=False,
        intents=[],
        audit_notes=[],
        semantic_dedupe_available=False,
        planner_failure=PlannerFailure(kind, detail),
        diagnostics=ReasonDiagnostics(
            response_status=status,
            timed_out=timed_out,
            finish_reason=(
                finish_reason or ("timeout" if timed_out else "error")
            ),
            raw_response=raw_response,
            raw_response_sha256=hashlib.sha256(
                raw_response.encode("utf-8", errors="replace")
            ).hexdigest(),
            response_chars=len(raw_response),
            parse_status=(
                "provider_output_limit" if finish_reason == "length"
                else "not_run_timeout" if timed_out else "not_run_error"
            ),
            parse_detail=detail,
        ),
    )


def _positive_fact_ids(
    value: Any, *, fact_id_map: dict[str, int] | None = None,
) -> list[int]:
    if not isinstance(value, list):
        return []
    result: list[int] = []
    id_map = fact_id_map or {}
    for item in value:
        text = str(item or "").strip()
        if text in id_map:
            fact_id = int(id_map[text])
        else:
            try:
                fact_id = int(item)
            except (TypeError, ValueError):
                continue
        if fact_id > 0 and fact_id not in result:
            result.append(fact_id)
    return result


@dataclass
class DraftSimulation:
    intents: list[Intent] = field(default_factory=list)
    supersede: list[str] = field(default_factory=list)
    drop_whys: list[str] = field(default_factory=list)
    supersede_facts: list[dict[str, Any]] = field(default_factory=list)
    sub_goal_operations: list[dict[str, Any]] = field(default_factory=list)
    reprioritize: dict[str, str] = field(default_factory=dict)
    parse_rejections: list[dict[str, Any]] = field(default_factory=list)
    receipts: list[dict[str, Any]] = field(default_factory=list)
    goal_met: bool = False
    complete_why: str = ""
    goal_evidence_facts: list[int] = field(default_factory=list)
    progress_summary: str = ""
    raw_open_steps: int = 0
    draft_id: str = ""


def _yaml_scalar(line: str) -> str:
    value = line.split(":", 1)[1].strip() if ":" in line else ""
    if not value:
        return ""
    if value[0] in "\"'":
        try:
            return str(json.loads(value))
        except json.JSONDecodeError:
            return value.strip().strip("\"'")
    return value


def _parse_preview_goals(yaml_text: str) -> list[dict[str, str]]:
    goals: list[dict[str, str]] = []
    current: dict[str, str] | None = None
    in_goals = False
    for raw in str(yaml_text or "").splitlines():
        line = raw.rstrip()
        if line.startswith("goals:"):
            in_goals = True
            continue
        if in_goals and line and not line.startswith(" "):
            break
        if not in_goals:
            continue
        if line.startswith("  - id:"):
            if current:
                goals.append(current)
            current = {
                "id": _yaml_scalar(line),
                "title": "",
                "state": "open",
            }
        elif current is not None and line.startswith("    criterion:"):
            current["title"] = _yaml_scalar(line)
        elif current is not None and line.startswith("    state:"):
            current["state"] = _yaml_scalar(line)
        elif current is not None and (
            line.startswith("    satisfiedAt:")
            or line.startswith("    satisfiedBy:")
            or line.startswith("    satisfiedReason:")
        ):
            current["state"] = "satisfied"
    if current:
        goals.append(current)
    return goals or [{
        "id": "goal_final",
        "title": "goal",
        "state": "open",
    }]


def _extract_pi_jsonl_progress(agent_state_dir: Path | None) -> dict[str, Any]:
    if agent_state_dir is None or not agent_state_dir.is_dir():
        return {}
    sessions_dir = agent_state_dir / "sessions"
    search_root = sessions_dir if sessions_dir.is_dir() else agent_state_dir
    try:
        candidates = sorted(
            (p for p in search_root.rglob("*.jsonl") if p.is_file()),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
    except OSError:
        return {}
    if not candidates:
        return {}
    latest_file = candidates[0]
    tool_calls = 0
    call_Order: list[str] = []
    calls_by_id: dict[str, dict[str, str]] = {}
    latest_note = ""
    started_ts = 0.0
    try:
        raw_lines = latest_file.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return {}
    for raw_line in raw_lines:
        if not raw_line.strip():
            continue
        try:
            row = json.loads(raw_line)
        except json.JSONDecodeError:
            continue
        if not isinstance(row, dict):
            continue
        if started_ts <= 0.0 and row.get("timestamp") is not None:
            raw_ts = row.get("timestamp")
            if isinstance(raw_ts, (int, float)) and raw_ts > 0:
                started_ts = float(raw_ts) / 1000.0 if float(raw_ts) > 1e11 else float(raw_ts)
            elif isinstance(raw_ts, str) and raw_ts.strip():
                try:
                    from datetime import datetime
                    started_ts = datetime.fromisoformat(
                        raw_ts.strip().replace("Z", "+00:00")
                    ).timestamp()
                except ValueError:
                    pass
        row_type = str(row.get("type") or "")
        if row_type == "message":
            msg = row.get("message") or {}
            if not isinstance(msg, dict):
                continue
            role = str(msg.get("role") or "")
            content = msg.get("content")
            if role == "assistant" and isinstance(content, list):
                for block in content:
                    if not isinstance(block, dict):
                        continue
                    btype = str(block.get("type") or "")
                    if btype in {"text", "thinking"}:
                        txt = " ".join(str(block.get(btype) or block.get("text") or "").split()).strip()
                        if txt:
                            latest_note = txt[:360]
                    elif btype == "toolCall":
                        tool_calls += 1
                        cid = str(block.get("id") or f"call_{tool_calls}")
                        tname = str(block.get("name") or "tool")
                        targs = block.get("arguments")
                        cmd_str = ""
                        if isinstance(targs, dict):
                            cmd_str = str(
                                targs.get("command")
                                or targs.get("path")
                                or targs.get("title")
                                or json.dumps(targs, ensure_ascii=False)
                            ).strip()
                        call_Order.append(cid)
                        calls_by_id[cid] = {
                            "tool": tname,
                            "command": " ".join(cmd_str.split())[:240],
                            "output": "(running...)",
                        }
            elif role == "toolResult":
                cid = str(msg.get("toolCallId") or "")
                out_parts: list[str] = []
                if isinstance(content, list):
                    for block in content:
                        if isinstance(block, dict) and block.get("type") == "text":
                            out_parts.append(str(block.get("text") or ""))
                elif isinstance(content, str):
                    out_parts.append(content)
                out_text = " ".join(" ".join(out_parts).split()).strip()
                if cid in calls_by_id:
                    calls_by_id[cid]["output"] = out_text[:280] if out_text else "(empty output)"
    recent_traces = [
        calls_by_id[cid]
        for cid in call_Order[-6:]
        if cid in calls_by_id
    ]
    return {
        "session_file": str(latest_file),
        "started_ts": started_ts,
        "tool_calls": tool_calls,
        "recent_traces": recent_traces,
        "latest_note": latest_note,
    }


def _collect_step_worker_snapshot(
    step_id: str,
    worker_id: str,
    *,
    live_solvers: Mapping[str, Any] | None = None,
    agent_state_root: Path | None = None,
) -> dict[str, Any]:
    solver = None
    if live_solvers:
        solver = live_solvers.get(worker_id)
        if solver is None:
            for cand in live_solvers.values():
                assigned = str(
                    getattr(cand, "intent_id_assigned", "")
                    or getattr(cand, "_intent_id", "")
                    or ""
                ).strip()
                if assigned == step_id:
                    solver = cand
                    if not worker_id:
                        worker_id = str(getattr(cand, "solver_id", "") or "")
                    break
    agent_state_dir: Path | None = None
    if solver is not None:
        raw_env = (
            getattr(solver, "_runtime_env", None)
            or getattr(solver, "_extra_worker_env", None)
            or {}
        )
        if isinstance(raw_env, Mapping) and raw_env.get("PI_CODING_AGENT_DIR"):
            cand_dir = Path(str(raw_env["PI_CODING_AGENT_DIR"])).expanduser()
            if cand_dir.is_dir():
                agent_state_dir = cand_dir
    if agent_state_dir is None and agent_state_root is not None and worker_id:
        cand_dir = agent_state_root / worker_id
        if cand_dir.is_dir():
            agent_state_dir = cand_dir
    jsonl_info = _extract_pi_jsonl_progress(agent_state_dir)
    step_trace_info: dict[str, Any] = {}
    if agent_state_root is not None and step_id:
        step_trace_file = agent_state_root / "step_traces" / f"{step_id}.json"
        if step_trace_file.is_file():
            try:
                step_trace_info = json.loads(step_trace_file.read_text(encoding="utf-8"))
            except Exception:
                step_trace_info = {}
    mem_calls = int(getattr(solver, "_tool_evidence_total_count", 0) or 0) if solver is not None else 0
    mem_traces: list[dict[str, str]] = []
    if solver is not None:
        for rec in list(getattr(solver, "_tool_evidence_records", None) or [])[-15:]:
            cmd = " ".join(str(getattr(rec, "command", "") or "").split())[:360]
            out = " ".join(str(getattr(rec, "output", "") or "").split())[:420]
            mem_traces.append({"tool": "bash", "command": cmd, "output": out or "(empty output)"})
        for pend in list(getattr(solver, "_pending_tool_calls", None) or [])[-2:]:
            if isinstance(pend, Mapping):
                cmd = " ".join(str(pend.get("command") or "").split())[:360]
                if cmd:
                    mem_traces.append({"tool": "bash", "command": cmd, "output": "(running...)"})
    tool_calls = max(
        mem_calls,
        len(mem_traces),
        int(jsonl_info.get("tool_calls") or 0),
        int(step_trace_info.get("tool_calls") or 0),
    )
    traces = (
        list(jsonl_info.get("recent_traces") or [])
        or mem_traces
        or list(step_trace_info.get("recent_traces") or [])
    )
    started_at = float(getattr(solver, "_worker_started_at", 0.0) or 0.0) if solver is not None else 0.0
    if started_at <= 0.0 and float(jsonl_info.get("started_ts") or 0.0) > 0:
        started_at = float(jsonl_info["started_ts"])
    if started_at <= 0.0 and float(step_trace_info.get("started_at") or 0.0) > 0:
        started_at = float(step_trace_info["started_at"])
    elapsed_s = max(0.0, time.time() - started_at) if started_at > 0 else 0.0
    has_live_context = bool(
        solver is not None
        or agent_state_dir is not None
        or tool_calls > 0
        or traces
    )
    return {
        "worker_id": worker_id or str(step_trace_info.get("worker_id") or ""),
        "agent_state_dir": str(agent_state_dir) if agent_state_dir is not None else "",
        "started_at": started_at,
        "elapsed_seconds": round(elapsed_s, 1),
        "tool_calls": tool_calls,
        "recent_traces": traces[-15:],
        "latest_note": str(jsonl_info.get("latest_note") or ""),
        "has_live_context": has_live_context,
    }


def _parse_preview_open_steps(
    yaml_text: str,
    *,
    shared_graph: Any = None,
    live_solvers: Mapping[str, Any] | None = None,
    agent_state_root: Path | None = None,
) -> list[dict[str, Any]]:
    steps: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    in_steps = False
    in_from = False

    def _flush() -> None:
        nonlocal current
        if current and str(current.get("state") or "open") == "open":
            step_id = str(current.get("id") or "")
            worker_id = ""
            if (
                not current.get("running")
                and shared_graph is not None
                and callable(getattr(shared_graph, "intent_claim_state", None))
            ):
                try:
                    claim = dict(
                        shared_graph.intent_claim_state(step_id)
                        or {}
                    )
                    if (
                        str(claim.get("status") or "") == "claimed"
                        and str(claim.get("dispatch_state") or "active") == "active"
                    ):
                        current["running"] = True
                        worker_id = str(claim.get("worker") or "")
                except Exception:
                    pass
            if current.get("running") and (live_solvers or agent_state_root is not None):
                snap = _collect_step_worker_snapshot(
                    step_id,
                    worker_id,
                    live_solvers=live_solvers,
                    agent_state_root=agent_state_root,
                )
                current.update(snap)
            steps.append(current)
        current = None

    for raw in str(yaml_text or "").splitlines():
        line = raw.rstrip()
        if line.startswith("steps:"):
            in_steps = True
            continue
        if not in_steps:
            continue
        if line and not line.startswith(" "):
            break
        if line.startswith("  - id:"):
            _flush()
            current = {
                "id": _yaml_scalar(line),
                "action": "",
                "priority": "normal",
                "state": "open",
                "running": False,
                "checkpoint_published": False,
                "from": [],
            }
            in_from = False
            continue
        if current is None:
            continue
        if line.startswith("    action:"):
            current["action"] = _yaml_scalar(line)
            in_from = False
        elif line.strip() == "from:":
            in_from = True
        elif in_from and line.startswith("      - "):
            current["from"].append(line.strip()[2:].strip())
        elif in_from and line.strip() == "[]":
            in_from = False
        elif line.startswith("    priority:"):
            current["priority"] = _yaml_scalar(line) or "normal"
            in_from = False
        elif line.startswith("    state:"):
            current["state"] = _yaml_scalar(line) or "open"
            in_from = False
        elif line.startswith("    execution:"):
            current["running"] = _yaml_scalar(line) == "running"
            in_from = False
        elif line.startswith("    checkpointPublished:"):
            current["checkpoint_published"] = _yaml_scalar(line).lower() == "true"
            in_from = False
        elif line.startswith("    ") and not line.startswith("      "):
            in_from = False
    _flush()
    return steps


def _fact_catalog(shared_graph: Any) -> tuple[dict[str, int], list[dict[str, Any]], int]:
    fact_id_map: dict[str, int] = {}
    facts: list[dict[str, Any]] = []
    watermark = 0
    if shared_graph is None:
        return fact_id_map, facts, watermark
    is_pentest_graph = (
        getattr(getattr(shared_graph, "challenge", None), "mode", "ctf") == "pentest"
    )
    active_seqs = (
        set(shared_graph._active_fact_seq_set())
        if callable(getattr(shared_graph, "_active_fact_seq_set", None))
        else None
    )
    cutoff = (int(shared_graph.current_visit_wave_cutoff())
              if callable(getattr(shared_graph, "current_visit_wave_cutoff", None)) else 0)
    next_fact_index = 0
    try:
        events = shared_graph.events()
    except Exception:
        return fact_id_map, facts, watermark
    fact_states: dict[int, dict[str, Any]] = {}
    if hasattr(shared_graph, "_fact_state_map"):
        try:
            fact_states = dict(shared_graph._fact_state_map() or {})
        except Exception:
            fact_states = {}
    for event in events:
        try:
            seq = int(event.get("seq") or 0)
        except (TypeError, ValueError):
            seq = 0
        watermark = max(watermark, seq)
        if str(event.get("kind") or "") != "fact_added":
            continue
        payload = event.get("payload") or {}
        text = str(payload.get("fact") or "").strip()
        if not text:
            continue
        source = str(payload.get("source") or "")
        if source != "origin" and seq <= cutoff:
            continue
        if source == "origin":
            fact_id = "fact_origin"
        else:
            next_fact_index += 1
            fact_id = f"fact_{next_fact_index:03d}"
            f_state = fact_states.get(int(seq), {})
            if (
                str(f_state.get("state") or "") in {"rejected", "merged", "superseded"}
                or bool(f_state.get("retired"))
                or (is_pentest_graph and f_state.get("verified_effective") is False)
            ):
                continue
        if active_seqs is not None and seq not in active_seqs:
            continue
        fact_id_map[fact_id] = seq
        title = text.split("\n", 1)[0].strip()
        enables = str(
            (payload.get("evidence_provenance") or {}).get("enables") or ""
        ).strip()
        if title:
            facts.append({
                "id": fact_id,
                "seq": seq,
                "title": title,
                "enables": enables,
                "derived_steps": 0,
                "negative_outcomes": 0,
                "enabling_outcomes": 0,
            })
    if facts and shared_graph is not None:
        fact_by_seq = {int(f["seq"]): f for f in facts}
        intent_from: dict[str, list[int]] = {}
        intent_to: dict[str, list[int]] = {}
        concluded_intents: set[str] = set()
        dead_end_intents: set[str] = set()
        for event in events:
            kind = str(event.get("kind") or "")
            payload = event.get("payload") or {}
            if kind == "intent_proposed":
                iid = str(payload.get("intent_id") or "").strip()
                raw_from = payload.get("from_fact_seqs") or payload.get("from_facts") or []
                if iid and isinstance(raw_from, list):
                    intent_from[iid] = [int(x) for x in raw_from if str(x).isdigit()]
            elif kind == "intent_concluded":
                iid = str(payload.get("intent_id") or "").strip()
                conclude_res = str(payload.get("result") or "").strip().lower()
                if conclude_res in {
                    "cancelled", "timed_out", "superseded",
                    "operator_stop", "interrupted", "handoff_missing",
                    "steered", "error", "oom",
                } or bool(payload.get("handoff_missing")):
                    continue
                if iid:
                    concluded_intents.add(iid)
                    to_seq = payload.get("to_fact_seq")
                    if isinstance(to_seq, int) and to_seq > 0:
                        intent_to.setdefault(iid, []).append(to_seq)
            elif kind == "dead_end":
                iid = str(payload.get("intent_id") or "").strip()
                if iid:
                    dead_end_intents.add(iid)
        for iid in concluded_intents:
            f_seqs = [s for s in intent_from.get(iid, []) if s in fact_by_seq]
            p_seqs = [s for s in intent_to.get(iid, []) if s in fact_by_seq]
            has_enabling = any(bool(fact_by_seq[s].get("enables")) for s in p_seqs)
            has_valid_fact = bool(p_seqs) and (iid not in dead_end_intents)
            is_fruitless = (not p_seqs) or (iid in dead_end_intents)
            for s in f_seqs:
                fact_by_seq[s]["derived_steps"] = int(fact_by_seq[s]["derived_steps"]) + 1
                if has_enabling or has_valid_fact:
                    fact_by_seq[s]["enabling_outcomes"] = int(fact_by_seq[s]["enabling_outcomes"]) + 1
                elif is_fruitless:
                    fact_by_seq[s]["negative_outcomes"] = int(fact_by_seq[s]["negative_outcomes"]) + 1
    return fact_id_map, facts, watermark


def _current_closed_reopen_rows(shared_graph: Any) -> list[dict[str, Any]]:
    cutoff = (int(shared_graph.current_visit_wave_cutoff())
              if callable(getattr(shared_graph, "current_visit_wave_cutoff", None)) else 0)
    return [row for row in closed_reopen_rows(shared_graph)
            if int(row.get("seq") or 0) > cutoff]


def _reserved_goal_ids(shared_graph: Any) -> set[str]:
    if shared_graph is None:
        return set()
    return {
        str((event.get("payload") or {}).get("goal_id") or "")
        for event in shared_graph.events()
        if event.get("kind") == "sub_goal_added"
    } - {""}


def _collect_done_steps_for_peek(
    shared_graph: Any,
    *,
    agent_state_root: Path | None = None,
) -> tuple[list[dict[str, Any]], dict[str, str]]:
    """Collect completed step execution traces and fact_id -> step_id map for peek_step."""
    done_steps: list[dict[str, Any]] = []
    fact_to_step: dict[str, str] = {}
    if shared_graph is None:
        return done_steps, fact_to_step
    cutoff = (
        int(shared_graph.current_visit_wave_cutoff())
        if callable(getattr(shared_graph, "current_visit_wave_cutoff", None))
        else 0
    )
    all_fact_ids: dict[int, str] = {}
    next_fact_index = 0
    try:
        events = shared_graph.events()
    except Exception:
        events = []
    for event in events:
        try:
            seq = int(event.get("seq") or 0)
        except (TypeError, ValueError):
            seq = 0
        if str(event.get("kind") or "") != "fact_added":
            continue
        payload = event.get("payload") or {}
        text = str(payload.get("fact") or "").strip()
        if not text:
            continue
        source = str(payload.get("source") or "")
        if source != "origin" and seq <= cutoff:
            continue
        if source == "origin":
            all_fact_ids[seq] = "fact_origin"
        else:
            next_fact_index += 1
            all_fact_ids[seq] = f"fact_{next_fact_index:03d}"

    conn = getattr(shared_graph, "_conn", None)
    lock = getattr(shared_graph, "_lock", None)
    chal_id = str(getattr(getattr(shared_graph, "challenge", None), "id", "") or "")
    artifacts = getattr(shared_graph, "artifacts", None)
    if conn is None or not chal_id:
        return done_steps, fact_to_step

    try:
        if lock is not None:
            with lock:
                intent_rows = conn.execute(
                    "SELECT intent_id, goal, worker, status, dispatch_state "
                    "FROM intents WHERE challenge_id=? AND status!='open' "
                    "ORDER BY created_seq, intent_id",
                    (chal_id,),
                ).fetchall()
                prod_rows = conn.execute(
                    "SELECT p.intent_id, p.fact_seq FROM intent_products p "
                    "JOIN intents i ON i.intent_id=p.intent_id "
                    "WHERE i.challenge_id=?",
                    (chal_id,),
                ).fetchall()
                obs_rows = conn.execute(
                    "SELECT intent_id, provenance_json FROM observations "
                    "WHERE challenge_id=? ORDER BY observation_seq",
                    (chal_id,),
                ).fetchall()
        else:
            intent_rows = []
            prod_rows = []
            obs_rows = []
    except Exception:
        return done_steps, fact_to_step

    step_to_facts: dict[str, list[str]] = {}
    for raw_iid, raw_fseq in prod_rows:
        iid = str(raw_iid or "").strip()
        try:
            fseq = int(raw_fseq or 0)
        except (TypeError, ValueError):
            fseq = 0
        fid = all_fact_ids.get(fseq, "")
        if iid and fid:
            step_to_facts.setdefault(iid, [])
            if fid not in step_to_facts[iid]:
                step_to_facts[iid].append(fid)
            fact_to_step[fid] = iid

    obs_refs_by_step: dict[str, list[dict[str, Any]]] = {}
    for raw_iid, raw_prov in obs_rows:
        iid = str(raw_iid or "").strip()
        if not iid:
            continue
        try:
            prov = json.loads(raw_prov or "{}")
        except Exception:
            prov = {}
        for ref in prov.get("artifact_refs") or []:
            if isinstance(ref, dict):
                obs_refs_by_step.setdefault(iid, []).append(ref)

    for raw_iid, raw_goal, raw_worker, raw_status, raw_disp in intent_rows:
        step_id = str(raw_iid or "").strip()
        if not step_id:
            continue
        file_info: dict[str, Any] = {}
        if agent_state_root is not None:
            tf = agent_state_root / "step_traces" / f"{step_id}.json"
            if tf.is_file():
                try:
                    file_info = json.loads(tf.read_text(encoding="utf-8"))
                except Exception:
                    file_info = {}
        traces: list[dict[str, str]] = list(file_info.get("recent_traces") or [])
        refs = obs_refs_by_step.get(step_id) or []
        if not traces and refs:
            for ref in refs[-15:]:
                cmd = " ".join(str(ref.get("command") or "").split())[:360]
                aid = str(ref.get("artifact_id") or "").strip()
                out_str = ""
                if aid and artifacts is not None:
                    try:
                        raw_out = artifacts.read_text(aid) or ""
                        out_str = " ".join(str(raw_out).split())[:420]
                    except Exception:
                        out_str = ""
                traces.append({
                    "tool": "bash",
                    "command": cmd,
                    "output": out_str or "(empty output)",
                })
        tool_calls = max(
            int(file_info.get("tool_calls") or 0),
            len(refs),
            len(traces),
        )
        done_steps.append({
            "id": step_id,
            "action": str(raw_goal or ""),
            "state": "dropped" if str(raw_disp or "") == "superseded" else str(raw_status or "done"),
            "worker_id": str(raw_worker or file_info.get("worker_id") or ""),
            "elapsed_seconds": float(file_info.get("elapsed_seconds") or 0.0),
            "tool_calls": tool_calls,
            "to": step_to_facts.get(step_id, []),
            "recent_traces": traces[-15:],
        })
    return done_steps, fact_to_step


def build_decide_preview_graph(
    shared_graph: Any,
    *,
    max_intents: int,
    live_solvers: Mapping[str, Any] | None = None,
    agent_state_root: Path | None = None,
) -> dict[str, Any]:
    fact_id_map, facts, watermark = _fact_catalog(shared_graph)
    done_steps, fact_to_step = _collect_done_steps_for_peek(
        shared_graph, agent_state_root=agent_state_root
    )
    yaml_text = ""
    to_yaml = getattr(shared_graph, "to_ctf_graph_yaml", None)
    if callable(to_yaml):
        try:
            yaml_text = str(to_yaml() or "")
        except Exception:
            yaml_text = ""
    resources = []
    if shared_graph is not None and callable(getattr(shared_graph, "pocs", None)):
        resources = [
            str(row.get("poc_id") or "")
            for row in shared_graph.pocs()
            if str(row.get("status") or "") in {"available", "directional", "wip"}
        ]
    mode = getattr(getattr(shared_graph, "challenge", None), "mode", "ctf")
    graph_seq = watermark
    if shared_graph is not None and callable(
        getattr(shared_graph, "semantic_graph_watermark", None)
    ):
        try:
            graph_seq = int(shared_graph.semantic_graph_watermark() or 0)
        except Exception:
            graph_seq = watermark
    return {
        "schema": "muteki.pentest-decide-graph.v1" if mode == "pentest" else "muteki.ctf-decide-graph.v1",
        "max_intents": max(-1, int(max_intents)),
        "graph_seq": graph_seq,
        "fact_id_map": fact_id_map,
        "facts": facts,
        "resource_ids": resources,
        "goals": _parse_preview_goals(yaml_text),
        "reserved_goal_ids": sorted(_reserved_goal_ids(shared_graph)),
        "open_steps": _parse_preview_open_steps(
            yaml_text,
            shared_graph=shared_graph,
            live_solvers=live_solvers,
            agent_state_root=agent_state_root,
        ),
        "done_steps": done_steps,
        "fact_to_step": fact_to_step,
        "cancelled": _current_closed_reopen_rows(shared_graph) if shared_graph is not None else [],
    }


def simulate_draft(
    operations: list[Any],
    *,
    max_intents: int,
    fact_id_map: dict[str, int] | None = None,
    shared_graph: Any = None,
    cancelled: list[dict[str, Any]] | None = None,
    draft_id: str = "",
    goals: list[dict[str, Any]] | None = None,
    reserved_goal_ids: list[str] | None = None,
    is_pentest: bool | None = None,
    resource_ids: list[str] | None = None,
    open_steps: list[dict[str, Any]] | None = None,
    challenger: bool = False,
) -> DraftSimulation:
    sim = DraftSimulation(draft_id=str(draft_id or "").strip())
    active_fact_map = dict(fact_id_map or {})
    if shared_graph is not None:
        active_fact_map, _, _ = _fact_catalog(shared_graph)
    initial_fact_map = dict(active_fact_map)
    known_goals = {str(row.get("id") or row.get("goal_id")): dict(row)
                   for row in (goals or [])}
    if shared_graph is not None and callable(getattr(shared_graph, "sub_goals", None)):
        known_goals.update({str(row["goal_id"]): dict(row)
                            for row in shared_graph.sub_goals()})
    known_goals.setdefault("goal_final", {"state": "open"})
    reserved_goals = set(reserved_goal_ids or []) | _reserved_goal_ids(shared_graph) | set(known_goals)

    def _sources_valid(raw: Any) -> bool:
        known_seqs = set(active_fact_map.values())
        return isinstance(raw, list) and all(
            str(item) in active_fact_map
            or (not isinstance(item, bool) and str(item).isdigit() and int(item) in known_seqs)
            for item in raw
        )

    def _open_sources_valid(raw: Any) -> bool:
        known_seqs = set(initial_fact_map.values())
        return isinstance(raw, list) and all(
            str(item) in initial_fact_map
            or (not isinstance(item, bool) and str(item).isdigit() and int(item) in known_seqs)
            for item in raw
        )

    def _record(
        index: int,
        op: str,
        *,
        outcome: str,
        reason_code: str,
        goal: str = "",
        intent_id: str = "",
        why: str = "",
    ) -> None:
        row = {
            "raw_index": index,
            "op": op,
            "outcome": outcome,
            "stage": "parse",
            "reason_code": reason_code,
            "goal": " ".join(goal.split()),
            "model_intent_id": intent_id,
        }
        if why:
            row["why"] = " ".join(why.split())
        sim.receipts.append(row)
        if op == "open_step" and outcome != "accepted":
            sim.parse_rejections.append(row)

    active_open_ids: set[str] = set()
    if shared_graph is not None:
        parsed_open = _parse_preview_open_steps(
            shared_graph.to_ctf_graph_yaml(), shared_graph=shared_graph
        )
        active_open_ids = {
            str(row.get("id") or "")
            for row in parsed_open
        }
    elif open_steps is not None:
        active_open_ids = {
            str(row.get("id") or "")
            for row in open_steps
            if isinstance(row, dict) and str(row.get("id") or "").strip()
        }
    dropped_open_ids = {
        str(op.get("intent_id") or op.get("step_id") or "").strip()
        for op in operations
        if isinstance(op, dict)
        and not challenger
        and str(op.get("op") or "").strip() == "drop_step"
        and str(op.get("intent_id") or op.get("step_id") or "").strip() in active_open_ids
    }
    effective_max_intents = (
        int(max_intents) + len(dropped_open_ids)
        if int(max_intents) >= 0
        else int(max_intents)
    )

    for index, operation in enumerate(operations):
        if not isinstance(operation, dict):
            _record(index, "unknown", outcome="dropped", reason_code="op_not_object")
            continue
        op = str(operation.get("op") or "").strip()
        if challenger and op in _CHALLENGER_PLAN_OPS:
            _record(
                index, op, outcome="dropped",
                reason_code="challenger_plan_forbidden",
            )
            continue
        if op == "open_step":
            sim.raw_open_steps += 1
            action = str(operation.get("action") or "").strip()
            if not action:
                _record(index, op, outcome="dropped", reason_code="missing_action")
                continue
            active_seqs = set(active_fact_map.values())
            active_draft_count = sum(
                set(intent.from_facts) <= active_seqs for intent in sim.intents
            )
            if effective_max_intents >= 0 and active_draft_count >= effective_max_intents:
                _record(
                    index, op, outcome="dropped",
                    reason_code="max_intents_exceeded", goal=action,
                )
                continue
            expected = str(
                operation.get("expected_observable")
                or operation.get("expectedObservable")
                or ""
            ).strip()
            stop = str(
                operation.get("stop_condition")
                or operation.get("stopCondition")
                or ""
            ).strip()
            coverage = str(
                operation.get("coverage_key")
                or operation.get("coverageKey")
                or ""
            ).strip()
            is_pentest_mode = (
                bool(is_pentest)
                if is_pentest is not None
                else getattr(getattr(shared_graph, "challenge", None), "mode", "ctf") == "pentest"
            )
            if is_pentest_mode and (not expected or not stop or not coverage):
                _record(
                    index, op, outcome="dropped",
                    reason_code="missing_step_contract", goal=action,
                )
                continue
            raw_sources = operation.get("from")
            if not isinstance(raw_sources, list):
                _record(index, op, outcome="dropped", reason_code="invalid_from", goal=action)
                continue
            if not _open_sources_valid(raw_sources):
                _record(index, op, outcome="dropped", reason_code="unknown_fact", goal=action)
                continue
            raw_resources = operation.get("requires") or []
            if not isinstance(raw_resources, list) or any(
                not isinstance(item, str) or not item.strip()
                for item in raw_resources
            ):
                _record(index, op, outcome="dropped", reason_code="invalid_resource", goal=action)
                continue
            required_pocs = list(dict.fromkeys(item.strip() for item in raw_resources))
            if shared_graph is not None and callable(getattr(shared_graph, "pocs", None)):
                available_pocs = {
                    str(row.get("poc_id") or "") for row in shared_graph.pocs()
                    if str(row.get("status") or "") in {"available", "directional", "wip"}
                }
                if set(required_pocs) - available_pocs:
                    _record(index, op, outcome="dropped", reason_code="unknown_resource", goal=action)
                    continue
            elif resource_ids is not None:
                available_pocs = {str(item) for item in resource_ids if str(item).strip()}
                if set(required_pocs) - available_pocs:
                    _record(index, op, outcome="dropped", reason_code="unknown_resource", goal=action)
                    continue
            from_facts = _positive_fact_ids(
                operation.get("from"), fact_id_map=initial_fact_map
            )
            priority = str(operation.get("priority") or "normal").strip().lower()
            if priority not in {"high", "normal", "low"}:
                priority = "normal"
            value_claim: dict[str, Any] = {}
            contract = getattr(getattr(shared_graph, "challenge", None), "pentest_contract", None)
            if contract is not None:
                from muteki.pentest.contract import in_scope_url
                asset = str(operation.get("asset") or contract.target).strip()
                try:
                    version = int(operation.get("authorization_version") or contract.version)
                except (TypeError, ValueError):
                    _record(index, op, outcome="dropped", reason_code="invalid_authorization_version", goal=action)
                    continue
                risk = str(operation.get("risk_tier") or "bounded_validation").strip()
                if not in_scope_url(asset, contract):
                    _record(index, op, outcome="dropped", reason_code="asset_out_of_scope", goal=action)
                    continue
                if version != contract.version:
                    _record(index, op, outcome="dropped", reason_code="authorization_version_mismatch", goal=action)
                    continue
                if risk not in {"passive", "bounded_validation"}:
                    _record(index, op, outcome="dropped", reason_code="risk_not_authorized", goal=action)
                    continue
                value_claim = {
                    "asset": asset,
                    "identity": str(operation.get("identity") or "anonymous").strip(),
                    "risk_tier": risk,
                    "evidence_requirement": str(operation.get("evidence_requirement") or expected).strip(),
                    "authorization_version": version,
                }
            intent_id = f"D{len(sim.intents) + 1}"
            sim.intents.append(
                Intent(
                    intent_id=intent_id,
                    goal=action,
                    from_facts=from_facts,
                    priority=priority,
                    requested_priority=priority,
                    expected_observable=expected,
                    stop_condition=stop,
                    coverage_key=coverage,
                    value_claim=value_claim,
                    required_pocs=required_pocs,
                    requires_capabilities=list(dict.fromkeys(
                        str(item).strip() for item in (operation.get("requires_capabilities") or [])
                        if str(item).strip()
                    )),
                )
            )
            _record(
                index, op, outcome="accepted", reason_code="accepted",
                goal=action, intent_id=intent_id,
            )
        elif op == "drop_step":
            intent_id = str(operation.get("intent_id") or operation.get("step_id") or "").strip()
            why = str(
                operation.get("reason") or operation.get("why") or ""
            ).strip()
            if not intent_id:
                _record(index, op, outcome="dropped", reason_code="missing_intent_id")
                continue
            if intent_id in sim.supersede:
                _record(
                    index, op, outcome="dropped",
                    reason_code="duplicate_drop", intent_id=intent_id, why=why,
                )
                continue
            sim.supersede.append(intent_id)
            if why:
                sim.drop_whys.append(f"{intent_id}: {why}")
            _record(
                index, op, outcome="accepted", reason_code="drop_step",
                intent_id=intent_id, why=why,
            )
        elif op == "change_step_priority":
            intent_id = str(operation.get("intent_id") or "").strip()
            priority = str(operation.get("priority") or "").strip().lower()
            if intent_id and priority in {"high", "normal", "low"}:
                sim.reprioritize[intent_id] = priority
                _record(
                    index, op, outcome="accepted",
                    reason_code="change_step_priority", intent_id=intent_id,
                )
            else:
                _record(
                    index, op, outcome="dropped",
                    reason_code="invalid_priority", intent_id=intent_id,
                )
        elif op == "supersede_fact":
            raw_fact_id = str(operation.get("fact_id") or "").strip()
            reason = str(operation.get("reason") or "").strip()
            raw_by_id = str(operation.get("by_fact_id") or "").strip()
            known_seqs = set(active_fact_map.values())

            def _stable_seq(key: str) -> int:
                try:
                    return int(operation.get(key) or 0)
                except (TypeError, ValueError):
                    return 0

            target_seq = _stable_seq("fact_seq") or (
                int(active_fact_map.get(raw_fact_id) or 0)
                if raw_fact_id and raw_fact_id != "fact_origin"
                else 0
            )
            by_seq = _stable_seq("by_fact_seq") or (
                int(active_fact_map.get(raw_by_id) or 0)
                if raw_by_id and raw_by_id != "fact_origin"
                else 0
            )
            by_requested = bool(raw_by_id) or _stable_seq("by_fact_seq") > 0
            if (
                target_seq <= 0
                or target_seq not in known_seqs
                or not reason
                or (by_requested and (
                    by_seq <= 0 or by_seq == target_seq or by_seq not in known_seqs))
            ):
                _record(
                    index, op, outcome="dropped",
                    reason_code="invalid_supersede_fact", goal=raw_fact_id,
                )
                continue
            if any(int(item.get("fact_seq") or 0) == target_seq for item in sim.supersede_facts):
                _record(
                    index, op, outcome="dropped",
                    reason_code="duplicate_supersede_fact", goal=raw_fact_id,
                )
                continue
            active_fact_map = {key: seq for key, seq in active_fact_map.items() if seq != target_seq}
            sim.supersede_facts.append({
                "fact_id": raw_fact_id,
                "fact_seq": target_seq,
                "by_fact_id": raw_by_id,
                "by_fact_seq": by_seq if by_seq > 0 else None,
                "reason": reason,
            })
            _record(
                index, op, outcome="accepted",
                reason_code="supersede_fact", goal=raw_fact_id, why=reason,
            )
        elif op == "add_sub_goal":
            criterion = str(operation.get("criterion") or "").strip()
            if not criterion:
                _record(index, op, outcome="dropped", reason_code="missing_goal_criterion")
                continue
            number = len(reserved_goals - {"goal_final"}) + 1
            while f"goal_{number:03d}" in reserved_goals:
                number += 1
            goal_id = f"goal_{number:03d}"
            requested_id = str(operation.get("goal_id") or goal_id)
            if requested_id != goal_id:
                _record(index, op, outcome="dropped", reason_code="stale_goal_id")
                continue
            reserved_goals.add(goal_id)
            known_goals[goal_id] = {"state": "open", "criterion": criterion}
            sim.sub_goal_operations.append({"op": op, "goal_id": goal_id, "criterion": criterion})
            _record(index, op, outcome="accepted", reason_code=op, goal=goal_id)
        elif op == "satisfy_goal":
            goal_id = str(operation.get("goal_id") or "goal_final").strip()
            if goal_id not in known_goals:
                _record(index, op, outcome="dropped", reason_code="unknown_goal", goal=goal_id)
                continue
            raw_sources = operation.get("from", [])
            if not _sources_valid(raw_sources) or (goal_id != "goal_final" and not raw_sources):
                _record(index, op, outcome="dropped", reason_code="invalid_goal_evidence")
                continue
            sources = _positive_fact_ids(raw_sources, fact_id_map=active_fact_map)
            reason = str(operation.get("reason") or operation.get("why") or "").strip()
            if not reason:
                _record(index, op, outcome="dropped", reason_code="missing_goal_reason")
                continue
            contract = getattr(getattr(shared_graph, "challenge", None), "pentest_contract", None)
            if contract is not None and goal_id == "goal_final":
                if contract.report_goal_mode == "count":
                    _record(index, op, outcome="dropped", reason_code="report_count_governed_by_host")
                    continue
                if not sources:
                    _record(index, op, outcome="dropped", reason_code="invalid_goal_evidence")
                    continue
                from muteki.pentest.judgement import goal_evidence_valid
                if not goal_evidence_valid(shared_graph.events(), contract, sources):
                    _record(index, op, outcome="dropped", reason_code="unverified_goal_evidence")
                    continue
            if goal_id == "goal_final":
                sim.goal_met = True
                sim.complete_why = reason
                sim.goal_evidence_facts = sources
            else:
                known_goals[goal_id]["state"] = "satisfied"
                sim.sub_goal_operations.append({"op": op, "goal_id": goal_id,
                                                "reason": reason, "evidence_fact_seqs": sources})
            _record(index, op, outcome="accepted", reason_code="satisfy_goal", goal=goal_id)
        elif op == "commit":
            sim.progress_summary = str(operation.get("summary") or "").strip()
            _record(index, op, outcome="accepted", reason_code="commit")
        elif op in {"preview", "peek_step"}:
            _record(index, op, outcome="ignored", reason_code="preview_not_an_op")
        else:
            _record(index, op or "unknown", outcome="dropped", reason_code="unknown_op")
    # A revision retires its source Fact. Reasons are audit text, never replacement evidence.
    def _open_sources_remain_valid(raw: Any) -> bool:
        return _sources_valid(raw)

    removed_intents: set[str] = set()
    valid_final_receipt: dict[str, Any] | None = None
    for receipt in sim.receipts:
        if receipt["outcome"] != "accepted" or receipt["op"] not in {"open_step", "satisfy_goal"}:
            continue
        operation = operations[receipt["raw_index"]]
        raw_from = operation.get("from", [])
        if receipt["op"] == "open_step":
            if not _open_sources_remain_valid(raw_from):
                receipt.update(outcome="dropped", reason_code="superseded_fact_in_draft")
                removed_intents.add(receipt["model_intent_id"])
                sim.parse_rejections.append(receipt)
        else:
            if not _sources_valid(raw_from):
                receipt.update(outcome="dropped", reason_code="superseded_fact_in_draft")
            elif receipt["goal"] == "goal_final":
                valid_final_receipt = receipt
    sim.intents = [intent for intent in sim.intents if intent.intent_id not in removed_intents]
    active_seqs = set(active_fact_map.values())
    sim.sub_goal_operations = [operation for operation in sim.sub_goal_operations
                               if operation["op"] != "satisfy_goal"
                               or set(operation["evidence_fact_seqs"]) <= active_seqs]
    if valid_final_receipt is None:
        sim.goal_met = False
        sim.complete_why = ""
        sim.goal_evidence_facts = []
    else:
        final_operation = operations[valid_final_receipt["raw_index"]]
        sim.complete_why = str(final_operation.get("reason") or final_operation.get("why") or "").strip()
        sim.goal_evidence_facts = _positive_fact_ids(final_operation.get("from", []), fact_id_map=active_fact_map)
    return sim


def render_decide_preview(
    draft: dict[str, Any],
    graph: dict[str, Any],
    *,
    shared_graph: Any = None,
) -> str:
    operations = draft.get("operations")
    if not isinstance(operations, list):
        operations = []
    fact_id_map = {
        str(key): int(value)
        for key, value in dict(graph.get("fact_id_map") or {}).items()
        if str(key).strip()
    }
    try:
        raw_max_intents = graph.get("max_intents")
        max_intents = 4 if raw_max_intents is None else int(raw_max_intents)
    except (TypeError, ValueError):
        max_intents = 4
    sim = simulate_draft(
        operations,
        max_intents=max_intents,
        fact_id_map=fact_id_map,
        shared_graph=shared_graph,
        cancelled=list(graph.get("cancelled") or []),
        goals=list(graph.get("goals") or []),
        reserved_goal_ids=list(graph.get("reserved_goal_ids") or []),
        draft_id=str(draft.get("draft_id") or ""),
        is_pentest=(str(graph.get("schema") or "") == "muteki.pentest-decide-graph.v1"),
        resource_ids=list(graph.get("resource_ids") or []) if "resource_ids" in graph else None,
        open_steps=list(graph.get("open_steps") or []) if "open_steps" in graph else None,
    )
    seq_to_id = {int(seq): fact_id for fact_id, seq in fact_id_map.items()}
    remaining: list[dict[str, Any]] = []
    dropped_ids = set(sim.supersede)
    revised_refs = {str(value) for item in sim.supersede_facts
                    for value in (item["fact_id"], item["fact_seq"])}
    for row in list(graph.get("open_steps") or []):
        if not isinstance(row, dict):
            continue
        step_id = str(row.get("id") or "").strip()
        if (not step_id or step_id in dropped_ids
                or any(str(source) in revised_refs for source in row.get("from") or [])):
            continue
        remaining.append({
            "id": step_id,
            "action": str(row.get("action") or ""),
            "priority": sim.reprioritize.get(
                step_id, str(row.get("priority") or "normal")
            ),
            "from": list(row.get("from") or []),
            "new": False,
        })
    for intent in sim.intents:
        remaining.append({
            "id": intent.intent_id,
            "action": intent.goal,
            "priority": intent.priority,
            "from": [
                seq_to_id.get(int(seq), str(seq))
                for seq in (intent.from_facts or [])
            ],
            "new": True,
        })
    lines = [
        f"# advisory snapshot seq={graph.get('graph_seq') or 0} "
        f"draft_ops={len(operations)} "
        "(commit 以当时图重验为准)",
    ]
    if draft.get("committed"):
        lines.append("# draft already committed")
    if sim.goal_met:
        lines.append("# satisfy_goal: 按现有规则将接受达成标记，不是证据验证通过")
    lines.append("goals:")
    projected_goals = {str(goal.get("id")): dict(goal)
                       for goal in list(graph.get("goals") or []) if isinstance(goal, dict)}
    for operation in sim.sub_goal_operations:
        goal_id = operation["goal_id"]
        if operation["op"] == "add_sub_goal":
            projected_goals[goal_id] = {"id": goal_id, "title": operation["criterion"], "state": "open"}
        elif goal_id in projected_goals:
            projected_goals[goal_id]["state"] = "satisfied"
    for goal_id, goal in projected_goals.items():
        state = "satisfied" if sim.goal_met and goal_id == "goal_final" else str(goal.get("state") or "open")
        title = str(goal.get("title") or goal_id)
        lines.append(f"  - {goal_id}: {title} [{state}]")
    lines.append("facts:")
    revised_ids = {item["fact_id"] for item in sim.supersede_facts}
    facts = [row for row in list(graph.get("facts") or [])
             if isinstance(row, dict) and row.get("id") not in revised_ids]
    if facts:
        for fact in facts:
            lines.append(
                f"  - {fact.get('id')}: {str(fact.get('title') or '')}"
            )
    else:
        lines.append("  []")
    lines.append("steps:")
    if remaining:
        for step in remaining:
            mark = " new" if step.get("new") else ""
            from_ids = ",".join(str(item) for item in step.get("from") or []) or "-"
            lines.append(
                f"  - {step['id']} [{step['priority']}{mark}]: "
                f"{step['action']}  from={from_ids}"
            )
    else:
        lines.append("  []")
    lines.append("changes:")
    drop_ids = list(sim.supersede)
    lines.append(f"  drop: [{', '.join(drop_ids)}]" if drop_ids else "  drop: []")
    if sim.reprioritize:
        changed = ", ".join(
            f"{intent_id}->{priority}"
            for intent_id, priority in sim.reprioritize.items()
        )
        lines.append(f"  priority: [{changed}]")
    else:
        lines.append("  priority: []")
    opened = [intent.intent_id for intent in sim.intents]
    lines.append(f"  open: [{', '.join(opened)}]" if opened else "  open: []")
    lines.append("receipts:")
    if sim.receipts:
        for row in sim.receipts:
            label = str(row.get("goal") or row.get("model_intent_id") or "")
            suffix = f" {label}" if label else ""
            lines.append(
                f"  - op[{row.get('raw_index')}] {row.get('op')} "
                f"{row.get('outcome')} {row.get('reason_code')}{suffix}"
            )
    else:
        lines.append("  []")
    return "\n".join(lines)


def _render_done_step_block(row: Mapping[str, Any], header_note: str = "") -> str:
    step_id = str(row.get("id") or "")
    state = str(row.get("state") or "done")
    worker_id = str(row.get("worker_id") or "")
    action = str(row.get("action") or "")
    tool_calls = int(row.get("tool_calls") or 0)
    elapsed_s = float(row.get("elapsed_seconds") or 0.0)
    to_facts = [str(f) for f in (row.get("to") or []) if str(f)]
    traces = list(row.get("recent_traces") or [])
    lines: list[str] = []
    if header_note:
        lines.append(header_note)
    w_info = f"Worker {worker_id}，" if worker_id else ""
    el_info = f"耗时 {elapsed_s:.0f}s，" if elapsed_s > 0 else ""
    to_info = f"，产出 {', '.join(to_facts)}" if to_facts else ""
    lines.append(f"step: {step_id}")
    lines.append(
        f"state: {state} (已结案，{w_info}{el_info}共执行 {tool_calls} 次工具调用{to_info})"
    )
    lines.append(f"action: {action}")
    if traces:
        lines.append("executed_tool_calls:")
        for idx, tr in enumerate(traces[-15:], 1):
            if not isinstance(tr, Mapping):
                continue
            tname = str(tr.get("tool") or "tool")
            cmd = str(tr.get("command") or "")
            out = str(tr.get("output") or "")
            lines.append(f"  [{idx}] {tname}: {cmd}")
            if out:
                lines.append(f"      -> {out}")
    else:
        lines.append("executed_tool_calls: (无已保存的工具输出记录)")
    return "\n".join(lines)


def render_peek_step(intent_id: str, graph: Mapping[str, Any]) -> str:
    target = str(intent_id or "").strip()
    if not target:
        return "intent_id 不能为空"
    fact_to_step = dict(graph.get("fact_to_step") or {})
    header_note = ""
    if target.startswith("fact_") and target in fact_to_step:
        resolved_step = str(fact_to_step[target]).strip()
        header_note = f"[{target} 由 {resolved_step} 产出]"
        target = resolved_step
    row = next(
        (
            dict(item)
            for item in list(graph.get("open_steps") or [])
            if isinstance(item, Mapping) and str(item.get("id") or "").strip() == target
        ),
        None,
    )
    done_steps_list = [
        dict(item)
        for item in list(graph.get("done_steps") or [])
        if isinstance(item, Mapping)
    ]
    done_by_id = {str(item.get("id") or "").strip(): item for item in done_steps_list}
    if row is None:
        done_row = done_by_id.get(target)
        if done_row is not None:
            out_text = _render_done_step_block(done_row, header_note=header_note)
            if target.startswith("step_"):
                alt_fact = "fact_" + target[5:]
                alt_step = str(fact_to_step.get(alt_fact) or "").strip()
                if alt_step and alt_step != target and alt_step in done_by_id:
                    out_text += (
                        "\n\n"
                        + _render_done_step_block(
                            done_by_id[alt_step],
                            header_note=f"[附：{alt_fact} 实际由 {alt_step} 产出，以下一并附上 {alt_step} 的记录]",
                        )
                    )
            return out_text
        return f"{target} 不在当前 open 或已结案 steps 中（ID 不存在）。"
    action = str(row.get("action") or "")
    if not row.get("running"):
        return (
            (f"{header_note}\n" if header_note else "")
            + f"step: {target}\n"
            + "state: open (排队待执行，尚无 Worker 认领)\n"
            + f"action: {action}"
        )
    worker_id = str(row.get("worker_id") or "")
    agent_state_dir_str = str(row.get("agent_state_dir") or "").strip()
    live_jsonl: dict[str, Any] = {}
    if agent_state_dir_str:
        live_jsonl = _extract_pi_jsonl_progress(Path(agent_state_dir_str))
    tool_calls = max(
        int(row.get("tool_calls") or 0),
        int(live_jsonl.get("tool_calls") or 0),
    )
    started_at = float(row.get("started_at") or live_jsonl.get("started_ts") or 0.0)
    if started_at > 0:
        elapsed_s = max(0.0, time.time() - started_at)
    else:
        elapsed_s = max(0.0, float(row.get("elapsed_seconds") or 0.0))
    traces = list(live_jsonl.get("recent_traces") or row.get("recent_traces") or [])
    latest_note = str(live_jsonl.get("latest_note") or row.get("latest_note") or "").strip()
    lines = []
    if header_note:
        lines.append(header_note)
    lines.extend([
        f"step: {target}",
        f"state: running (Worker {worker_id or 'active'} 正在执行中，已运行 {elapsed_s:.0f}s，已执行 {tool_calls} 次工具调用)",
        f"action: {action}",
    ])
    if traces:
        lines.append("recent_tool_calls:")
        for idx, tr in enumerate(traces[-15:], 1):
            if not isinstance(tr, Mapping):
                continue
            tname = str(tr.get("tool") or "tool")
            cmd = str(tr.get("command") or "")
            out = str(tr.get("output") or "")
            lines.append(f"  [{idx}] {tname}: {cmd}")
            if out:
                lines.append(f"      -> {out}")
    else:
        lines.append("recent_tool_calls: (尚无已完成工具输出)")
    if latest_note:
        lines.append(f"latest_progress: {latest_note}")
    return "\n".join(lines)


def print_peek_step() -> None:
    intent_id = str(os.environ.get("MUTEKI_DECIDE_PEEK_STEP_ID") or "").strip()
    graph_path = Path(os.environ.get("MUTEKI_DECIDE_GRAPH_PATH") or "")
    try:
        graph = json.loads(graph_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        graph = {"open_steps": []}
    print(render_peek_step(intent_id, graph), end="")


def print_decide_preview() -> None:
    draft_path = Path(os.environ.get("MUTEKI_DECIDE_DRAFT_PATH") or "")
    graph_path = Path(os.environ.get("MUTEKI_DECIDE_GRAPH_PATH") or "")
    try:
        draft = json.loads(draft_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        draft = {"committed": False, "operations": []}
    try:
        graph = json.loads(graph_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        graph = {
            "schema": "muteki.ctf-decide-graph.v1",
            "max_intents": int(os.environ.get("MUTEKI_DECIDE_MAX_STEPS") or 4),
            "graph_seq": 0,
            "fact_id_map": {},
            "facts": [],
            "goals": [],
            "open_steps": [],
            "cancelled": [],
        }
    print(render_decide_preview(draft, graph), end="")


def _materialise_draft(
    draft: dict[str, Any], *, max_intents: int,
    fact_id_map: dict[str, int] | None = None,
    shared_graph: Any = None,
    challenger: bool = False,
) -> ReasonResult:
    operations = draft.get("operations")
    if not draft.get("committed") or not isinstance(operations, list):
        return _failure(
            PlannerFailureKind.INVALID_PLAN,
            "Pi did not commit a valid planner draft",
            status="received",
        )

    sim = simulate_draft(
        operations,
        max_intents=max_intents,
        fact_id_map=fact_id_map,
        shared_graph=shared_graph,
        draft_id=str(draft.get("draft_id") or "").strip(),
        challenger=challenger,
    )
    yielding = any(
        isinstance(op, dict) and op.get("op") == "commit"
        and op.get("yield_visit") is True for op in operations
    )
    if yielding:
        revised_refs = {str(value) for item in sim.supersede_facts
                        for value in (item["fact_id"], item["fact_seq"])}
        remaining = (
            _parse_preview_open_steps(
                shared_graph.to_ctf_graph_yaml(), shared_graph=shared_graph
            )
            if shared_graph is not None else []
        )
        if (
            getattr(getattr(shared_graph, "challenge", None), "mode", "ctf") != "ctf"
            or sim.goal_met
            or sim.intents
            or not sim.progress_summary
            or any(row["id"] not in sim.supersede
                   and not any(str(source) in revised_refs for source in row.get("from") or [])
                   for row in remaining)
        ):
            return _failure(
                PlannerFailureKind.INVALID_PLAN,
                "yield_visit requires CTF, an explanation, and no remaining Step or completed Goal",
                status="received",
            )
    try:
        planning_graph_seq = int(draft.get("graph_seq"))
    except (TypeError, ValueError):
        planning_graph_seq = -1
    accepted = sum(1 for row in sim.receipts if row.get("outcome") == "accepted")
    dropped = sum(1 for row in sim.receipts if row.get("outcome") == "dropped")
    planner_failure: PlannerFailure | None = None
    if (
        sim.raw_open_steps > 0
        and not sim.intents
        and not sim.goal_met
        and not yielding
    ):
        current_seq = -1
        if shared_graph is not None and callable(
            getattr(shared_graph, "semantic_graph_watermark", None)
        ):
            try:
                current_seq = int(shared_graph.semantic_graph_watermark() or 0)
            except Exception:
                current_seq = -1
        if planning_graph_seq >= 0 and current_seq > planning_graph_seq:
            planner_failure = PlannerFailure(
                PlannerFailureKind.STALE_PLAN,
                f"Decide graph changed while planning "
                f"(draft graph_seq={planning_graph_seq}, "
                f"current={current_seq}); replan from current Facts",
            )
        else:
            planner_failure = PlannerFailure(
                PlannerFailureKind.INVALID_PLAN,
                "All drafted Steps were rejected; inspect draft receipts "
                "and replan from current Facts",
            )
    if sim.drop_whys:
        supersede_why = "; ".join(sim.drop_whys)
    elif sim.supersede:
        supersede_why = "Reason removed obsolete open Steps"
    else:
        supersede_why = ""

    audit_notes: list[dict[str, Any]] = []
    if sim.receipts:
        audit_notes.append({
            "kind": "draft_receipts",
            "schema": "muteki.decide-draft-receipts.v1",
            "draft_id": sim.draft_id,
            "operations": sim.receipts,
        })
    if yielding:
        audit_notes.append({
            "kind": "yield_visit",
            "schema": "muteki.decide-yield-visit.v1",
            "draft_id": sim.draft_id,
            "summary": sim.progress_summary,
        })

    return ReasonResult(
        goal_met=sim.goal_met,
        intents=sim.intents,
        audit_notes=audit_notes,
        verdict=VERDICT_COMPLETE if sim.goal_met else VERDICT_EXPLORE,
        complete_why=sim.complete_why,
        goal_evidence_facts=sim.goal_evidence_facts,
        progress_summary=sim.progress_summary,
        yield_visit=yielding,
        semantic_dedupe_available=False,
        supersede_intents=sim.supersede,
        supersede_why=supersede_why,
        supersede_facts=sim.supersede_facts,
        sub_goal_operations=sim.sub_goal_operations,
        reprioritize_intents=sim.reprioritize,
        planning_graph_seq=planning_graph_seq,
        planner_failure=planner_failure,
        diagnostics=ReasonDiagnostics(
            response_status="received",
            finish_reason="stop",
            parse_status="parsed",
            parse_detail=(
                f"Pi committed planner operations; draft_id={sim.draft_id or '-'}; "
                f"accepted={accepted} dropped={dropped}"
                + (" yield_visit=true" if yielding else "")
            ),
            raw_intent_count=sim.raw_open_steps,
            parsed_intent_count=len(sim.intents),
        ),
        intent_parse_rejections=sim.parse_rejections,
    )


def _apply_pi_terminal_diagnostics(
    result: ReasonResult,
    *,
    draft: Mapping[str, Any],
    parsed: CliResult,
) -> None:
    """Classify provider termination relative to the durable commit boundary."""
    committed = bool(
        draft.get("committed")
        and isinstance(draft.get("operations"), list)
    )
    if parsed.error and committed:
        result.diagnostics.parse_detail = (
            "Pi committed planner operations; post-commit runtime error: "
            f"{parsed.error}"
        )
    elif parsed.model_truncated and committed:
        result.diagnostics.finish_reason = "length"
        result.diagnostics.parse_detail = (
            "Pi committed planner operations before the provider ended "
            "optional post-commit output at its token limit"
        )
    elif parsed.model_truncated:
        detail = (
            "Pi Decide provider response reached its output token limit "
            "before the planner draft was committed"
        )
        result.planner_failure = PlannerFailure(
            PlannerFailureKind.INVALID_PLAN,
            detail,
        )
        result.diagnostics.finish_reason = "length"
        result.diagnostics.parse_status = "provider_output_limit"
        result.diagnostics.parse_detail = detail
    elif parsed.error:
        detail = (
            "Pi Decide failed before the planner draft was committed: "
            f"{parsed.error}"
        )
        result.planner_failure = PlannerFailure(
            PlannerFailureKind.INVALID_PLAN,
            detail,
        )
        result.diagnostics.parse_detail = detail


async def _record_pi_decide_usage(
    parsed: CliResult,
    *,
    cost: CostController | None,
    run_id: str,
    challenge_id: str,
    model: str,
    usage_id: str,
    generation: int | None,
    status: str,
    credential_id: str = "",
    provider_id: str = "",
    catalog_provider: str | None = None,
) -> None:
    """Settle one actual Pi CLI invocation, including a failed or partial turn.

    The work directory identifies this invocation. A repeated settlement of the
    same invocation uses its same identity, while a new CLI launch has a new
    identity even if its prompt is identical.
    """
    if cost is None or not run_id:
        return
    usage: dict[str, Any] = {
        "source": "pi-decide",
        "status": status,
        "measurement_scope": "invocation",
        "input_includes_cache": True,
    }
    for key in (
        "input_tokens", "output_tokens", "cache_read_tokens",
        "cache_write_tokens", "reasoning_tokens",
    ):
        value = getattr(parsed, key, None)
        if value is not None:
            usage[key] = value
    if catalog_provider:
        usage["catalog_provider"] = catalog_provider
        usage["route_provenance"] = {"source": "invocation_staged_model",
                                     "evidence": ".muteki-agent-state/reason/pi-agent/models.json",
                                     "generation": generation}
    input_tokens = parsed.input_tokens
    output_tokens = parsed.output_tokens
    native_cost = parsed.cost_usd
    if native_cost is not None and math.isfinite(native_cost) and native_cost > 0:
        # Pi's JSONL cost.total is calculated from its local provider model
        # configuration; it is an estimate, not a supplier billing receipt.
        usage["estimated_cost"] = native_cost
        await cost.add_external_usd(
            native_cost,
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            input_tokens=int(input_tokens or 0),
            output_tokens=int(output_tokens or 0),
            usage=usage,
            usage_id=usage_id,
            model=model,
            actor_kind="coordinator",
            generation=generation,
            engine="pi", credential_id=credential_id, provider_id=provider_id,
        )
    elif input_tokens is not None and output_tokens is not None:
        await cost.record(
            model=model,
            input_tokens=int(input_tokens),
            output_tokens=int(output_tokens),
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            usage=usage,
            usage_id=usage_id,
            actor_kind="coordinator",
            generation=generation,
            engine="pi", credential_id=credential_id, provider_id=provider_id,
        )
    else:
        # Keep the original partial/missing fields in the usage row, while the
        # live ledger marks this invocation unpriced. A dollar-budgeted run can
        # then stop dispatching immediately if coverage becomes incomplete.
        await cost.record(
            model=model,
            input_tokens=int(input_tokens or 0),
            output_tokens=int(output_tokens or 0),
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
            usage=usage,
            usage_id=usage_id,
            actor_kind="coordinator",
            generation=generation,
            engine="pi", credential_id=credential_id, provider_id=provider_id,
            estimate_cost=False,
        )


async def _start_planner_mcp_bridge(service: Any, *, mode: str, run_id: str):
    """Reuse configured run-scoped MCP connections for the Pi planning process."""
    catalog = await service.prepare_tools("pi", mode, run_id)
    errors = service.mcp_connection_errors("pi", mode, run_id)
    token = uuid.uuid4().hex
    pending: set[asyncio.Task[Any]] = set()

    async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        task = asyncio.current_task()
        if task is not None:
            pending.add(task)
        status = "200 OK"
        try:
            header = (await reader.readuntil(b"\r\n\r\n")).decode("latin-1")
            lines = header.split("\r\n")
            if lines[0].split()[:2] != ["POST", "/" + token]:
                status = "404 Not Found"
                result = {"isError": True, "content": [{"type": "text", "text": "Unknown MCP endpoint"}]}
            else:
                headers = dict(line.split(":", 1) for line in lines[1:] if ":" in line)
                length = int(next((value for key, value in headers.items() if key.lower() == "content-length"), "0"))
                request = json.loads(await reader.readexactly(length))
                result = await service.invoke(
                    "pi", str(request.get("name") or ""), request.get("arguments") or {},
                    mode=mode, scope=run_id,
                )
        except Exception as exc:
            result = {"isError": True, "content": [{"type": "text", "text": f"{type(exc).__name__}: {exc}"}]}
        finally:
            if task is not None:
                pending.discard(task)
        try:
            body = json.dumps(result, ensure_ascii=False).encode("utf-8")
            writer.write(f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body)
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    server = await asyncio.start_server(handle, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    config = {
        "endpoint": f"http://127.0.0.1:{port}/{token}",
        "tools": [{"name": item["name"], "description": item.get("description", ""),
                   "input_schema": item.get("input_schema") or {"type": "object", "properties": {}}}
                  for item in catalog],
        "connection_errors": errors,
    }
    return server, pending, config


async def run_ctf_pi_reason(
    *,
    graph_summary: str,
    max_intents: int,
    model: str,
    account_root: str | Path | None,
    account_id: str,
    state_root: str | Path,
    shared_graph: Any,
    mode: str = "ctf",
    cost: CostController | None = None,
    run_id: str = "",
    challenge_id: str = "",
    generation: int | None = None,
    cost_budget_usd: float | None = None,
    reasoning_effort: str = "default",
    live_solvers: Mapping[str, Any] | None = None,
    challenger: bool = False,
    plugin_service: Any = None,
) -> ReasonResult:
    """Run one stateless Pi turn and materialise its committed operations."""
    state_dir = Path(state_root).expanduser().resolve()
    state_dir.mkdir(parents=True, exist_ok=True)
    work_dir = (
        state_dir.parent.parent
        if state_dir.parent.name == ".muteki-agent-state"
        else state_dir
    )
    usage_id = f"pi-decide:{run_id}:{int(time.time() * 1000)}"
    plugin_server = None
    plugin_tasks: set[asyncio.Task[Any]] = set()
    try:
        draft_path = state_dir / "draft.json"
        draft_path.write_text(
            json.dumps({"committed": False, "operations": []}),
            encoding="utf-8",
        )
        graph_path = state_dir / "graph.json"
        graph_path.write_text(
            json.dumps(
                build_decide_preview_graph(
                    shared_graph,
                    max_intents=max_intents,
                    live_solvers=live_solvers,
                    agent_state_root=state_dir.parent,
                ),
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        driver = PiDriver()
        runtime = runtime_env_for_engine(
            "pi",
            account_root=account_root,
            account_id=account_id,
            env=os.environ,
            agent_state_dir=state_dir / "pi-agent",
            model=model,
        )
        from muteki.core.usage_routes import staged_pi_provider
        from muteki.solver.credential_accounts import account_credential_id, system_credential_id
        usage_credential = account_credential_id(runtime.account_id) if runtime.account_id else system_credential_id("pi")
        usage_provider = ""
        if runtime.account_id and account_root is not None:
            try:
                usage_provider = (Path(account_root) / runtime.account_id / "PROVIDER").read_text().strip()
            except FileNotFoundError:
                pass
        usage_catalog_provider = None
        env = os.environ.copy()
        env.update(driver.env_extra())
        env.update(runtime.env)
        pythonpath = os.pathsep.join(
            item for item in (_REPO_ROOT.as_posix(), env.get("PYTHONPATH") or "")
            if item
        )
        env["PYTHONPATH"] = pythonpath
        env["MUTEKI_DECIDE_DRAFT_PATH"] = str(draft_path)
        env["MUTEKI_DECIDE_GRAPH_PATH"] = str(graph_path)
        env["MUTEKI_DECIDE_PYTHON"] = sys.executable
        env["MUTEKI_DECIDE_MAX_STEPS"] = str(max(-1, int(max_intents)))
        env["MUTEKI_DECIDE_CHALLENGER"] = "1" if challenger else "0"
        plugin_tool_names: list[str] = []
        native_components: list[str] = []
        if plugin_service is not None:
            plugin_service.stage_worker(work_dir, engine="pi", mode=mode)
            native_components = plugin_service.stage_native_worker(work_dir, engine="pi", mode=mode, env=env)
            plugin_server, plugin_tasks, plugin_config = await _start_planner_mcp_bridge(
                plugin_service, mode=mode, run_id=run_id,
            )
            plugin_path = state_dir / "plugins.json"
            plugin_path.write_text(json.dumps(plugin_config, ensure_ascii=False), encoding="utf-8")
            env["MUTEKI_DECIDE_PLUGIN_PATH"] = str(plugin_path)
            plugin_tool_names = [item["name"] for item in plugin_config["tools"]]
            if plugin_config["connection_errors"]:
                graph_summary += "\n\nConfigured MCP connection errors:\n" + json.dumps(plugin_config["connection_errors"], ensure_ascii=False)
        else:
            env.pop("MUTEKI_DECIDE_PLUGIN_PATH", None)

        provider = str(env.get("MUTEKI_PI_PROVIDER") or "").strip()
        # A positive dollar budget preflights the explicit planner model. An
        # ambient Pi default must not replace that model after preflight.
        # Unlimited Runs retain their established environment-first behavior.
        selected_model = str(
            (model if cost_budget_usd is not None and cost_budget_usd > 0 else None)
            or env.get("MUTEKI_PI_MODEL") or model or ""
        ).strip()
        usage_catalog_provider = staged_pi_provider(state_dir / "pi-agent", selected_model)
        system_prompt = _pi_system_prompt(mode=mode, challenger=challenger)
        argv = [
            driver.bin,
            "-p",
            "--mode",
            "json",
            "--no-session",
            *(["--no-extensions", "--no-skills"] if plugin_service is None else []),
            "--no-context-files",
            "--no-builtin-tools",
            "--extension",
            str(_EXTENSION),
            *([] if native_components else [
                "--tools",
                ",".join([_CHALLENGER_TOOLS if challenger else _TOOLS, *plugin_tool_names]),
            ]),
        ]
        if provider:
            argv.extend(["--provider", provider])
        if selected_model:
            argv.extend(["--model", selected_model])
        argv.extend(["--system-prompt", system_prompt])
        argv = apply_reasoning_effort(
            argv,
            engine="pi",
            reasoning_effort=reasoning_effort,
        )
        check_process_launch(
            argv, cwd=str(work_dir), env=env,
            stdin_text=str(graph_summary), source="pi-decide")

        proc = None
        communicate_task = None

        async def _stop_and_collect() -> tuple[bytes, bytes]:
            # 主进程退出不代表进程组已空；子进程可能仍持有输出管道。
            if proc is not None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if communicate_task is None:
                return b"", b""
            try:
                return await asyncio.wait_for(
                    asyncio.shield(communicate_task), timeout=5.0
                )
            except asyncio.TimeoutError:
                if proc is not None:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                try:
                    return await asyncio.wait_for(
                        asyncio.shield(communicate_task), timeout=5.0
                    )
                except asyncio.TimeoutError:
                    communicate_task.cancel()
                    await asyncio.gather(communicate_task, return_exceptions=True)
                    return b"", b""

        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=str(work_dir),
                env=env,
                start_new_session=True,
            )
            communicate_task = asyncio.create_task(
                proc.communicate(str(graph_summary).encode("utf-8"))
            )
            stdout_bytes, stderr_bytes = await asyncio.wait_for(
                asyncio.shield(communicate_task), timeout=300.0
            )
        except asyncio.TimeoutError:
            stdout_bytes, stderr_bytes = await _stop_and_collect()
            parsed = driver.parse(
                stdout_bytes.decode("utf-8", errors="replace"),
                stderr_bytes.decode("utf-8", errors="replace"),
            )
            await _record_pi_decide_usage(
                parsed, cost=cost, run_id=run_id,
                challenge_id=challenge_id, model=selected_model,
                usage_id=usage_id, generation=generation, status="timeout",
                credential_id=usage_credential, provider_id=usage_provider, catalog_provider=usage_catalog_provider,
            )
            return _failure(
                PlannerFailureKind.TIMEOUT,
                "Pi Decide exceeded 300 seconds",
                status="timeout",
                timed_out=True,
                raw_response=parsed.text,
            )
        except asyncio.CancelledError:
            await _stop_and_collect()
            raise
        except Exception as exc:
            stdout_bytes, stderr_bytes = await _stop_and_collect()
            if proc is not None:
                parsed = driver.parse(
                    stdout_bytes.decode("utf-8", errors="replace"),
                    stderr_bytes.decode("utf-8", errors="replace"),
                )
                await _record_pi_decide_usage(
                    parsed, cost=cost, run_id=run_id,
                    challenge_id=challenge_id, model=selected_model,
                    usage_id=usage_id, generation=generation, status="error",
                    credential_id=usage_credential, provider_id=usage_provider, catalog_provider=usage_catalog_provider,
                )
            return _failure(
                PlannerFailureKind.EXCEPTION,
                f"{type(exc).__name__}: {exc}",
                status="error",
            )
        finally:
            if proc is not None and proc.returncode is None:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass

        stdout = stdout_bytes.decode("utf-8", errors="replace")
        stderr = stderr_bytes.decode("utf-8", errors="replace")
        parsed = driver.parse(stdout, stderr)
        try:
            draft = json.loads(draft_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            await _record_pi_decide_usage(
                parsed, cost=cost, run_id=run_id,
                challenge_id=challenge_id, model=selected_model,
                usage_id=usage_id, generation=generation, status="error",
                    credential_id=usage_credential, provider_id=usage_provider, catalog_provider=usage_catalog_provider,
            )
            if proc is not None and proc.returncode not in (None, 0):
                return _failure(
                    PlannerFailureKind.UNAVAILABLE,
                    f"Pi Decide process exited with code {proc.returncode}: "
                    f"{stderr.strip() or parsed.error or 'no diagnostic output'}",
                    status="error",
                    raw_response=parsed.text,
                )
            detail = (
                parsed.error
                or (
                    "Pi Decide provider response reached its output token limit"
                    if parsed.model_truncated else ""
                )
                or f"{type(exc).__name__}: {exc}"
            )
            return _failure(
                PlannerFailureKind.INVALID_PLAN,
                detail,
                status="received" if stdout.strip() else "empty",
                raw_response=parsed.text,
                finish_reason="length" if parsed.model_truncated else "",
            )

        await _record_pi_decide_usage(
            parsed, cost=cost, run_id=run_id,
            challenge_id=challenge_id, model=selected_model,
            usage_id=usage_id, generation=generation,
            credential_id=usage_credential, provider_id=usage_provider, catalog_provider=usage_catalog_provider,
            status=("observed" if draft.get("committed")
                    and isinstance(draft.get("operations"), list)
                    else "error"),
        )
        fact_id_map, _facts, _watermark = _fact_catalog(shared_graph)
        result = _materialise_draft(
            draft,
            max_intents=max_intents,
            fact_id_map=fact_id_map,
            shared_graph=shared_graph,
            challenger=challenger,
        )
        result.diagnostics.raw_response = parsed.text
        result.diagnostics.raw_response_sha256 = hashlib.sha256(
            parsed.text.encode("utf-8", errors="replace")
        ).hexdigest()
        result.diagnostics.response_chars = len(parsed.text)
        result.diagnostics.input_tokens = int(parsed.input_tokens or 0)
        result.diagnostics.output_tokens = int(parsed.output_tokens or 0)
        # `commit` is the transaction boundary. Pi may time out while producing
        # optional prose after the extension has durably committed the draft;
        # that transport error must not turn an already-applied transaction into
        # a failed Reason pass.
        _apply_pi_terminal_diagnostics(result, draft=draft, parsed=parsed)
        return result
    except Exception as exc:
        return _failure(
            PlannerFailureKind.EXCEPTION,
            f"{type(exc).__name__}: {exc}",
            status="error",
        )
    finally:
        if plugin_server is not None:
            plugin_server.close()
            await plugin_server.wait_closed()
        for task in list(plugin_tasks):
            task.cancel()
        if plugin_tasks:
            await asyncio.gather(*plugin_tasks, return_exceptions=True)
