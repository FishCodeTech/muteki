"""Normalized Conversation user-input questions and structured answers (C22).

Adapters emit Provider-specific pending payloads; this module normalizes them
into a shared ``questions[]`` read model and validates / maps structured
answers for ``conversation.user_input.resolve``.
"""

from __future__ import annotations

from typing import Any, Optional

QUESTION_KINDS = frozenset({
    "header",
    "single_select",
    "multi_select",
    "free_text",
    "boolean",
    "number",
})


def _as_dict(value: Any) -> dict[str, Any]:
    return dict(value) if isinstance(value, dict) else {}


def _as_str(value: Any, default: str = "") -> str:
    if value is None:
        return default
    text = str(value).strip()
    return text if text else default


def _bool(value: Any, default: bool = False) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return bool(value)
    text = str(value).strip().lower()
    if text in {"1", "true", "yes", "y", "on"}:
        return True
    if text in {"0", "false", "no", "n", "off"}:
        return False
    return default


def _normalize_option(raw: Any) -> Optional[dict[str, Any]]:
    if isinstance(raw, str):
        text = raw.strip()
        if not text:
            return None
        return {"value": text, "label": text, "recommended": False}
    if not isinstance(raw, dict):
        return None
    value = _as_str(
        raw.get("value")
        or raw.get("id")
        or raw.get("label")
        or raw.get("title")
        or raw.get("name")
    )
    if not value:
        return None
    label = _as_str(
        raw.get("label") or raw.get("title") or raw.get("name") or value,
        value,
    )
    recommended = _bool(
        raw.get("recommended")
        if "recommended" in raw
        else raw.get("isRecommended")
        if "isRecommended" in raw
        else raw.get("is_recommended"),
        False,
    )
    return {"value": value, "label": label, "recommended": recommended}


def _kind_from_flags(
    *,
    has_options: bool,
    multi: bool,
    free_text: bool,
    header: bool,
    schema_type: str = "",
) -> str:
    if header:
        return "header"
    schema = schema_type.strip().lower()
    if schema == "boolean":
        return "boolean"
    if schema in {"number", "integer"}:
        return "number"
    if schema == "array" or multi:
        return "multi_select" if has_options or schema == "array" else "free_text"
    if has_options:
        return "single_select"
    if free_text or schema in {"", "string"}:
        return "free_text"
    return "free_text"


def normalize_question(raw: Any, *, index: int = 0) -> Optional[dict[str, Any]]:
    """Normalize one question dict into the Conversation read model."""
    if isinstance(raw, str):
        text = raw.strip()
        if not text:
            return None
        return {
            "question_id": f"q{index}",
            "kind": "free_text",
            "prompt": text,
            "required": True,
            "options": [],
            "allow_free_text": True,
            "placeholder": "",
        }
    if not isinstance(raw, dict):
        return None

    header_text = _as_str(raw.get("header"))
    prompt = _as_str(
        raw.get("prompt")
        or raw.get("question")
        or raw.get("message")
        or raw.get("label")
        or raw.get("title")
        or header_text
    )
    question_id = _as_str(
        raw.get("question_id")
        or raw.get("id")
        or raw.get("name")
        or raw.get("key"),
        f"q{index}",
    )
    raw_options = raw.get("options") or raw.get("choices") or []
    options: list[dict[str, Any]] = []
    if isinstance(raw_options, list):
        for item in raw_options:
            option = _normalize_option(item)
            if option is not None:
                options.append(option)

    explicit_kind = _as_str(raw.get("kind") or raw.get("type")).lower()
    is_header = (
        explicit_kind == "header"
        or bool(header_text and not _as_str(raw.get("question") or raw.get("prompt")))
        or _bool(raw.get("isHeader") or raw.get("is_header"), False)
    )
    multi = _bool(
        raw.get("multi")
        or raw.get("multiple")
        or raw.get("multiSelect")
        or raw.get("multi_select")
        or raw.get("allow_multiple"),
        False,
    )
    allow_free_text = _bool(
        raw.get("allow_free_text")
        if "allow_free_text" in raw
        else raw.get("isOther")
        if "isOther" in raw
        else raw.get("allowOther"),
        not options and not is_header,
    )
    schema_type = _as_str(raw.get("schema_type") or raw.get("value_type"))
    if explicit_kind in QUESTION_KINDS:
        kind = explicit_kind
    else:
        kind = _kind_from_flags(
            has_options=bool(options),
            multi=multi,
            free_text=allow_free_text,
            header=is_header,
            schema_type=schema_type,
        )
    if kind == "header":
        required = False
        allow_free_text = False
    else:
        required = _bool(
            raw.get("required") if "required" in raw else None,
            True,
        )
    if not prompt and kind == "header":
        prompt = header_text or question_id
    if not prompt:
        prompt = question_id
    return {
        "question_id": question_id,
        "kind": kind,
        "prompt": prompt,
        "required": required,
        "options": options,
        "allow_free_text": allow_free_text if kind != "header" else False,
        "placeholder": _as_str(raw.get("placeholder")),
    }


def questions_from_codex_params(params: dict[str, Any]) -> list[dict[str, Any]]:
    """Map Codex ``requestUserInput`` / MCP elicitation params → questions[]."""
    raw_questions = params.get("questions")
    if isinstance(raw_questions, list) and raw_questions:
        out: list[dict[str, Any]] = []
        for index, item in enumerate(raw_questions):
            normalized = normalize_question(item, index=index)
            if normalized is not None:
                out.append(normalized)
        if out:
            return out
    return questions_from_schema(
        params.get("requestedSchema") or {},
        title=_as_str(params.get("message")),
    )


def questions_from_schema(
    schema: Any,
    *,
    title: str = "",
) -> list[dict[str, Any]]:
    """Map JSON Schema ``requestedSchema`` properties → questions[]."""
    row = _as_dict(schema)
    properties = row.get("properties")
    if not isinstance(properties, dict) or not properties:
        if title:
            return [{
                "question_id": "answer",
                "kind": "free_text",
                "prompt": title,
                "required": True,
                "options": [],
                "allow_free_text": True,
                "placeholder": "",
            }]
        return []
    required_keys = {
        str(item)
        for item in (row.get("required") or [])
        if item is not None and str(item).strip()
    }
    out: list[dict[str, Any]] = []
    for index, (key, prop) in enumerate(properties.items()):
        if not isinstance(prop, dict):
            continue
        enum_values = prop.get("enum") if isinstance(prop.get("enum"), list) else []
        options = [
            opt for opt in (_normalize_option(item) for item in enum_values)
            if opt is not None
        ]
        prop_type = _as_str(prop.get("type"), "string").lower()
        multi = prop_type == "array"
        kind = _kind_from_flags(
            has_options=bool(options),
            multi=multi,
            free_text=not options,
            header=False,
            schema_type=prop_type,
        )
        prompt = _as_str(
            prop.get("title") or prop.get("description") or key,
            str(key),
        )
        out.append({
            "question_id": str(key),
            "kind": kind,
            "prompt": prompt,
            "required": str(key) in required_keys if required_keys else True,
            "options": options,
            "allow_free_text": kind == "free_text" or (
                bool(options) and _bool(prop.get("allow_free_text"), False)
            ),
            "placeholder": _as_str(prop.get("placeholder")),
            "schema_type": prop_type,
        })
        _ = index
    if title and out and out[0]["kind"] != "header":
        # Keep request-level message as a non-interactive header when present.
        out.insert(0, {
            "question_id": "__title__",
            "kind": "header",
            "prompt": title,
            "required": False,
            "options": [],
            "allow_free_text": False,
            "placeholder": "",
        })
    return out


def normalize_pending_user_input(payload: dict[str, Any]) -> dict[str, Any]:
    """Ensure a pending user-input payload has normalized ``questions``."""
    pending = dict(payload)
    request_id = _as_str(pending.get("request_id"))
    if request_id:
        pending["request_id"] = request_id

    existing = pending.get("questions")
    questions: list[dict[str, Any]] = []
    if isinstance(existing, list) and existing:
        for index, item in enumerate(existing):
            normalized = normalize_question(item, index=index)
            if normalized is not None:
                questions.append(normalized)

    if not questions:
        native = _as_dict(pending.get("native"))
        schema = pending.get("schema") or native.get("requestedSchema")
        if isinstance(schema, dict) and schema.get("properties"):
            questions = questions_from_schema(
                schema,
                title=_as_str(
                    pending.get("title")
                    or pending.get("message")
                    or pending.get("question")
                ),
            )
        elif native.get("questions") or native.get("requestedSchema"):
            questions = questions_from_codex_params(native)
        else:
            legacy = _as_str(
                pending.get("question")
                or pending.get("prompt")
                or pending.get("message")
                or pending.get("title")
            )
            raw_options = pending.get("options") or pending.get("choices") or []
            options = []
            if isinstance(raw_options, list):
                options = [
                    opt for opt in (_normalize_option(item) for item in raw_options)
                    if opt is not None
                ]
            if legacy or options:
                questions = [{
                    "question_id": "answer",
                    "kind": "single_select" if options else "free_text",
                    "prompt": legacy or "Agent 请求补充信息",
                    "required": True,
                    "options": options,
                    "allow_free_text": True,
                    "placeholder": "",
                }]

    pending["questions"] = questions
    if questions:
        first_interactive = next(
            (q for q in questions if q.get("kind") != "header"),
            questions[0],
        )
        pending.setdefault("question", first_interactive.get("prompt"))
        pending.setdefault("message", first_interactive.get("prompt"))
        if first_interactive.get("options"):
            pending.setdefault("options", first_interactive.get("options"))
    title = _as_str(pending.get("title") or pending.get("message"))
    if title:
        pending["title"] = title
    return pending


def _answer_values(entry: Any) -> tuple[list[str], str]:
    if entry is None:
        return [], ""
    if isinstance(entry, str):
        text = entry.strip()
        return ([text] if text else []), text
    if isinstance(entry, list):
        values = [_as_str(item) for item in entry if _as_str(item)]
        return values, ""
    if not isinstance(entry, dict):
        return [], ""
    values_raw = entry.get("values")
    if values_raw is None:
        values_raw = entry.get("answers")
    values: list[str] = []
    if isinstance(values_raw, list):
        values = [_as_str(item) for item in values_raw if _as_str(item)]
    elif values_raw is not None and _as_str(values_raw):
        values = [_as_str(values_raw)]
    text = _as_str(entry.get("text"))
    return values, text


def expand_legacy_text_answers(
    pending: dict[str, Any],
    text: str,
) -> dict[str, dict[str, Any]]:
    """Map legacy single ``text`` onto the first interactive question."""
    cleaned = text.strip()
    if not cleaned:
        return {}
    questions = pending.get("questions") if isinstance(pending.get("questions"), list) else []
    for question in questions:
        if not isinstance(question, dict):
            continue
        if question.get("kind") == "header":
            continue
        qid = _as_str(question.get("question_id"))
        if not qid:
            continue
        return {qid: {"values": [cleaned], "text": cleaned}}
    return {"answer": {"values": [cleaned], "text": cleaned}}


def validate_user_input_answers(
    pending: dict[str, Any],
    answers: dict[str, Any],
    *,
    decision: str = "submit",
) -> dict[str, dict[str, Any]]:
    """Validate structured answers against pending questions.

    Raises ``ValueError`` with a stable ``code|message`` prefix on failure.
    """
    decision_norm = (decision or "submit").strip().lower()
    if decision_norm == "cancel":
        return {}
    if decision_norm != "submit":
        raise ValueError(
            "conversation.user_input.invalid_decision|"
            f"unsupported decision: {decision_norm}"
        )

    normalized_pending = normalize_pending_user_input(pending)
    questions = [
        q for q in (normalized_pending.get("questions") or [])
        if isinstance(q, dict)
    ]
    by_id = {
        _as_str(q.get("question_id")): q
        for q in questions
        if _as_str(q.get("question_id"))
    }
    cleaned: dict[str, dict[str, Any]] = {}
    raw_answers = answers if isinstance(answers, dict) else {}
    for qid, entry in raw_answers.items():
        key = _as_str(qid)
        if not key:
            continue
        if key not in by_id:
            raise ValueError(
                "conversation.user_input.unknown_question|"
                f"unknown question_id: {key}"
            )
        question = by_id[key]
        if question.get("kind") == "header":
            raise ValueError(
                "conversation.user_input.header_answered|"
                f"header question cannot be answered: {key}"
            )
        values, text = _answer_values(entry)
        if not values and text:
            values = [text]
        cleaned[key] = {"values": values, "text": text}

    for question in questions:
        qid = _as_str(question.get("question_id"))
        if question.get("kind") == "header" or not question.get("required", True):
            continue
        entry = cleaned.get(qid) or {}
        values = list(entry.get("values") or [])
        text = _as_str(entry.get("text"))
        if not values and not text:
            raise ValueError(
                "conversation.user_input.required|"
                f"required question unanswered: {qid}"
            )
    return cleaned


def flatten_answers_text(answers: dict[str, dict[str, Any]]) -> str:
    """Join structured answers into a legacy single text string."""
    parts: list[str] = []
    for qid, entry in answers.items():
        values = [str(v) for v in (entry.get("values") or []) if str(v).strip()]
        text = _as_str(entry.get("text"))
        body = ", ".join(values) if values else text
        if body:
            parts.append(f"{qid}: {body}" if len(answers) > 1 else body)
    return "\n".join(parts)


def answers_for_codex_tool(
    answers: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Codex tool user-input response shape."""
    return {
        "answers": {
            qid: {"answers": list(entry.get("values") or (
                [entry["text"]] if entry.get("text") else []
            ))}
            for qid, entry in answers.items()
        }
    }


def content_for_elicitation(
    answers: dict[str, dict[str, Any]],
    *,
    schema: Optional[dict[str, Any]] = None,
) -> dict[str, Any]:
    """MCP / ACP elicitation accept content object."""
    properties = {}
    if isinstance(schema, dict):
        raw_props = schema.get("properties")
        if isinstance(raw_props, dict):
            properties = raw_props
    content: dict[str, Any] = {}
    for qid, entry in answers.items():
        if qid.startswith("__"):
            continue
        values = list(entry.get("values") or [])
        text = _as_str(entry.get("text"))
        prop = properties.get(qid) if isinstance(properties.get(qid), dict) else {}
        prop_type = _as_str(prop.get("type"), "").lower()
        if prop_type == "boolean":
            raw = (values[0] if values else text).strip().lower()
            content[qid] = raw in {"1", "true", "yes", "y", "on", "是", "允许", "同意"}
        elif prop_type == "integer":
            content[qid] = int(values[0] if values else text)
        elif prop_type == "number":
            content[qid] = float(values[0] if values else text)
        elif prop_type == "array" or len(values) > 1:
            content[qid] = values if values else ([text] if text else [])
        else:
            content[qid] = values[0] if values else text
    return content


def c22_fixture_questions() -> list[dict[str, Any]]:
    """Canonical three-interactive-question fixture for CU / unit tests."""
    return [
        {
            "question_id": "q_header",
            "kind": "header",
            "prompt": "部署确认",
            "required": False,
            "options": [],
            "allow_free_text": False,
            "placeholder": "",
        },
        {
            "question_id": "q_env",
            "kind": "single_select",
            "prompt": "选择部署环境",
            "required": True,
            "options": [
                {"value": "staging", "label": "staging", "recommended": False},
                {"value": "prod", "label": "prod", "recommended": True},
            ],
            "allow_free_text": False,
            "placeholder": "",
        },
        {
            "question_id": "q_flags",
            "kind": "multi_select",
            "prompt": "可选特性开关",
            "required": False,
            "options": [
                {"value": "a", "label": "a", "recommended": False},
                {"value": "b", "label": "b", "recommended": False},
                {"value": "c", "label": "c", "recommended": False},
            ],
            "allow_free_text": False,
            "placeholder": "",
        },
        {
            "question_id": "q_note",
            "kind": "free_text",
            "prompt": "备注",
            "required": True,
            "options": [],
            "allow_free_text": True,
            "placeholder": "填写备注…",
        },
    ]


__all__ = [
    "QUESTION_KINDS",
    "answers_for_codex_tool",
    "c22_fixture_questions",
    "content_for_elicitation",
    "expand_legacy_text_answers",
    "flatten_answers_text",
    "normalize_pending_user_input",
    "normalize_question",
    "questions_from_codex_params",
    "questions_from_schema",
    "validate_user_input_answers",
]
