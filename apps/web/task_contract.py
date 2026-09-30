"""Canonical conversational dispatch contract.

``/start`` freezes the operator's raw instruction for Swarm. Structured fields
are copied only when the caller sent them explicitly. Title and category for
the left rail are labeled later by a fire-and-forget Planner call and never
rewrite the task body.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Mapping

from apps.web.dispatch_parse import explicit_category
from muteki.models.solve_graph import (
    CompletionContract,
    TaskAttachment,
    TaskContract,
)
from muteki.solver.gate import normalize_flag_contract
from muteki.pentest.contract import compile_pentest_prompt


def _attachment_contracts(paths: list[Any]) -> list[TaskAttachment]:
    out: list[TaskAttachment] = []
    for raw in paths:
        path = Path(str(raw))
        if not path.exists() or not path.is_file():
            continue
        try:
            size = path.stat().st_size
        except OSError:
            size = 0
        suffix = path.suffix.lower().lstrip(".") or "binary"
        out.append(TaskAttachment(
            path=str(path), name=path.name, size=max(0, int(size)),
            summary=f"{path.name} · {size} bytes · {suffix}",
        ))
    return out


def _flag_contract_inputs(
    root: Mapping[str, Any], challenge: Mapping[str, Any],
) -> tuple[str, str]:
    """Read the two CTF-format inputs once for preflight and final projection."""
    raw_format = str(
        challenge.get("flag_format")
        or challenge.get("flagFormat")
        or root.get("flag_format")
        or root.get("flagFormat")
        or ""
    ).strip()
    wrapper = str(
        challenge.get("flag_format_wrapper")
        or challenge.get("flagWrapper")
        or root.get("flag_format_wrapper")
        or root.get("flagWrapper")
        or ""
    ).strip()
    return raw_format, wrapper


def build_task_contract(
    body: Mapping[str, Any], *, pentest_version: int = 2,
) -> TaskContract:
    """Finalize a contract from explicit operator fields only."""
    root = dict(body or {})
    challenge = dict(root.get("challenge") or {})
    raw = str(root.get("prompt") or challenge.get("description") or "").strip()
    mode = str(challenge.get("mode") or root.get("mode") or "ctf")
    if mode not in {"ctf", "pentest"}:
        raise ValueError(f"unknown dispatch mode {mode!r}")

    title = str(challenge.get("name") or "").strip()
    category = explicit_category(challenge.get("category")) or None
    target = str(challenge.get("target") or root.get("target") or "").strip()
    scope = str(challenge.get("scope") or root.get("scope") or "").strip()

    raw_flag_format, raw_flag_wrapper = _flag_contract_inputs(root, challenge)
    expected_flags = max(1, int(
        challenge.get("expected_flags") or root.get("expected_flags") or 1))
    multi_flag = bool(
        challenge.get("multi_flag")
        if challenge.get("multi_flag") is not None
        else root.get("multi_flag", False))
    if mode == "ctf":
        flag_contract = normalize_flag_contract(raw_flag_format, raw_flag_wrapper)
        flag_format = flag_contract.flag_format
        flag_hint = flag_contract.flag_format_wrapper
        completion = CompletionContract(
            kind="ctf_flag",
            goal="collect valid flags" if multi_flag else "recover one valid flag",
            task_type=category or "misc",
            quantity=expected_flags if (multi_flag or expected_flags > 1) else 1,
            flag_format=flag_format,
            flag_format_hint=flag_hint,
            expected_flags=expected_flags,
            multi_flag=multi_flag,
        )
    else:
        report_goal_mode = challenge.get("report_goal_mode", root.get("report_goal_mode", "automatic"))
        raw_count = challenge.get("expected_findings", root.get("expected_findings"))
        if raw_count is not None and (type(raw_count) is not int or raw_count < 1):
            raise ValueError("expected_findings 必须是正整数")
        pentest = compile_pentest_prompt(
            raw, target=target, scope=scope,
            report_goal_mode=report_goal_mode,
            expected_findings=raw_count,
            version=pentest_version,
        )
        target = pentest.target
        scope = ", ".join(pentest.authorization.scope)
        goal = pentest.goal
        completion = CompletionContract(
            kind="count" if pentest.report_goal_mode == "count" else "outcome",
            goal=goal,
            task_type="authorized_pentest",
            quantity=pentest.expected_findings,
            finding_class="vulnerability_report",
            expected_flags=1,
            outcome_predicate="model_goal_with_evidence",
        )

    return TaskContract(
        raw_instruction=raw,
        mode=mode,  # type: ignore[arg-type]
        title=title,
        category=category,  # type: ignore[arg-type]
        attachments=_attachment_contracts(list(challenge.get("attachments") or [])),
        execution_target=target or None,
        authorization_scope=scope,
        completion_contract=completion,
        pentest_contract=pentest if mode == "pentest" else None,
    )


def project_contract_into_body(
    body: Mapping[str, Any], contract: TaskContract,
) -> dict[str, Any]:
    """Create the legacy ``Challenge`` projection consumed by existing workers."""
    out = dict(body or {})
    challenge = dict(out.get("challenge") or {})
    completion = contract.completion_contract
    challenge["description"] = contract.raw_instruction
    challenge["mode"] = contract.mode
    challenge["attachments"] = [item.path for item in contract.attachments]
    challenge["task_contract"] = contract.model_dump(mode="json")
    if contract.title:
        challenge["name"] = contract.title
    else:
        challenge.pop("name", None)
    if contract.category:
        challenge["category"] = contract.category
    else:
        challenge.pop("category", None)
    if contract.execution_target:
        challenge["target"] = contract.execution_target
    else:
        challenge.pop("target", None)
    if contract.authorization_scope:
        challenge["scope"] = contract.authorization_scope
    else:
        challenge.pop("scope", None)
    out["prompt"] = contract.raw_instruction
    out["mode"] = contract.mode
    out["task_contract"] = contract.model_dump(mode="json")
    if contract.mode == "pentest":
        challenge["goal"] = completion.goal
        challenge["pentest_contract"] = contract.pentest_contract.model_dump(mode="json") if contract.pentest_contract else None
        challenge["report_goal_mode"] = contract.pentest_contract.report_goal_mode if contract.pentest_contract else "automatic"
        challenge["expected_findings"] = contract.pentest_contract.expected_findings if contract.pentest_contract else None
    else:
        challenge["expected_flags"] = max(1, completion.expected_flags)
        challenge["multi_flag"] = bool(completion.multi_flag)
        challenge["flag_format"] = completion.flag_format
        if ("allow_operator_input" not in challenge
                and "allow_operator_input" not in out):
            challenge["allow_operator_input"] = False
        if completion.flag_format_hint:
            challenge["flag_format_hint"] = completion.flag_format_hint
    out["challenge"] = challenge
    return out


def prepare_dispatch_contract(
    body: Mapping[str, Any],
    *, pentest_version: int = 2,
) -> tuple[dict[str, Any], TaskContract]:
    """Freeze the raw instruction. Planner labeling is not part of this gate."""
    root = dict(body or {})
    challenge = dict(root.get("challenge") or {})
    mode = str(challenge.get("mode") or root.get("mode") or "ctf")
    if mode not in {"ctf", "pentest"}:
        raise ValueError(f"unknown dispatch mode {mode!r}")
    if mode == "ctf":
        raw_flag_format, raw_flag_wrapper = _flag_contract_inputs(root, challenge)
        normalize_flag_contract(raw_flag_format, raw_flag_wrapper)
    contract = build_task_contract(root, pentest_version=pentest_version)
    return project_contract_into_body(root, contract), contract
