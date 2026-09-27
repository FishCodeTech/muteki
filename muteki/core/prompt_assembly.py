from __future__ import annotations

import hashlib
import math
import os
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Optional

_CONTEXT_MARKER = "{context}"
_SMALL_OUTPUT_ROLES = frozenset({"btw", "dispatch", "judge", "metadata", "title"})
_DEFAULT_CONTEXT_WINDOW_TOKENS = 262_144


def _known_model_context_window(model: str) -> Optional[int]:
    return _DEFAULT_CONTEXT_WINDOW_TOKENS if str(model or "").strip() else None


def _driver_context_window(driver: Any) -> Optional[int]:
    for name in ("context_window_tokens", "context_window", "max_context_tokens"):
        value = _positive_int(getattr(driver, name, None))
        if value is not None:
            return value
    return None


class AdmissionError(ValueError):
    pass


class PromptBudgetExceeded(ValueError):
    pass


@dataclass(frozen=True)
class PromptPart:
    name: str
    text: str
    omission_group: str = ""
    item_id: str = ""
    external_files: tuple[str, ...] = ()
    secret_tainted: bool = False


@dataclass(frozen=True)
class PromptBudget:
    role: str
    model: str
    context_window_tokens: int
    output_reserve_tokens: int
    safety_margin_tokens: int
    input_budget_tokens: int


@dataclass(frozen=True)
class PromptOmission:
    group: str
    item_count: int
    source_chars: int
    item_ids: tuple[str, ...] = ()


@dataclass(frozen=True)
class CompiledPrompt:
    prompt: str
    required_complete: bool
    omissions: tuple[PromptOmission, ...]
    external_files: tuple[str, ...]
    estimated_host_tokens: int
    host_prompt_chars: int
    host_prompt_digest: str
    secret_tainted: bool
    section_source_chars: dict[str, int]
    section_delivered_chars: dict[str, int]


def admit_text(
    value: Any,
    *,
    field: str,
    max_chars: int,
    allow_empty: bool = False,
    strip: bool = False,
) -> str:
    text = str(value or "")
    if strip:
        text = text.strip()
    if not text and not allow_empty:
        raise AdmissionError(f"{field} cannot be empty")
    if len(text) > max_chars:
        raise AdmissionError(
            f"{field} exceeds the {max_chars}-character admission limit"
        )
    return text


def estimate_host_tokens(text: str) -> int:
    body = str(text or "")
    if not body:
        return 0
    ascii_chars = sum(1 for char in body if ord(char) < 128)
    non_ascii_chars = len(body) - ascii_chars
    estimate = math.ceil(ascii_chars / 2.5) + math.ceil(non_ascii_chars * 1.5)
    return max(1, estimate + 8)


def _positive_int(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return None
    return parsed if parsed > 0 else None


def _configured_int(role: str, suffix: str) -> Optional[int]:
    role_key = role.upper().replace("-", "_")
    return _positive_int(
        os.environ.get(f"MUTEKI_PROMPT_{suffix}_{role_key}")
        or os.environ.get(f"MUTEKI_PROMPT_{suffix}")
    )


def resolve_prompt_budget(
    model: str,
    profile: Optional[Mapping[str, Any]] = None,
    driver: Any = None,
    role: str = "worker",
) -> PromptBudget:
    values = dict(profile or {})
    context_window = next(
        (
            value
            for value in (
                _positive_int(values.get("context_window_tokens")),
                _positive_int(values.get("context_window")),
                _positive_int(values.get("max_context_tokens")),
                _configured_int(role, "CONTEXT_TOKENS"),
                _known_model_context_window(model),
                _driver_context_window(driver),
                _DEFAULT_CONTEXT_WINDOW_TOKENS,
            )
            if value is not None
        )
    )
    default_reserve = (
        min(4_096, max(512, context_window // 16))
        if role in _SMALL_OUTPUT_ROLES
        else min(8_192, max(2_048, context_window // 8))
    )
    output_reserve = next(
        (
            value
            for value in (
                _positive_int(values.get("output_reserve_tokens")),
                _positive_int(values.get("max_output_tokens")),
                _positive_int(getattr(driver, "output_reserve_tokens", None)),
                _configured_int(role, "OUTPUT_RESERVE_TOKENS"),
                default_reserve,
            )
            if value is not None
        )
    )
    safety_margin = next(
        (
            value
            for value in (
                _positive_int(values.get("prompt_safety_margin_tokens")),
                _configured_int(role, "SAFETY_MARGIN_TOKENS"),
                max(256, math.ceil(context_window * 0.05)),
            )
            if value is not None
        )
    )
    input_budget = context_window - output_reserve - safety_margin
    if input_budget < 1_024:
        raise PromptBudgetExceeded(
            f"resolved {role} prompt budget is too small for model {model or '(unknown)'}"
        )
    return PromptBudget(
        role=role,
        model=str(model or ""),
        context_window_tokens=context_window,
        output_reserve_tokens=output_reserve,
        safety_margin_tokens=safety_margin,
        input_budget_tokens=input_budget,
    )


def _render_prompt(fixed_template: str, parts: Sequence[PromptPart], notices: str) -> str:
    context = "\n".join(part.text for part in parts if part.text)
    if notices:
        context = f"{context}\n{notices}" if context else notices
    template = str(fixed_template or "")
    if _CONTEXT_MARKER in template:
        return template.replace(_CONTEXT_MARKER, context, 1)
    if not context:
        return template
    return f"{template}\n{context}" if template else context


def _omissions(parts: Sequence[PromptPart]) -> tuple[PromptOmission, ...]:
    groups: dict[str, list[PromptPart]] = {}
    for part in parts:
        group = part.omission_group or part.name
        groups.setdefault(group, []).append(part)
    return tuple(
        PromptOmission(
            group=group,
            item_count=len(items),
            source_chars=sum(len(item.text) for item in items),
            item_ids=tuple(item.item_id for item in items if item.item_id),
        )
        for group, items in groups.items()
    )


def _omission_notice(rows: Sequence[PromptOmission]) -> str:
    if not rows:
        return ""
    details = "; ".join(
        f"{row.group}: {row.item_count} item(s), {row.source_chars} character(s)"
        for row in rows
    )
    return f"[Context intentionally omitted: {details}.]"


def _section_chars(parts: Sequence[PromptPart]) -> dict[str, int]:
    out: dict[str, int] = {}
    for part in parts:
        out[part.name] = out.get(part.name, 0) + len(part.text)
    return out


def compile_prompt(
    fixed_template: str,
    required_sections: Sequence[PromptPart],
    optional_sections: Sequence[PromptPart],
    input_budget: int | PromptBudget,
) -> CompiledPrompt:
    budget_tokens = (
        input_budget.input_budget_tokens
        if isinstance(input_budget, PromptBudget)
        else int(input_budget)
    )
    if budget_tokens <= 0:
        raise PromptBudgetExceeded("input budget must be positive")
    required = tuple(required_sections)
    optional = tuple(optional_sections)
    if any(not part.text.strip() for part in required):
        missing = ", ".join(part.name for part in required if not part.text.strip())
        raise PromptBudgetExceeded(f"required prompt section is empty: {missing}")
    included_optional: list[PromptPart] = []
    omitted_optional: list[PromptPart] = []
    required_prompt = _render_prompt(fixed_template, required, "")
    if estimate_host_tokens(required_prompt) > budget_tokens:
        raise PromptBudgetExceeded(
            "required prompt sections exceed the resolved input budget"
        )
    for part in optional:
        if not part.text.strip():
            continue
        candidate = [*required, *included_optional, part]
        prompt = _render_prompt(
            fixed_template,
            candidate,
            _omission_notice(_omissions(omitted_optional)),
        )
        if estimate_host_tokens(prompt) <= budget_tokens:
            included_optional.append(part)
        else:
            omitted_optional.append(part)
    while True:
        omissions = _omissions(omitted_optional)
        prompt = _render_prompt(
            fixed_template,
            [*required, *included_optional],
            _omission_notice(omissions),
        )
        estimated_tokens = estimate_host_tokens(prompt)
        if estimated_tokens <= budget_tokens:
            break
        if not included_optional:
            raise PromptBudgetExceeded(
                "required prompt sections and omission notice exceed the input budget"
            )
        omitted_optional.insert(0, included_optional.pop())
    included = (*required, *included_optional)
    external_files = tuple(
        dict.fromkeys(path for part in included for path in part.external_files)
    )
    secret_tainted = any(part.secret_tainted for part in included)
    digest = "" if secret_tainted else hashlib.sha256(prompt.encode("utf-8")).hexdigest()
    source_chars = _section_chars((*required, *optional))
    delivered_chars = _section_chars(included)
    assembly_overhead = max(0, len(prompt) - sum(delivered_chars.values()))
    source_chars["assembly-overhead"] = assembly_overhead
    delivered_chars["assembly-overhead"] = assembly_overhead
    return CompiledPrompt(
        prompt=prompt,
        required_complete=True,
        omissions=omissions,
        external_files=external_files,
        estimated_host_tokens=estimated_tokens,
        host_prompt_chars=len(prompt),
        host_prompt_digest=digest,
        secret_tainted=secret_tainted,
        section_source_chars=source_chars,
        section_delivered_chars=delivered_chars,
    )
