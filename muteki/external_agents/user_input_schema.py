"""Normalized Conversation user-input questions and structured answers (C22).

Adapters emit Provider-specific pending payloads; this module normalizes them
into a shared ``questions[]`` read model and validates / maps structured
answers for ``conversation.user_input.resolve``.
"""

from __future__ import annotations

from datetime import date, datetime
import math
import re
from typing import Any, Optional
from urllib.parse import urlsplit


class UserInputValidationError(ValueError):
    """Correctable request-boundary error, with a stable typed code."""

    def __init__(self, code: str, message: str):
        self.code = code
        self.message = message
        super().__init__(f"{code}|{message}")

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
    value = _wire_value(raw["value"]) if "value" in raw and raw["value"] is not None else next((_as_str(raw[key]) for key in
                  ("value", "id", "label", "title", "name")
                  if key in raw and raw[key] is not None and _as_str(raw[key])), "")
    if not value and "value" not in raw:
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
    if kind == "boolean":
        kind = "single_select"
        options = [{"value": "true", "label": "True", "recommended": False},
                   {"value": "false", "label": "False", "recommended": False}]
        allow_free_text = False
    normalized = {
        "question_id": question_id,
        "kind": kind,
        "prompt": prompt,
        "required": required,
        "options": options,
        "allow_free_text": allow_free_text if kind != "header" else False,
        "placeholder": _as_str(raw.get("placeholder")),
    }
    for key in ("schema_type", "schema"):
        if key in raw:
            normalized[key] = raw[key]
    return normalized


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
    if isinstance(properties, dict) and not properties:
        return [{"question_id": "__title__", "kind": "header", "prompt": title,
                 "required": False, "options": [], "allow_free_text": False,
                 "placeholder": ""}] if title else []
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
        prop_type = _as_str(prop.get("type"), "string").lower()
        multi = prop_type == "array"
        option_schema = _as_dict(prop.get("items")) if multi else prop
        enum_values = option_schema.get("enum")
        options = []
        if isinstance(enum_values, list):
            options = [{"value": _wire_value(value), "label": _wire_value(value) or "Empty value",
                        "recommended": False} for value in enum_values]
        else:
            alternatives = option_schema.get("anyOf" if multi else "oneOf")
            if isinstance(alternatives, list):
                options = [{"value": _wire_value(item["const"]),
                            "label": _as_str(item.get("title"), _wire_value(item["const"])),
                            "recommended": False}
                           for item in alternatives if isinstance(item, dict) and "const" in item]
        if prop_type == "boolean":
            options = [{"value": "true", "label": "True", "recommended": False},
                       {"value": "false", "label": "False", "recommended": False}]
        kind = _kind_from_flags(
            has_options=bool(options),
            multi=multi,
            free_text=not options,
            header=False,
            schema_type="string" if prop_type == "boolean" else prop_type,
        )
        prompt = _as_str(
            prop.get("title") or prop.get("description") or key,
            str(key),
        )
        out.append({
            "question_id": str(key),
            "kind": kind,
            "prompt": prompt,
            "required": str(key) in required_keys,
            "options": options,
            "allow_free_text": kind == "free_text" or (
                bool(options) and _bool(prop.get("allow_free_text"), False)
            ),
            "placeholder": _as_str(prop.get("placeholder")),
            "schema_type": prop_type,
            "schema": dict(prop),
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
        if isinstance(schema, dict) and "properties" in schema:
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
        values = [_wire_value(item) for item in values_raw if item is not None]
    elif values_raw is not None and _as_str(values_raw):
        values = [_as_str(values_raw)]
    text = str(entry["text"]) if entry.get("text") is not None else ""
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
    if decision_norm == "decline" and "decline" not in pending.get("response_actions", []):
        raise UserInputValidationError("conversation.user_input.invalid_decision",
                                       "this request does not support an explicit decline")
    if decision_norm in {"cancel", "decline"}:
        return {}
    if decision_norm != "submit":
        raise UserInputValidationError("conversation.user_input.invalid_decision",
                                       f"unsupported decision: {decision_norm}")

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
            raise UserInputValidationError("conversation.user_input.unknown_question",
                                           f"unknown question_id: {key}")
        question = by_id[key]
        if question.get("kind") == "header":
            raise UserInputValidationError("conversation.user_input.header_answered",
                                           f"header question cannot be answered: {key}")
        values, text = _answer_values(entry)
        if values or text:
            options = {_wire_value(option.get("value")) for option in question.get("options", [])}
            if options and any(value not in options for value in values):
                raise UserInputValidationError("conversation.user_input.option",
                                               f"invalid option for {key}")
            if (question.get("kind") == "single_select"
                    or question.get("schema_type") in {"string", "number", "integer", "boolean"}) and len(values) > 1:
                raise UserInputValidationError("conversation.user_input.cardinality",
                                               f"select one option for {key}")
            if text and text not in values and options and not question.get("allow_free_text"):
                raise UserInputValidationError("conversation.user_input.free_text",
                                               f"free text is not supported for {key}")
            schema = question.get("schema")
            if not isinstance(schema, dict):
                schema = {"type": question["schema_type"]} if question.get("schema_type") else {}
            if schema:
                _validate_schema_value(_content_value(values, text, schema), schema, key)
        cleaned[key] = {"values": values, "text": text}

    for question in questions:
        qid = _as_str(question.get("question_id"))
        if question.get("kind") == "header" or not question.get("required", True):
            continue
        entry = cleaned.get(qid) or {}
        values = list(entry.get("values") or [])
        text = str(entry["text"]) if entry.get("text") is not None else ""
        if not values and not text:
            raise UserInputValidationError("conversation.user_input.required",
                                           f"required question unanswered: {qid}")
    return cleaned


def flatten_answers_text(answers: dict[str, dict[str, Any]]) -> str:
    """Join structured answers into a legacy single text string."""
    parts: list[str] = []
    for qid, entry in answers.items():
        values = [str(v) for v in (entry.get("values") or []) if str(v).strip()]
        text = str(entry["text"]) if entry.get("text") is not None else ""
        body = ", ".join(values)
        if text and text not in values:
            body = f"{body}\n{text}" if body else text
        if body:
            parts.append(f"{qid}: {body}" if len(answers) > 1 else body)
    return "\n".join(parts)


def answers_for_codex_tool(
    answers: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Codex tool user-input response shape."""
    result = {}
    for qid, entry in answers.items():
        values = list(entry.get("values") or [])
        text = str(entry["text"]) if entry.get("text") is not None else ""
        if text and text not in values:
            values.append(text)
        result[qid] = {"answers": values}
    return {"answers": result}


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
        text = str(entry["text"]) if entry.get("text") is not None else ""
        prop = properties.get(qid) if isinstance(properties.get(qid), dict) else {}
        if not values and not text:
            continue
        value = _content_value(values, text, prop)
        _validate_schema_value(value, prop, qid)
        content[qid] = value
    return content


def _wire_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _content_value(values: list[str], text: str, schema: dict[str, Any]) -> Any:
    raw = values[0] if values else text
    kind = schema.get("type")
    if not isinstance(kind, (str, type(None))):
        raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                       "input schema requires a single primitive type")
    try:
        if kind == "boolean":
            if raw not in {"true", "false"}:
                raise ValueError("expected true or false")
            return raw == "true"
        if kind == "integer":
            return int(raw)
        if kind == "number":
            value = float(raw)
            if not math.isfinite(value):
                raise ValueError("expected a finite number")
            return value
        if kind == "array":
            items = values if values else ([text] if text else [])
            item_schema = schema.get("items")
            if item_schema is not None and not isinstance(item_schema, dict):
                raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                               "array items must use one property schema")
            return [_content_value([item], "", item_schema or {}) for item in items]
        if kind not in {None, "string"}:
            raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                           f"unsupported input schema type: {kind}")
        if kind is None:
            choices = schema.get("enum", [])
            if "const" in schema:
                choices = [schema["const"]]
            alternatives = schema.get("oneOf") or schema.get("anyOf") or []
            if alternatives:
                choices = [item["const"] for item in alternatives
                           if isinstance(item, dict) and "const" in item]
            matches = [item for item in choices if _wire_value(item) == raw]
            if matches:
                if any(type(item) is not type(matches[0]) for item in matches):
                    raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                                   "input options have ambiguous wire values")
                return matches[0]
        return raw
    except UserInputValidationError:
        raise
    except (TypeError, ValueError, OverflowError) as exc:
        raise UserInputValidationError("conversation.user_input.type",
                                       f"expected {kind}") from exc


def _validate_schema_value(value: Any, schema: dict[str, Any], qid: str) -> None:
    def invalid(message: str):
        raise UserInputValidationError("conversation.user_input.constraint", f"{qid}: {message}")
    annotations = {"$schema", "$id", "title", "description", "default", "examples",
                   "deprecated", "readOnly", "writeOnly"}
    supported = {"type", "enum", "const", "oneOf", "anyOf", "minLength", "maxLength",
                 "pattern", "format", "minimum", "maximum", "exclusiveMinimum",
                 "exclusiveMaximum", "multipleOf", "minItems", "maxItems", "uniqueItems", "items"}
    unknown = [key for key in schema if key not in supported | annotations and not key.startswith("x-")]
    if unknown:
        raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                       f"{qid}: unsupported schema keywords: {', '.join(unknown)}")
    for key in ("minLength", "maxLength", "minItems", "maxItems"):
        if key in schema and (isinstance(schema[key], bool)
                              or not isinstance(schema[key], int) or schema[key] < 0):
            raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                           f"{qid}: {key} must be a non-negative integer")
    for key in ("minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum", "multipleOf"):
        if key in schema and (isinstance(schema[key], bool)
                              or not isinstance(schema[key], (int, float))
                              or not math.isfinite(schema[key])):
            raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                           f"{qid}: {key} must be a finite number")
    if "multipleOf" in schema and schema["multipleOf"] <= 0:
        raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                       f"{qid}: multipleOf must be positive")
    if "enum" in schema and not isinstance(schema["enum"], list):
        raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                       f"{qid}: enum must be an array")
    if "const" in schema and value != schema["const"]:
        invalid("value differs from the required constant")
    if "enum" in schema and value not in schema["enum"]:
        invalid("value is outside the allowed options")
    alternatives = schema.get("oneOf") or schema.get("anyOf")
    if alternatives is not None:
        if not isinstance(alternatives, list) or any(not isinstance(item, dict) or "const" not in item for item in alternatives):
            raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                           f"{qid}: only const alternatives are supported")
        if value not in [item["const"] for item in alternatives]:
            invalid("value is outside the allowed options")
    if isinstance(value, str):
        if len(value) < schema.get("minLength", 0):
            invalid("text is too short")
        if "maxLength" in schema and len(value) > schema["maxLength"]:
            invalid("text is too long")
        if "pattern" in schema:
            try:
                matched = re.search(schema["pattern"], value)
            except (TypeError, re.error) as exc:
                raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                               f"{qid}: invalid pattern") from exc
            if not matched:
                invalid("text does not match the requested pattern")
        fmt = schema.get("format")
        try:
            if fmt == "date":
                if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
                    invalid("date must use YYYY-MM-DD")
                date.fromisoformat(value)
            elif fmt == "date-time":
                if not re.fullmatch(r"\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:[Zz]|[+-]\d{2}:\d{2})", value):
                    invalid("date-time must use RFC 3339")
                parsed = datetime.fromisoformat(value.replace("Z", "+00:00").replace("z", "+00:00"))
                if parsed.tzinfo is None:
                    invalid("date-time requires an explicit timezone")
            elif fmt == "email" and not re.fullmatch(r"[^\s@]+@[^\s@]+\.[^\s@]+", value):
                invalid("invalid email address")
            elif fmt in {"uri", "url"} and not urlsplit(value).scheme:
                invalid("invalid absolute URI")
            elif fmt not in {None, "date", "date-time", "email", "uri", "url"}:
                raise UserInputValidationError("conversation.user_input.unsupported_schema",
                                               f"{qid}: unsupported format {fmt}")
        except ValueError as exc:
            if isinstance(exc, UserInputValidationError):
                raise
            invalid(f"invalid {fmt}")
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        if "multipleOf" in schema:
            from fractions import Fraction
            if (Fraction(str(value)) / Fraction(str(schema["multipleOf"]))).denominator != 1:
                invalid("number violates multipleOf")
        for key, bad in (("minimum", lambda bound: value < bound),
                         ("maximum", lambda bound: value > bound),
                         ("exclusiveMinimum", lambda bound: value <= bound),
                         ("exclusiveMaximum", lambda bound: value >= bound)):
            if key in schema and bad(schema[key]):
                invalid(f"number violates {key}")
    elif isinstance(value, list):
        if len(value) < schema.get("minItems", 0) or ("maxItems" in schema and len(value) > schema["maxItems"]):
            invalid("number of selected options is outside the requested range")
        if schema.get("uniqueItems") and len(set(value)) != len(value):
            invalid("selected options must be unique")
        items = _as_dict(schema.get("items"))
        for item in value:
            _validate_schema_value(item, items, qid)


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
