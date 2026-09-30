"""Result, fact, flag, finding, PoC and report publish. Moved from cli_solver.py."""
from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
import signal
import tempfile
import time
import uuid
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Optional

from muteki.core.events import (
    EventType, blackboard_delta_payload, hitl_request_payload,
    insight_payload, shared_graph_delta_payload, solve_graph_delta_payload,
)
from muteki.core.prompt_assembly import estimate_host_tokens
from muteki.solver.cli_driver import (
    CliResult,
)
from muteki.solver.result_codes import (
    RESULT_CANCELLED,
    RESULT_EXPLORED,
    RESULT_OOM,
    RESULT_OUTPUT_LIMIT,
    RESULT_DISK_LIMIT,
    RESULT_SOLVED,
    RESULT_STEERED,
    RESULT_TIMED_OUT,
)
from muteki.solver.cli_protocol import (
    _looks_like_verifier_output,
    _parse_lockout_seconds,
)
from muteki.solver.ctf_fgs import (
    flag_finding_payload,
    product_finding_payload,
    unanswered_observation_claim,
)
from muteki.solver.worker_result import (
    DeadEndClaim,
    NeedInputClaim,
    ObservationClaim,
    WorkerResult,
)


_MAX_ATOMIC_FACT_TOKENS = 256
_MAX_ATOMIC_DEAD_END_TOKENS = 512


class WorkerRuntimeUnavailable(RuntimeError):
    """A Worker process cannot start because its runtime model setup is absent."""

    def __init__(self, detail: str, *, code: str) -> None:
        super().__init__(detail)
        self.code = code


class ToolArtifactPersistenceError(RuntimeError):
    code = "tool_artifact_persistence_failed"


class BlackboardReceiptPersistenceError(RuntimeError):
    code = "blackboard_receipt_persistence_failed"


def _worker_runtime_failure_code(result: CliResult) -> str:
    """Use only an explicit runtime error code; stderr is diagnostic data."""
    status = getattr(result, "runtime_status", None)
    code = str(status.get("error_code") or "") if isinstance(status, dict) else ""
    if code in {
        "provider_config_missing", "model_catalog_missing",
        "process_input_illegal", "shared_context_unready",
        "worker_environment_unavailable",
    }:
        return code
    return ""


def _atomic_claim(self, text: str, *, kind: str) -> tuple[str, str]:
    value = str(text or "").strip()
    limit = (
        _MAX_ATOMIC_FACT_TOKENS if kind == "fact"
        else _MAX_ATOMIC_DEAD_END_TOKENS
    )
    if estimate_host_tokens(value) <= limit:
        return value, ""
    artifact_id = str(self.artifacts.put(value, suffix=f".{kind}.txt") or "")
    return (
        f"Oversized {kind} stored as [artifact:{artifact_id}] "
        f"({len(value)} characters; exceeds {limit} estimated tokens).",
        artifact_id,
    )


@dataclass
class ToolEvidenceRecord:
    """One concrete tool observation that may support a Fact promotion."""

    output: str
    command: str
    call_id: str
    worker_id: str
    intent_id: str
    target_epoch: str
    target: str
    artifact_id: str
    artifact_sha256: str
    observed_at: float
    attributed: bool
    event_seq: int = 0
    event_at: float = 0.0

    def provenance(self, *, run_id: str, promoted_at: float) -> dict[str, object]:
        return {
            "tool_event_id": f"{run_id}:{self.event_seq}",
            "tool_event_seq": int(self.event_seq),
            "tool_event_ts": float(self.event_at),
            "tool_call_id": self.call_id,
            "worker_id": self.worker_id,
            "intent_id": self.intent_id,
            "target_epoch": self.target_epoch,
            "target": self.target,
            "artifact_id": self.artifact_id,
            "artifact_sha256": self.artifact_sha256,
            "observed_at": float(self.observed_at),
            "promoted_at": float(promoted_at),
        }


def _fact_evidence_body(raw: str) -> str:
    return str(raw or "").strip()


def _fact_supported_by_tool_event(
    self, fact: str, *, output: str, command: str = "",
) -> bool:
    """Match a claim against one request/response tool event.

    Request identity commonly lives in the command while the observable lives in
    stdout.  The combined event may support the fact, but at least one concrete
    fact token must still occur in stdout so a command that merely attempted an
    action cannot prove its own success.
    """
    body = _fact_evidence_body(output)
    if not body:
        return False
    if self._fact_witnessed_in_chunk(fact, body):
        return True
    combined = f"{command}\n{body}".strip()
    if not command or not self._fact_witnessed_in_chunk(fact, combined):
        return False
    fact_tokens = {
        token for token in re.findall(
            r"[a-z0-9_./:-]{4,}", str(fact or "").lower())
        if token not in {
            "http", "https", "true", "false", "with", "from", "that",
            "this", "there", "have", "confirmed",
        }
    }
    body_lower = body.lower()
    return any(token in body_lower for token in fact_tokens)


def _current_intent_id(self) -> str:
    return str(
        getattr(self, "intent_id_assigned", "")
        or getattr(self, "_intent_id", "")
        or f"intent:{self.solver_id}"
    )


def _fact_claim_provenance(
    self, provenance: Optional[dict] = None,
) -> dict[str, object]:
    """Attach the current Worker/Intent/target identity to every Fact claim."""
    result: dict[str, object] = dict(provenance or {})
    result.setdefault("worker_id", self.solver_id)
    result.setdefault("intent_id", _current_intent_id(self))
    result.setdefault(
        "target_epoch", str(getattr(self, "_target_epoch", "") or "1"))
    result.setdefault("observed_at", time.time())
    return result


def _ctf_step_artifact_refs(self) -> list[dict[str, object]]:
    """Current Step's exact persisted tool outputs, without their text copies."""
    current_intent = _current_intent_id(self)
    epoch = str(getattr(self, "_target_epoch", "") or "")
    refs: list[dict[str, object]] = []
    for row in getattr(self, "_tool_artifact_refs", None) or []:
        if (row.get("worker_id") != self.solver_id
                or row.get("intent_id") != current_intent
                or row.get("target_epoch") != epoch
                or int(row.get("tool_event_seq") or 0) <= 0):
            continue
        artifact_id = str(row.get("artifact_id") or "")
        expected = str(row.get("sha256") or "")
        actual = self.artifacts.sha256(artifact_id) if artifact_id else None
        if not actual or actual != expected:
            raise ToolArtifactPersistenceError(
                f"tool artifact missing or changed: {artifact_id or '(empty)'}"
            )
        refs.append({
            "artifact_id": artifact_id,
            "sha256": expected,
            "size": int(row.get("size") or 0),
            "command": str(row.get("command") or ""),
            "tool_event_seq": int(row.get("tool_event_seq") or 0),
        })
    return refs


def _target_epoch_matches(self, observed_epoch: str = "") -> bool:
    current = str(getattr(self, "_target_epoch", "") or "")
    observed = str(observed_epoch or "")
    return bool(current and observed and current == observed)


def _record_artifact_matches(self, record: ToolEvidenceRecord) -> bool:
    if not record.artifact_id or not record.artifact_sha256:
        return False
    try:
        digest = self.artifacts.sha256(record.artifact_id)
    except Exception:
        return False
    return bool(digest and digest == record.artifact_sha256)


def _referenced_fact_evidence(
    self, artifact_id: str,
) -> Optional[ToolEvidenceRecord]:
    """Resolve a Worker-selected tool observation by stable artifact ID."""
    if not artifact_id:
        return None
    now = time.time()
    current_intent = _current_intent_id(self)
    started_at = float(getattr(self, "_worker_started_at", 0.0) or 0.0)
    records = list(getattr(self, "_tool_evidence_records", None) or [])
    for record in reversed(records):
        if (record.artifact_id != artifact_id
                or not record.attributed or record.event_seq <= 0
                or record.event_at <= 0
                or record.worker_id != self.solver_id
                or record.intent_id != current_intent
                or not _target_epoch_matches(self, record.target_epoch)
                or record.observed_at < started_at
                or record.observed_at > record.event_at + 1.0
                or record.event_at > now
                or not record.output
                or not _record_artifact_matches(self, record)):
            continue
        return record
    return None


def _persist_raw_tool_output(
    self, raw: str, *, command: str = "", call_id: str = "",
    attributed: bool = True, intent_id: str = "", target_epoch: str = "",
    target: str = "",
) -> ToolEvidenceRecord:
    """Append one tool execution's FULL stdout/stderr to the audit corpus.
    Called from the LIVE path (_emit_step) for every tool_result, BEFORE any
    truncation/summarization. Results without a matching invocation are retained
    for audit but marked non-authoritative. The paired `command` is kept for the
    target-identity attestation (round-10)."""
    if not raw and not command:
        raise ValueError("tool evidence requires command or output")
    artifact_id = ""
    artifact_sha256 = ""
    if raw:
        try:
            artifact_id = str(self.artifacts.put(raw, suffix=".txt") or "")
            artifact_sha256 = str(
                self.artifacts.sha256(artifact_id) if artifact_id else ""
            )
            if artifact_id and self.artifacts.read_text(artifact_id) != raw:
                raise ToolArtifactPersistenceError(
                    "tool output artifact differs from the captured text"
                )
        except Exception as exc:
            raise ToolArtifactPersistenceError(
                f"tool output could not be persisted: {type(exc).__name__}: {exc}"
            ) from exc
        if not artifact_id or not artifact_sha256:
            raise ToolArtifactPersistenceError(
                "tool output persistence returned no verifiable artifact"
            )
    record = ToolEvidenceRecord(
        output=raw,
        command=command or "",
        call_id=call_id or "",
        worker_id=self.solver_id,
        intent_id=str(intent_id or _current_intent_id(self)),
        target_epoch=str(
            target_epoch or getattr(self, "_target_epoch", "") or ""
        ),
        target=str(target or self._target() or ""),
        artifact_id=artifact_id,
        artifact_sha256=artifact_sha256,
        observed_at=time.time(),
        attributed=bool(attributed),
    )
    if artifact_id:
        refs = list(getattr(self, "_tool_artifact_refs", None) or [])
        refs.append({
            "artifact_id": artifact_id,
            "sha256": artifact_sha256,
            "size": len(raw.encode("utf-8", errors="replace")),
            "command": command or "",
            "worker_id": self.solver_id,
            "intent_id": record.intent_id,
            "target_epoch": record.target_epoch,
            "tool_event_seq": 0,
        })
        self._tool_artifact_refs = refs
    # Build the complete trimmed state off to the side, then swap all parallel
    # fields together so an exception cannot leave their indexes misaligned.
    outputs = [*self._raw_tool_outputs, raw]
    commands = [*self._raw_tool_commands, command or ""]
    attributed_rows = [*self._raw_tool_attributed, bool(attributed)]
    records = [*self._tool_evidence_records, record]
    chars = self._raw_tool_outputs_chars + len(raw)
    while chars > self._RAW_OUTPUT_CHAR_CAP and len(outputs) > 1:
        chars -= len(outputs.pop(0))
        commands.pop(0)
        attributed_rows.pop(0)
        records.pop(0)
    self._raw_tool_outputs = outputs
    self._raw_tool_commands = commands
    self._raw_tool_attributed = attributed_rows
    self._tool_evidence_records = records
    self._raw_tool_outputs_chars = chars
    self._maybe_steer_idle_repeat()
    return record


def _bind_tool_evidence_event(self, record: ToolEvidenceRecord, event: object) -> None:
    """Complete the evidence record after EventBus assigns its seq and time."""
    if event is None:
        return
    record.event_seq = int(getattr(event, "seq", 0) or 0)
    record.event_at = float(getattr(event, "ts", 0.0) or 0.0)
    for ref in reversed(getattr(self, "_tool_artifact_refs", None) or []):
        if ref.get("artifact_id") == record.artifact_id:
            ref["tool_event_seq"] = record.event_seq
            break


def _provenance_corpus(self) -> str:
    """Return attributed raw tool output for non-Flag evidence handling."""
    return "\n".join(
        output for idx, output in enumerate(self._raw_tool_outputs)
        if (idx < len(self._raw_tool_attributed)
            and self._raw_tool_attributed[idx])
    )


def _finding_evidence_corpus(self) -> str:
    """Request (attributed command) plus response (that command's output).

    Report reproduction needs the argv/URL as well because the replay target
    and input often live in the request while the witness lives in its output.
    """
    parts: list[str] = []
    for idx, output in enumerate(self._raw_tool_outputs):
        if (idx >= len(self._raw_tool_attributed)
                or not self._raw_tool_attributed[idx]):
            continue
        cmd = (self._raw_tool_commands[idx]
               if idx < len(self._raw_tool_commands) else "")
        parts.append(f"{cmd}\n{output}")
    return "\n".join(parts)


async def _accept_submitted_flag(self, flag: str) -> bool:
    if flag in self._rejected_flags() or flag in self._already_found:
        return False
    return await self._accept_flag(flag)


def _blackboard_operation_allowed(self, operation: str) -> bool:
    if operation == "context":
        return True
    if operation in {"mcp_tools", "mcp_schema", "mcp_call"}:
        return (getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
                and self.mode in {"bootstrap", "explore", "fact_verifier", "review"})
    if operation == "recent_evidence":
        return getattr(self.challenge, "mode", "ctf") == "pentest" and self.mode in {"bootstrap", "explore", "fact_verifier"}
    if operation == "need_input" and not bool(
        getattr(self.challenge, "allow_operator_input", True)
    ):
        return False
    ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
    if ctf_mode:
        if operation == "read_artifact":
            return self.mode in {"bootstrap", "explore", "fact_verifier"}
        if self.mode in {"bootstrap", "explore"}:
            permitted = {
                "submit_fact", "commit_step", "dead_end", "need_input",
                "save_poc",
            }
            if self.challenge.mode == "pentest":
                permitted.add("submit_report")
            return operation in (permitted | ({"submit_flag"} if self.challenge.mode == "ctf" else set()))
        if self.mode == "fact_verifier":
            return operation in {"submit_fact", "submit_report", "commit_step", "dead_end", "save_poc"}
        if self.mode == "respond" and str(
            self.hitl_cmd.get("action") or ""
        ) == "mark_false":
            return operation in {
                "fact", "dead_end", "need_input", "save_poc", "submit_flag",
            }
        return False
    coordination = {
        "claim_intent", "claim_activity", "claim_resource", "release_resource",
    }
    solve = {
        "fact", "dead_end", "need_input", "save_poc",
        "report_capability_gap", "publish_capability",
        "launch_runtime_resource", "publish_access_path",
    }
    if self.mode in {"bootstrap", "explore"}:
        if operation in solve | coordination | {"branch_proposal"}:
            return True
        if operation == "submission_lock":
            return bool(getattr(self.challenge, "verifier_rate_limited", False))
        if operation == "submit_flag":
            return getattr(self.challenge, "mode", "ctf") == "ctf"
    if self.mode == "fact_verifier":
        return operation in solve | coordination
    if self.mode == "review":
        return operation in {
            "review_finding", "fact_challenge", "fact_merge",
            "fact_reject", "fact_revalidation",
        }
    if self.mode == "respond" and str(
            self.hitl_cmd.get("action") or "") == "mark_false":
        if operation in solve | coordination | {"branch_proposal"}:
            return True
        if operation == "submit_flag":
            return getattr(self.challenge, "mode", "ctf") == "ctf"
        return operation == "submission_lock" and bool(
            getattr(self.challenge, "verifier_rate_limited", False))
    return False


async def _handle_blackboard_request(self, payload: dict) -> dict:
    if str(payload.get("protocol") or "") != "muteki-blackboard-v2":
        return {"ok": False, "detail": "unsupported Blackboard protocol"}
    operation = str(payload.get("operation") or "")
    if not self._blackboard_operation_allowed(operation):
        return {
            "ok": False,
            "detail": f"operation {operation or '(empty)'} is not allowed for {self.mode}",
        }
    shared_graph = getattr(self, "shared_graph", None)
    if operation in {"mcp_tools", "mcp_schema", "mcp_call"}:
        service = getattr(self, "plugin_service", None)
        if service is None:
            return {"ok": False, "code": "worker_mcp.unavailable",
                    "detail": "Managed MCP service is unavailable in this execution"}
        mode = str(getattr(self.challenge, "mode", "ctf") or "ctf")
        engine = str(self.driver.name)
        try:
            tools = await service.prepare_tools(engine, mode, self.run_id)
            if operation == "mcp_tools":
                catalog = [{"name": item["name"], "description": item["description"],
                            "package": item["_package"], "server": item["_server"]}
                           for item in tools]
                errors = service.mcp_connection_errors(engine, mode, self.run_id)
                return {"ok": True, "message": json.dumps(
                    {"tools": catalog, "connection_errors": errors}, ensure_ascii=False)}
            name = str(payload.get("name") or "")
            selected = next((item for item in tools if item["name"] == name), None)
            if selected is None:
                return {"ok": False, "code": "worker_mcp.tool_unavailable",
                        "detail": "MCP tool is not enabled for this Worker mode and engine"}
            if operation == "mcp_schema":
                return {"ok": True, "message": json.dumps(selected["input_schema"], ensure_ascii=False)}
            arguments = payload.get("arguments")
            if not isinstance(arguments, dict):
                return {"ok": False, "code": "worker_mcp.invalid_arguments",
                        "detail": "MCP arguments must be a JSON object"}
            result = await service.invoke(engine, name, arguments, mode=mode, scope=self.run_id)
            return {"ok": True, "message": json.dumps(result, ensure_ascii=False)}
        except asyncio.TimeoutError:
            return {"ok": False, "code": "worker_mcp.timeout", "detail": "MCP tool timed out"}
        except Exception as exc:
            return {"ok": False, "code": "worker_mcp.failed",
                    "detail": f"MCP {type(exc).__name__}: {exc}"}
    if operation == "context":
        return {"ok": True, "content": self._live_blackboard_context()}
    if operation == "recent_evidence":
        records = []
        for row in getattr(self, "_tool_evidence_records", None) or []:
            if (not row.attributed or row.event_seq <= 0
                    or row.worker_id != self.solver_id
                    or row.intent_id != _current_intent_id(self)
                    or not _target_epoch_matches(self, row.target_epoch)):
                continue
            if not _record_artifact_matches(self, row):
                raise ToolArtifactPersistenceError(
                    f"tool artifact missing or changed: {row.artifact_id}"
                )
            records.append({
                "artifact_id": row.artifact_id,
                "tool_event_id": f"{self.run_id}:{row.event_seq}",
                "bytes": self.artifacts.size(row.artifact_id),
            })
        return {"ok": True, "content": json.dumps(records, ensure_ascii=False)}
    if operation == "read_artifact":
        artifact_id = str(payload.get("artifact_id") or "")
        expected = (
            shared_graph.ctf_artifact_digest(artifact_id)
            if shared_graph is not None else None
        )
        if not expected:
            own = _referenced_fact_evidence(self, artifact_id)
            expected = own.artifact_sha256 if own is not None else None
        if not expected:
            return {"ok": False, "detail": "artifact is not referenced by this challenge"}
        if self.artifacts.sha256(artifact_id) != expected:
            return {"ok": False, "detail": "artifact is missing or has changed"}
        content = self.artifacts.read_text(artifact_id)
        if content is None:
            return {"ok": False, "detail": "artifact is missing"}
        if "\x00" in content:
            return {"ok": False, "detail": "artifact contains binary NUL bytes"}
        return {"ok": True, "content": content}
    if operation == "submit_fact":
        title = str(payload.get("title") or "").strip()
        content = str(payload.get("content") or "")
        if not title or not content.strip():
            return {"ok": False, "detail": "title and content are required"}
        evidence_artifact_id = str(payload.get("evidence_artifact_id") or "").strip()
        if getattr(self.challenge, "mode", "ctf") == "pentest" and not _referenced_fact_evidence(self, evidence_artifact_id):
            return {"ok": False, "detail": "evidence artifact is missing or not attributed to this Step; call recent-evidence"}
        self._draft_fact = {
            "title": title, "content": content,
            "evidence_artifact_id": evidence_artifact_id,
        }
        self._draft_report = None
        self._accepted_blackboard_requests += 1
        return {
            "ok": True,
            "message": "已记入草稿。核对无误即调 commit-step 定稿收束。",
        }
    if operation == "submit_report":
        from muteki.pentest.contract import in_scope_url
        item = payload.get("report")
        if not isinstance(item, dict):
            return {"ok": False, "detail": "report must be a JSON object"}
        required = ("title", "finding_class", "resource_id", "identity_a",
                    "summary", "observed_impact", "severity_rationale", "remediation")
        missing = [key for key in required if not isinstance(item.get(key), str) or not item[key].strip()]
        steps = item.get("reproduction_steps")
        retest = item.get("retest_steps")
        severity = item.get("severity")
        if missing:
            return {"ok": False, "detail": f"required nonempty text fields: {', '.join(missing)}"}
        if not isinstance(severity, str) or severity not in {
            "critical", "high", "medium", "low", "informational", "unrated",
        }:
            return {"ok": False, "detail": "severity must be critical, high, medium, low, informational, or unrated"}
        if not isinstance(steps, list) or not steps or not all(
            isinstance(x, str) and x.strip() for x in steps
        ):
            return {"ok": False, "detail": "reproduction_steps must be a nonempty array of nonempty strings"}
        if not isinstance(retest, list) or not retest or not all(
            isinstance(x, str) and x.strip() for x in retest
        ):
            return {"ok": False, "detail": "retest_steps must be a nonempty array of nonempty strings"}
        contract = getattr(self.challenge, "pentest_contract", None)
        if contract is None or not in_scope_url(item["resource_id"], contract):
            return {"ok": False, "detail": "report resource is outside the authorized scope"}
        if not getattr(self, "_draft_fact", None):
            return {"ok": False, "detail": "submit-fact with tool evidence before submit-report"}
        optional_text = ("identity_b", "preconditions", "potential_impact")
        for field in optional_text:
            if field in item and not isinstance(item[field], str):
                return {"ok": False, "detail": f"{field} must be text"}
        if "affected_assets" in item and (not isinstance(item["affected_assets"], list)
                                          or not item["affected_assets"]
                                          or any(not isinstance(value, str) or not value.strip()
                                                 for value in item["affected_assets"])):
            return {"ok": False, "detail": "affected_assets must be a nonempty array of nonempty strings"}
        screenshot_ids = item.get("screenshot_poc_ids", [])
        if not isinstance(screenshot_ids, list) or any(not isinstance(x, str) or not x for x in screenshot_ids):
            return {"ok": False, "detail": "screenshot_poc_ids must be a list of saved PoC IDs"}
        note = item.get("evidence_note")
        if note is None and contract.version >= 2:
            return {"ok": False, "detail": "evidence_note must cite the selected Fact artifact and explain observed/significance"}
        if note is not None:
            if not isinstance(note, dict):
                return {"ok": False, "detail": "evidence_note must be an object"}
            for field in ("artifact_id", "observed", "significance"):
                if not isinstance(note.get(field), str) or not note[field].strip():
                    return {"ok": False, "detail": f"evidence_note.{field} must be nonempty text"}
            if note["artifact_id"] != self._draft_fact["evidence_artifact_id"]:
                return {"ok": False, "detail": "evidence_note.artifact_id must match submit-fact --evidence in this Step"}
        pending_images = {p.poc_id: p for p in getattr(self, "_pending_pocs", None) or []
                          if p.name.lower().endswith((".png", ".jpg", ".jpeg", ".webp"))}
        if any(pid not in pending_images for pid in screenshot_ids):
            return {"ok": False, "detail": "every screenshot must be saved in this Step with save-poc first"}
        self._draft_report = {key: value for key, value in item.items()
                              if key in {*required, "identity_b", "severity", "preconditions",
                                         "potential_impact", "reproduction_steps", "retest_steps", "affected_assets",
                                         "screenshot_poc_ids", "evidence_note"}}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "REPORT_DRAFTED; commit-step will atomically submit it with the cited Fact"}
    if operation == "commit_step":
        if self._worker_result_committed:
            prior = dict(getattr(self, "_last_worker_result_commit", None) or {})
            prior_facts = list(prior.get("facts") or [])
            return {
                "ok": True,
                "message": ("STEP_COMMITTED REPORT_EVIDENCE_ADDED"
                            if int(prior.get("evidence_event_seq") or 0) > 0
                            else "STEP_COMMITTED REPORT_SUBMITTED"
                            if int(prior.get("report_seq") or 0) > 0 else "STEP_COMMITTED"),
                "fact_seq": int(prior_facts[-1]) if prior_facts else 0,
                "report_seq": int(prior.get("report_seq") or 0),
                "evidence_event_seq": int(prior.get("evidence_event_seq") or 0),
                "finish_worker": True,
            }
        draft = dict(getattr(self, "_draft_fact", None) or {})
        title = str(draft.get("title") or "").strip()
        content = str(draft.get("content") or "")
        if not title or not content.strip():
            return {"ok": False, "detail": "submit-fact draft is missing"}
        if shared_graph is None:
            return {"ok": False, "detail": "shared graph is unavailable"}

        intent_id = _current_intent_id(self)
        fact_text = f"{title}\n{content}"
        provenance = _fact_claim_provenance(
            self, {"artifact_refs": _ctf_step_artifact_refs(self)}
        )
        if getattr(self.challenge, "mode", "ctf") == "pentest":
            record = _referenced_fact_evidence(
                self, str(draft.get("evidence_artifact_id") or ""),
            )
            if record is None:
                return {
                    "ok": False,
                    "detail": "所选工具证据已失效或不属于当前 Step；请重新查看 recent-evidence",
                }
            provenance.update(record.provenance(
                run_id=str(getattr(self, "run_id", "") or ""),
                promoted_at=time.time(),
            ))
        claim = ObservationClaim(
            text=fact_text,
            claim_verified=True,
            provenance=provenance,
        )
        solved = self._flags_complete_for_worker()
        status = RESULT_SOLVED if solved else RESULT_EXPLORED
        worker_result = WorkerResult(
            status=status,
            result_detail=title,
            produced_new_info=True,
            stop_condition_result="met" if solved else "",
            observations=[claim],
            dead_ends=list(getattr(self, "_pending_dead_ends", None) or []),
            pocs=list(getattr(self, "_pending_pocs", None) or []),
            artifact_ids=(
                [str(self._transcript_artifact_id)]
                if getattr(self, "_transcript_artifact_id", "") else []
            ),
        )
        report_proposed = bool(getattr(self, "_draft_report", None))
        commit = shared_graph.commit_worker_result(
            **worker_result.to_commit_kwargs(
                actor=self.solver_id,
                worker_id=self.solver_id,
                intent_id=intent_id,
                target_epoch=str(getattr(self, "_target_epoch", "") or ""),
                run_id=str(getattr(self, "run_id", "") or ""),
            ),
            vulnerability_report=(dict(self._draft_report) if getattr(self, "_draft_report", None) else None),
            conclude=True,
        )
        if not isinstance(commit, dict) or not commit.get("concluded"):
            return {"ok": False, "detail": "step commit was not applied"}
        fact_seqs = [int(seq) for seq in commit.get("facts") or [] if int(seq) > 0]
        if not fact_seqs:
            return {"ok": False, "detail": "step commit produced no Fact"}

        self._last_fact_seq = max(fact_seqs)
        self._last_worker_result = worker_result
        self._last_worker_result_commit = dict(commit)
        self._worker_result_committed = True
        self._step_committed = True
        self._draft_fact = None
        self._draft_report = None
        self.graph.add_evidence(
            source=self.driver.name, fact=fact_text, verified=True,
        )
        try:
            await self._emit(
                EventType.SHARED_GRAPH_DELTA,
                **shared_graph_delta_payload(
                    f"worker_result:{status}", verified=True, confidence=1.0,
                    actor=self.solver_id,
                    fact_seq=int((commit.get("seqs") or {}).get("summary") or -1),
                ),
            )
        except Exception:
            pass
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            try:
                shared_graph.finding_found(
                    actor=self.solver_id,
                    finding=product_finding_payload(
                        fact_seq=self._last_fact_seq,
                        intent_id=intent_id,
                        title=title,
                    ),
                    intent_id=intent_id,
                )
            except Exception:
                pass
        self._accepted_blackboard_requests += 1
        return {
            "ok": True,
            "message": ("STEP_COMMITTED REPORT_EVIDENCE_ADDED"
                        if int(commit.get("evidence_event_seq") or 0) > 0
                        else "STEP_COMMITTED REPORT_SUBMITTED" if int(commit.get("report_seq") or 0) > 0
                        else "STEP_COMMITTED REPORT_DUPLICATE" if report_proposed
                        else "STEP_COMMITTED"),
            "fact_seq": self._last_fact_seq,
            "report_seq": int(commit.get("report_seq") or 0),
            "evidence_event_seq": int(commit.get("evidence_event_seq") or 0),
            "finish_worker": True,
        }
    if operation == "fact":
        text = str(payload.get("text") or "").strip()
        if not text:
            return {"ok": False, "detail": "fact text is empty"}
        ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
        if ctf_mode:
            fact_text = text
            claim_artifact_id = ""
        else:
            fact_text, claim_artifact_id = _atomic_claim(self, text, kind="fact")
        seq = await self._record_fact(
            fact_text if ctf_mode else f"[{self.driver.name}] {fact_text}",
            verified=True,
            artifact_id=claim_artifact_id,
            witness="",
            provenance=_fact_claim_provenance(self),
            subject=str(payload.get("subject") or ""),
            predicate=str(payload.get("predicate") or ""),
            object_value=payload.get("object"),
            scope=str(payload.get("scope") or ""),
            canonical_key=str(payload.get("canonical_key") or ""),
        )
        if int(seq or -1) > 0:
            self._last_fact_seq = max(
                int(getattr(self, "_last_fact_seq", -1) or -1), int(seq))
        self._accepted_blackboard_requests += 1
        return {
            "ok": int(seq or 0) > 0,
            "message": "FACT_ADDED" if int(seq or 0) > 0 else "FACT_NOT_ADDED",
            "fact_seq": int(seq or 0),
        }
    if operation == "dead_end":
        ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
        if ctf_mode:
            reason = str(payload.get("reason") or "").strip()
        else:
            reason, _artifact_id = _atomic_claim(
                self, str(payload.get("reason") or ""), kind="dead-end")
        tested_scope = str(payload.get("tested_scope") or "").strip()
        observed_result = str(payload.get("observed_result") or "").strip()
        if not reason:
            return {"ok": False, "detail": "dead-end reason is empty"}
        if ctf_mode and (not tested_scope or not observed_result):
            return {"ok": False, "detail": "tested_scope and observed_result are required"}
        claim = DeadEndClaim(
            reason=f"[{self.driver.name}] {reason}",
            tested_scope=tested_scope,
            observed_result=observed_result,
            route_hash=str(getattr(self, "route_hash", "") or ""),
            coverage_key=str(getattr(self, "coverage_key", "") or ""),
        )
        if claim not in self._pending_dead_ends:
            self._pending_dead_ends.append(claim)
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "STAGED"}
    if operation == "need_input":
        need = str(payload.get("need") or "").strip()
        if not need:
            return {"ok": False, "detail": "operator request is empty"}
        need_card = need if len(need) <= 1000 else need[:1000] + " …"
        request_id, correlation = self._decision_request_identity(
            need_card, "external_blocker")
        await self._emit(
            EventType.HITL_REQUEST,
            **hitl_request_payload(
                self.solver_id, need_card, kind="need_input",
                need_kind="external_blocker", request_id=request_id,
                **correlation),
        )
        self._pending_need_input = NeedInputClaim(
            need=need_card, need_kind="external_blocker")
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "OPERATOR_REQUESTED"}
    if operation == "save_poc":
        accepted = await self._handle_poc_save(
            str(payload.get("path") or ""),
            str(payload.get("entry_command") or ""),
            str(payload.get("status") or "available"),
            str(payload.get("note") or ""),
        )
        if accepted:
            self._accepted_blackboard_requests += 1
        poc_id = str(getattr(self, "_last_saved_poc_id", "") or "") if accepted else ""
        return {"ok": accepted, "message": f"SAVED {poc_id}" if accepted else "",
                "poc_id": poc_id}
    if operation == "report_capability_gap":
        if shared_graph is None:
            return {"ok": False, "detail": "SharedGraph is unavailable"}
        gap = shared_graph.report_capability_gap(
            actor=self.solver_id,
            intent_id=_current_intent_id(self),
            target_epoch=str(getattr(self, "_target_epoch", "") or "1"),
            description=str(payload.get("description") or ""),
            required_capabilities=list(payload.get("required_capabilities") or []),
            consumers=list(payload.get("consumers") or []),
        )
        if not gap:
            return {"ok": False, "detail": "capability gap is empty or duplicate"}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "GAP_RECORDED", **gap}
    if operation == "publish_capability":
        if shared_graph is None:
            return {"ok": False, "detail": "SharedGraph is unavailable"}
        evidence = [
            int(seq) for seq in (payload.get("evidence_fact_seqs") or [])
            if str(seq).isdigit() and int(seq) > 0
        ]
        seq = shared_graph.publish_capability(
            actor=self.solver_id,
            capability_key=str(payload.get("capability_key") or ""),
            target_epoch=str(getattr(self, "_target_epoch", "") or "1"),
            kind=str(payload.get("kind") or "generic"),
            quality=str(payload.get("quality") or ""),
            sharing=str(payload.get("sharing") or "run-shared"),
            source_intent=_current_intent_id(self),
            evidence_fact_seqs=evidence,
            metadata=dict(payload.get("metadata") or {}),
        )
        if seq <= 0:
            return {"ok": False, "detail": "capability evidence is invalid or unchanged"}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "CAPABILITY_PUBLISHED", "seq": seq}
    if operation == "launch_runtime_resource":
        if shared_graph is None:
            return {"ok": False, "detail": "SharedGraph is unavailable"}
        command = str(payload.get("command") or "").strip()
        cleanup_command = str(payload.get("cleanup_command") or "").strip()
        name = re.sub(r"[^a-zA-Z0-9_.-]+", "-", str(payload.get("name") or "resource"))[:48]
        cwd_host = Path(str(getattr(self, "_current_workdir", "") or self._workdir)).resolve()
        resource_id = f"RR-{name}-{uuid.uuid4().hex[:12]}"
        log_host = cwd_host / ".muteki-runtime" / f"{resource_id}.log"
        log_host.parent.mkdir(parents=True, exist_ok=True)
        backend = "container" if self.container is not None else "local"
        container_name = str(getattr(self.container, "container", "") or "")
        mapper = getattr(self.container, "to_container_path", None)
        cwd_runtime = str(cwd_host)
        log_runtime = str(log_host)
        if callable(mapper):
            cwd_runtime = str(mapper(str(cwd_host)))
            log_runtime = str(mapper(str(log_host)))
        from muteki.swarm.runtime_resources import (
            allocate_runtime_port, launch_runtime_resource,
        )
        worker_env = self._worker_env(str(cwd_host))
        keep = {
            "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG",
            "LC_ALL", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
            "http_proxy", "https_proxy", "all_proxy", "no_proxy",
            "MUTEKI_TOOLBOX_DIR",
        }
        runtime_env = {key: value for key, value in worker_env.items() if key in keep}
        allocated_port = 0
        if bool(payload.get("allocate_port")):
            allocated_port = allocate_runtime_port(
                backend=backend, container_name=container_name)
            runtime_env["MUTEKI_RESOURCE_PORT"] = str(allocated_port)
        try:
            launched = launch_runtime_resource(
                command=command, cwd=cwd_runtime, env=runtime_env,
                log_path=log_runtime, backend=backend,
                container_name=container_name,
            )
        except Exception as exc:
            return {"ok": False, "detail": f"runtime launch failed: {type(exc).__name__}: {exc}"}
        seq = shared_graph.register_runtime_resource(
            actor=self.solver_id, resource_id=resource_id,
            target_epoch=str(getattr(self, "_target_epoch", "") or "1"),
            owner_intent=_current_intent_id(self), backend=backend,
            pid=launched.get("pid"), container_name=container_name,
            log_path=str(log_host), cwd=cwd_runtime, env=runtime_env,
            cleanup_command=cleanup_command,
            health=dict(payload.get("health") or {}),
        )
        if seq <= 0:
            from muteki.swarm.runtime_resources import stop_runtime_resource
            stop_runtime_resource(launched)
            return {"ok": False, "detail": "runtime resource registration failed"}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "RESOURCE_RUNNING", "resource_id": resource_id,
                "port": allocated_port, "log_path": str(log_host)}
    if operation == "publish_access_path":
        if shared_graph is None:
            return {"ok": False, "detail": "SharedGraph is unavailable"}
        resource_id = str(payload.get("runtime_resource_id") or "").strip()
        health = dict(payload.get("health") or {})
        if health.get("kind") == "tcp":
            from muteki.swarm.runtime_resources import tcp_health
            if not tcp_health(
                str(health.get("host") or "127.0.0.1"),
                int(health.get("port") or 0),
                backend=("container" if self.container is not None else "local"),
                container_name=str(getattr(self.container, "container", "") or ""),
            ):
                return {"ok": False, "detail": "access-path health check failed"}
        access_path_id = f"AP-{hashlib.sha256(resource_id.encode()).hexdigest()[:16]}"
        seq = shared_graph.publish_access_path(
            actor=self.solver_id, access_path_id=access_path_id,
            target_epoch=str(getattr(self, "_target_epoch", "") or "1"),
            runtime_resource_id=resource_id,
            source_intent=_current_intent_id(self),
            reach=list(payload.get("reach") or []),
            operations=list(payload.get("operations") or []),
            quality=str(payload.get("quality") or "multiplexed"),
            endpoint=str(payload.get("endpoint") or ""),
            use_spec=dict(payload.get("use_spec") or {}), health=health,
            dependency_fact_seqs=list(payload.get("dependency_fact_seqs") or []),
            capability_keys=list(payload.get("capability_keys") or []),
        )
        if seq <= 0:
            return {"ok": False, "detail": "access path contract or dependencies are invalid"}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "ACCESS_PATH_READY",
                "access_path_id": access_path_id, "seq": seq}
    if operation == "submit_flag":
        if not await self._accept_submitted_flag(str(payload.get("flag") or "")):
            return {"ok": False, "detail": "Flag was empty, duplicate, or rejected"}
        self._accepted_blackboard_requests += 1
        return {"ok": True, "message": "SUBMITTED"}
    if operation == "branch_proposal":
        if shared_graph is None:
            return {"ok": False, "detail": "SharedGraph is unavailable"}
        goal = str(payload.get("goal") or "").strip()
        expected = str(payload.get("expected_observable") or "").strip()
        stop = str(payload.get("stop_condition") or "").strip()
        if not goal or not expected or not stop:
            return {
                "ok": False,
                "detail": "branch requires goal, expected observable, and stop condition",
            }
        current_intent = _current_intent_id(self)
        parent_branch = str(getattr(self, "branch_id", "") or "").strip()
        identity = "\x1f".join((parent_branch, current_intent, goal.casefold()))
        digest = hashlib.sha256(
            identity.encode("utf-8", errors="replace")).hexdigest()[:16]
        branch_id = f"branch-worker-{digest}"
        from_facts = list(getattr(self, "from_facts", []) or [])
        if int(getattr(self, "_last_fact_seq", -1) or -1) > 0:
            from_facts.append(int(self._last_fact_seq))
        from_facts = sorted(set(from_facts))
        coverage_key = str(payload.get("coverage_key") or "").strip()
        route_hash = str(payload.get("route_hash") or "").strip()[:180]
        lane_key = str(payload.get("lane_key") or "").strip()[:240]
        risk_class = str(payload.get("risk_class") or "").strip()[:80]
        resource_key = str(payload.get("resource_key") or "").strip()[:240]
        result = shared_graph.split_branch(
            actor=self.solver_id,
            title=goal,
            parent_id=parent_branch,
            branches=[{
                "id": branch_id,
                "assumption": goal,
                "prove_or_disprove": expected,
                "source_intent": current_intent,
                "from_facts": from_facts,
                "expected_observable": expected,
                "stop_condition": stop,
                "coverage_key": coverage_key,
                "route_hash": route_hash,
                "lane_key": lane_key,
                "risk_class": risk_class,
                "resource_key": resource_key,
            }],
        )
        self._accepted_blackboard_requests += 1
        return {
            "ok": True,
            "message": f"DECLARED {branch_id}",
            "branch_id": branch_id,
            "seq": int(result.get("seq") or 0),
        }
    if operation in {
            "review_finding", "fact_challenge", "fact_merge",
            "fact_reject", "fact_revalidation"}:
        marker = {
            "review_finding": "REVIEW_FINDING",
            "fact_challenge": "FACT_CHALLENGE",
            "fact_merge": "FACT_MERGE",
            "fact_reject": "FACT_REJECT",
            "fact_revalidation": "FACT_REVALIDATION",
        }[operation]
        review_payload = dict(payload)
        review_payload.pop("protocol", None)
        review_payload.pop("request_id", None)
        review_payload.pop("operation", None)
        review_payload.pop("created_at", None)
        applied = await self._apply_review_actions([(marker, review_payload)])
        if applied:
            self._accepted_blackboard_requests += 1
        return {"ok": applied > 0, "message": "PROPOSED" if applied else ""}
    if shared_graph is None:
        return {"ok": False, "detail": "SharedGraph is unavailable"}
    if operation == "submission_lock":
        action = str(payload.get("action") or "")
        resource_key = "submission:verifier"
        if action == "acquire":
            if self._verifier_locked_now() or self._submit_blocked_now():
                return {"ok": True, "won": False, "message": "LOCKED"}
            lock = shared_graph.request_resource_lock(
                actor=self.solver_id, resource_key=resource_key,
                scope="challenge", risk_class="rate_limited",
                owner_worker=self.solver_id, lease_s=self._SUBMIT_HOLD_S)
            won = bool(lock.get("acquired"))
            if won and self.insight is not None:
                await self.insight.submit_locked(self.solver_id)
            return {"ok": True, "won": won,
                    "message": "ACQUIRED" if won else "HELD"}
        released = shared_graph.release_resource_lock(
            actor=self.solver_id, resource_key=resource_key,
            by_worker=self.solver_id)
        if self.insight is not None:
            await self.insight.submit_unlocked(
                self.solver_id, str(payload.get("note") or ""))
        return {"ok": True, "won": bool(released.get("released")),
                "message": "RELEASED"}
    if operation == "claim_intent":
        won = bool(shared_graph.claim_intent(
            worker=self.solver_id,
            intent_id=str(payload.get("intent_id") or "")))
        return {"ok": True, "won": won}
    if operation == "claim_activity":
        won = bool(shared_graph.try_claim_activity(
            worker=self.solver_id, key=str(payload.get("key") or "")))
        return {"ok": True, "won": won}
    if operation == "claim_resource":
        lock = shared_graph.request_resource_lock(
            actor=self.solver_id,
            resource_key=str(payload.get("resource_key") or ""),
            scope=str(payload.get("scope") or "activity"),
            risk_class=str(payload.get("risk_class") or ""),
            owner_worker=self.solver_id)
        won = bool(lock.get("acquired"))
        return {"ok": True, "won": won,
                "detail": "" if won else f"held_by={lock.get('held_by') or 'teammate'}"}
    released = shared_graph.release_resource_lock(
        actor=self.solver_id,
        resource_key=str(payload.get("resource_key") or ""),
        by_worker=self.solver_id)
    return {"ok": True, "won": bool(released.get("released"))}


async def _drain_blackboard_requests_once(self) -> int:
    async with self._blackboard_drain_lock:
        return await _drain_blackboard_requests_locked(self)


async def _drain_blackboard_requests_locked(self) -> int:
    ingress_dir = getattr(self, "_blackboard_ingress_dir", None)
    if not ingress_dir:
        return 0
    try:
        paths = sorted(Path(ingress_dir).glob("request-*.json"))
    except OSError:
        return 0
    handled = 0
    for path in paths:
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
            if not isinstance(payload, dict):
                raise ValueError("request payload is not an object")
        except Exception:
            try:
                os.replace(path, path.with_suffix(".bad"))
            except OSError:
                pass
            continue
        try:
            result = await self._handle_blackboard_request(payload)
        except Exception as exc:
            if str(getattr(exc, "code", "") or "") in {
                "tool_artifact_persistence_failed",
                "event_persistence_failed",
            }:
                _stop_for_blackboard_terminal_error(self, exc)
                raise
            result = {"ok": False, "detail": f"{type(exc).__name__}: {exc}"}
        try:
            self._write_blackboard_result(
                str(payload.get("request_id") or ""), result)
        except Exception as exc:
            failure = BlackboardReceiptPersistenceError(
                f"Blackboard receipt could not be persisted: "
                f"{type(exc).__name__}: {exc}"
            )
            _stop_for_blackboard_terminal_error(self, failure)
            raise failure from exc
        if result.get("ok") and result.get("finish_worker"):
            # The commit receipt is durable and its response file now exists.
            # End only the current CLI invocation; the logical Worker completes
            # normally in cli_modes.
            self._finish_event.set()
            with self._procs_lock:
                live = list(self._live_procs)
            for proc in live:
                self._signal_proc(proc, getattr(signal, "SIGKILL", 9))
        try:
            path.unlink()
        except OSError:
            pass
        handled += 1
    return handled


def _stop_for_blackboard_terminal_error(self, exc: BaseException) -> None:
    self._blackboard_terminal_error = exc
    with self._procs_lock:
        live = list(self._live_procs)
    for proc in live:
        self._signal_proc(proc, getattr(signal, "SIGKILL", 9))


async def _blackboard_drain_loop(self) -> None:
    while True:
        await self._drain_blackboard_requests_once()
        await asyncio.sleep(0.2)


def _write_blackboard_result(self, request_id: str, result: dict) -> None:
    ingress_dir = getattr(self, "_blackboard_ingress_dir", None)
    safe_id = re.sub(r"[^A-Za-z0-9_-]", "", str(request_id or ""))
    if not ingress_dir or not safe_id:
        return
    body = {"request_id": safe_id, **dict(result)}
    fd, temporary = tempfile.mkstemp(
        prefix=f".result-{safe_id}-", suffix=".tmp", dir=str(ingress_dir))
    final_path = Path(ingress_dir) / f"result-{safe_id}.json"
    try:
        try:
            owner = Path(ingress_dir).stat()
            os.fchown(fd, owner.st_uid, owner.st_gid)
        except (AttributeError, OSError):
            pass
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(body, handle, ensure_ascii=False, separators=(",", ":"))
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, final_path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass


async def _maybe_broadcast_lockout(self, text: str) -> None:
    """Parse a cooldown/burn-lockout duration out of verifier output and, if it
    extends the known lock, record it locally + broadcast VERIFIER_LOCKED once
    per distinct deadline so siblings stop submitting. Best-effort.

    GUARD: only treat this as a real lockout if the text is the VERIFIER's own
    verdict (not a worker reading a doc/file that describes the lockout) — else
    any chunk mentioning "burn-lockout … 30 min" broadcasts a phantom backoff
    (run-11553: a worker read docs/PROBLEM_verifier_*.md → fake 30-min lock)."""
    if not _looks_like_verifier_output(text):
        return
    secs = _parse_lockout_seconds(text)
    if secs <= 0:
        return
    deadline = time.time() + secs
    # only act if this lock is MEANINGFULLY later than what we already know (avoid
    # re-broadcasting the same lock every chunk); 5s slop absorbs clock jitter.
    if deadline <= self._verifier_locked_until + 5:
        return
    self._verifier_locked_until = deadline
    await self._emit_bb("dead_end",
                        reason=f"[{self.driver.name}] verifier locked ~{int(secs)}s — "
                               "swarm backing off submissions")
    if self.insight is not None:
        try:
            await self.insight.verifier_locked(self.solver_id, secs)
        except Exception:
            pass


@staticmethod
def _stderr_tail(res: CliResult, *, max_chars: int = 1800) -> str:
    err = (getattr(res, "raw_stderr", "") or "").strip()
    if not err:
        return ""
    return "\n".join(err.splitlines()[-12:])[-max_chars:]


def _result_text_with_stderr(self, res: CliResult) -> str:
    # A runner-controlled stop is already a complete, typed outcome.  Its
    # subprocess normally exits non-zero (and may have no final assistant
    # message at all), so treating that transport shape as a generic CLI
    # failure here prevents the caller from reaching its timeout/cancel/OOM
    # convergence path.  Preserve any partial response for harvesting, while
    # leaving the stop reason on ``CliResult`` as the authoritative status.
    if (res.timed_out or res.cancelled or res.steered or res.finished
            or getattr(res, "oom_killed", False)
            or getattr(res, "output_limit", False)
            or getattr(res, "disk_limit", False)):
        text = res.text or ""
        if text.strip():
            return text
        if res.finished:
            reason = "finished after commit-step"
        elif res.timed_out:
            reason = "timed out"
        elif getattr(res, "oom_killed", False):
            reason = "was terminated by OOM"
        elif getattr(res, "output_limit", False):
            reason = "exceeded the stdout/stderr output budget"
        elif getattr(res, "disk_limit", False):
            reason = "exceeded the workdir disk budget"
        elif res.steered:
            reason = "was steered"
        else:
            reason = "was cancelled"
        return (
            f"[{self.driver.name}] worker {reason}; "
            "no assistant text was captured."
        )
    error = str(getattr(res, "error", "") or "").strip()
    if res.returncode not in (None, 0):
        error = error or f"{self.driver.name} exited with code {res.returncode}"
    text = res.text or ""
    if error:
        diagnostic = "\n".join(filter(None, (error, _stderr_tail(res))))
        runtime_code = _worker_runtime_failure_code(res)
        if runtime_code:
            raise WorkerRuntimeUnavailable(
                diagnostic, code=runtime_code)
        raise RuntimeError(error)
    if not text.strip():
        raise RuntimeError(
            f"{self.driver.name} turn ended without assistant text")
    return text


@staticmethod
def _controlled_result_stop(
    res: CliResult,
) -> "Optional[tuple[str, str, str]]":
    """Map a runner-controlled stop to intent result, stop tag and detail."""
    if res.cancelled:
        return (
            RESULT_CANCELLED, "cancelled",
            "Worker was cancelled before completing its assignment.",
        )
    if res.steered:
        return (
            RESULT_STEERED, "steered",
            "Worker was steered before completing its assignment.",
        )
    if getattr(res, "oom_killed", False):
        return (
            RESULT_OOM, "oom",
            "Worker was terminated by OOM before completing its assignment.",
        )
    if getattr(res, "output_limit", False):
        return (
            RESULT_OUTPUT_LIMIT, "output_limit",
            "Worker exceeded the stdout/stderr output budget before completing "
            "its assignment.",
        )
    if getattr(res, "disk_limit", False):
        return (
            RESULT_DISK_LIMIT, "disk_limit",
            "Worker exceeded the workdir disk budget before completing its "
            "assignment.",
        )
    if res.timed_out:
        return (
            RESULT_TIMED_OUT, "timeout",
            "Worker timed out before completing its assignment.",
        )
    return None


async def _emit_empty_stderr_diagnostic(self, res: CliResult) -> None:
    if (res.text or "").strip():
        return
    tail = self._stderr_tail(res, max_chars=1200)
    if tail:
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] produced no stdout; stderr tail:\n{tail}\n",
        )




def _current_run_tool_allows_verified(
    self, *, fact: str = "", artifact_id: str = "", witness: str = "",
    provenance: Optional[dict] = None,
) -> bool:
    """Validate every persisted binding needed for a Fact promotion."""
    p = dict(provenance or {})
    if not fact or not artifact_id or not p:
        return False
    if str(p.get("worker_id") or "") != self.solver_id:
        return False
    if str(p.get("intent_id") or "") != _current_intent_id(self):
        return False
    if not _target_epoch_matches(self, str(p.get("target_epoch") or "")):
        return False
    if str(p.get("artifact_id") or "") != artifact_id:
        return False
    try:
        event_seq = int(p.get("tool_event_seq") or 0)
        event_at = float(p.get("tool_event_ts") or 0.0)
        observed_at = float(p.get("observed_at") or 0.0)
        promoted_at = float(p.get("promoted_at") or 0.0)
    except (TypeError, ValueError):
        return False
    if (event_seq <= 0 or event_at <= 0 or observed_at <= 0
            or event_at + 1.0 < observed_at or promoted_at < event_at):
        return False
    if observed_at < float(getattr(self, "_worker_started_at", 0.0) or 0.0):
        return False
    records = list(getattr(self, "_tool_evidence_records", None) or [])
    for record in records:
        if (record.event_seq != event_seq
                or record.event_at != event_at
                or record.worker_id != self.solver_id
                or record.intent_id != _current_intent_id(self)
                or record.target_epoch != str(p.get("target_epoch") or "")
                or record.artifact_id != artifact_id
                or record.artifact_sha256 != str(p.get("artifact_sha256") or "")
                or not record.attributed
                or not _record_artifact_matches(self, record)):
            continue
        body = _fact_evidence_body(record.output)
        if witness and witness not in body:
            continue
        if _fact_supported_by_tool_event(
                self, fact, output=record.output, command=record.command):
            return True
    return False




def _estimated_cli_result_from_stream(self) -> CliResult:
    """Floor usage from live stream activity when the vendor omitted usage.

    DeepSeek-via-claude.orig kills often leave no final ``result`` / usage
    block. Tool starts and reasoning chars still prove the model ran.
    """
    tools = int(getattr(self, "_stream_tool_starts", 0) or 0)
    reason_chars = int(getattr(self, "_stream_reasoning_chars", 0) or 0)
    out_tok = 0
    if tools or reason_chars:
        out_tok = max(1, (reason_chars // 4) + (tools * 256))
    in_tok = out_tok * 4 if out_tok else 0
    return CliResult(
        text="",
        cost_usd=None,
        usage_estimated=True,
        input_tokens=in_tok or None,
        output_tokens=out_tok or None,
    )


async def _flush_stream_activity_cost(self) -> None:
    """Emit a floor COST_UPDATE from stream counters if nothing was charged yet."""
    if self.cost is None or self._stream_cost_flushed:
        return
    est = self._estimated_cli_result_from_stream()
    if not (est.input_tokens or est.output_tokens):
        return
    await self._stream_cost(est)


async def _stream_cost(self, res: CliResult) -> None:
    if self.cost is None:
        return
    from muteki.core.usage import cli_usage
    observed_usage = cli_usage(res)
    profile = dict(
        getattr(self, "_worker_profile", None)
        or getattr(getattr(self, "driver", None), "profile", None)
        or {}
    )
    model_name = str(
        profile.get("model")
        or getattr(self, "model", "")
        or __import__("os").environ.get("MUTEKI_WORKER_MODEL")
        or ""
    ).strip()
    generation = getattr(getattr(self, "_session_supervisor", None), "execution_generation", None)
    engine = str(
        profile.get("engine")
        or getattr(getattr(self, "driver", None), "name", "")
        or getattr(self, "cli_engine", "")
        or ""
    ).strip()
    usd = res.cost_usd
    in_tok = int(res.input_tokens or 0)
    out_tok = int(res.output_tokens or 0)
    # Record when we have EITHER a dollar cost OR token usage. claude reports a
    # real dollar cost; codex's was re-derived from tokens in the driver; cursor
    # is subscription-backed (usd is None → $0) but still reports tokens, so it
    # contributes to the deck's token-usage column at zero cost.
    if usd is None and not (in_tok or out_tok):
        # Last resort: price from live stream activity so a killed turn with
        # real tools cannot seal a zero-cost receipt.
        est = self._estimated_cli_result_from_stream()
        in_tok = int(est.input_tokens or 0)
        out_tok = int(est.output_tokens or 0)
        if not (in_tok or out_tok):
            if self.cost.usage_sink is not None:
                self.cost.usage_sink(observed_usage, identity=res.usage_id, model=model_name, worker_id=self.solver_id, role="worker", actor_kind="worker")
            return
    try:
        reported = None if usd is None else float(usd)
        # A missing/zero native amount does not identify a tariff. An exact
        # configured model price can produce an estimate; otherwise keep the
        # tokens without inventing a dollar amount.
        if (reported is None or reported <= 0.0) and (in_tok or out_tok):
            await self.cost.record(
                model=model_name,
                input_tokens=in_tok,
                output_tokens=out_tok,
                run_id=self.run_id,
                solver_id=self.solver_id,
                challenge_id=self.challenge.id,
                usage=observed_usage, usage_id=res.usage_id, actor_kind="worker", generation=generation, engine=engine,
            )
            self._stream_cost_flushed = True
            return
        await self.cost.add_external_usd(
            float(reported or 0.0), run_id=self.run_id,
            solver_id=self.solver_id, challenge_id=self.challenge.id,
            input_tokens=in_tok, output_tokens=out_tok,
            usage=observed_usage, usage_id=res.usage_id, model=model_name, actor_kind="worker", generation=generation, engine=engine)
        self._stream_cost_flushed = True
    except Exception:
        __import__("logging").getLogger(__name__).exception("usage settlement failed for worker %s", self.solver_id)


def _is_solved_claim(self, fact: str) -> bool:
    """True if `fact` is a bare completion claim (not concrete evidence). A claim
    accompanying a real flag this worker actually accepted is NOT downgraded —
    the flag is the proof, the surrounding text is fine."""
    if not fact:
        return False
    if self._already_found:
        return False  # this worker holds a real gated flag → its claims are earned
    return bool(self._SOLVED_CLAIM_RE.search(fact))


async def _record_fact(self, fact: str, *, verified: bool, artifact_id: str,
                       witness: str = "",
                       provenance: Optional[dict] = None,
                       subject: str = "", predicate: str = "",
                       object_value: object = None, scope: str = "",
                       canonical_key: str = "") -> int:
    """Publish one Fact without delay.

    Every claim remains part of the end-of-life WorkerResult receipt.  A
    claim is also written to SharedGraph immediately so Reason and sibling
    Workers operate on current evidence while its producer is alive.  Claims
    CTF claims enter the shared graph directly. Other modes retain their existing
    evidence admission behavior. The final receipt is idempotent.
    """
    graph_provenance = _fact_claim_provenance(self, provenance)
    ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
    if ctf_mode:
        verified = True
    else:
        provenance_ok = bool(
            verified and self._current_run_tool_allows_verified(
                fact=fact, artifact_id=artifact_id, witness=witness,
                provenance=provenance)
        )
        if verified and self._is_solved_claim(fact):
            verified = False
            await self._emit_bb("claim_solved_rejected", claim=fact[:200],
                                worker=self.solver_id)
        elif verified and not (
            (artifact_id and artifact_id.strip()) or (witness and witness.strip())
        ):
            verified = False
        elif verified and not provenance_ok:
            verified = False
    if verified:
        self.graph.add_evidence(
            source=self.driver.name, fact=fact, artifact_id=artifact_id,
            verified=True,
        )
        await self._emit(
            EventType.SOLVE_GRAPH_DELTA,
            **solve_graph_delta_payload(
                "evidence_added", source=self.driver.name, fact=fact))
    # Accumulate for the checkpoint/final receipt as well. Deduplicate repeated
    # Skill requests by normalized fact text; an evidence-backed repeat upgrades
    # a queued candidate in place instead of double-accumulating. The graph's
    # identity dedupe remains the backstop for the immediate + receipt writes.
    claim = ObservationClaim(
        text=fact, claim_verified=bool(verified),
        provenance=dict(graph_provenance), subject=subject,
        predicate=predicate, object_value=object_value, scope=scope,
        canonical_key=canonical_key, witness=witness)
    key = canonical_key or " ".join(str(fact).split()).lower()
    index = self._pending_observation_index.get(key)
    if index is None:
        self._pending_observation_index[key] = len(self._pending_observations)
        self._pending_observations.append(claim)
    elif verified and not self._pending_observations[index].claim_verified:
        self._pending_observations[index] = claim
    if self.shared_graph is not None:
        return self.shared_graph.add_evidence(
            actor=self.solver_id,
            source=self.driver.name,
            fact=fact,
            artifact_id=artifact_id or None,
            verified=bool(verified),
            witness=witness or None,
            intent_id=_current_intent_id(self),
            provenance=graph_provenance,
            subject=subject, predicate=predicate, object_value=object_value,
            scope=scope, canonical_key=canonical_key,
        )
    return -1


def _build_worker_result(self, status: str, result_detail: str = "") -> WorkerResult:
    """Assemble this run's structured result from the accumulated claims.

    produced_new_info: a verified observation carrying provenance, a saved PoC,
    or a Flag accepted this run — the signals that this worker moved the board.
    stop_condition_result stays best-effort: "met" only on a solved status.
    """
    observations = list(getattr(self, "_pending_observations", None) or [])
    dead_ends = list(getattr(self, "_pending_dead_ends", None) or [])
    pocs = list(getattr(self, "_pending_pocs", None) or [])
    produced_new_info = bool(
        self._stream_accepted or pocs
        or any(o.claim_verified and o.provenance for o in observations))
    return WorkerResult(
        status=status,
        result_detail=result_detail,
        produced_new_info=produced_new_info,
        stop_condition_result=("met" if status == RESULT_SOLVED else ""),
        observations=observations,
        dead_ends=dead_ends,
        pocs=pocs,
        artifact_ids=[str(getattr(self, "_transcript_artifact_id", "") or "")]
        if getattr(self, "_transcript_artifact_id", "") else [],
        need_input=getattr(self, "_pending_need_input", None),
    )


async def _commit_worker_result(self, status: str, result_detail: str = "",
                                *, handoff_missing: bool = False,
                                checkpoint: bool = False) -> dict:
    """Commit accumulated evidence atomically at a checkpoint or final exit.

    Checkpoints publish the current observations, dead ends, PoCs and NEED_INPUT
    without concluding the Intent or latching final completion. Final exit uses a
    separate stable receipt and concludes the owned Intent last. Both paths keep
    the same provenance gate and transaction boundary.
    """
    try:
        await self._drain_blackboard_requests_once()
    except Exception:
        pass
    if not checkpoint and self._worker_result_committed:
        return dict(getattr(self, "_last_worker_result_commit", None) or {})
    wr = self._build_worker_result(status, result_detail=result_detail)
    if not checkpoint:
        salvage = unanswered_observation_claim(self, status)
        if salvage is not None:
            wr = replace(
                wr,
                observations=[*list(wr.observations), salvage],
                produced_new_info=wr.produced_new_info,
            )
    if checkpoint and not (
        wr.observations or wr.dead_ends or wr.pocs or wr.need_input
    ):
        return {}
    if handoff_missing and not wr.handoff_missing:
        wr = WorkerResult.salvage(
            status=wr.status, result_detail=wr.result_detail,
            produced_new_info=wr.produced_new_info,
            stop_condition_result=wr.stop_condition_result,
            observations=wr.observations, dead_ends=wr.dead_ends,
            pocs=wr.pocs, artifact_ids=wr.artifact_ids,
            need_input=wr.need_input)
    if not checkpoint:
        self._last_worker_result = wr
    shared_graph = getattr(self, "shared_graph", None)
    commit_worker_result = getattr(shared_graph, "commit_worker_result", None)
    if not callable(commit_worker_result):
        return {}
    intent_id = str(
        getattr(self, "_intent_id", "") or self.intent_id_assigned or "")
    checkpoint_id = (
        f"checkpoint-{int(getattr(self, '_worker_checkpoint_index', 0)) + 1}"
        if checkpoint else ""
    )
    commit: object = None
    last_error: Optional[BaseException] = None
    # A failed graph transaction remains retryable.  Two immediate attempts cover
    # transient SQLite busy/lock windows; a persistent failure is raised so the
    # worker/reaper salvage path can make a final handoff_missing attempt.  The
    # committed latch is set only after a valid commit receipt exists.
    for attempt in range(2):
        try:
            commit = commit_worker_result(
                **wr.to_commit_kwargs(
                    actor=self.solver_id, worker_id=self.solver_id,
                    intent_id=intent_id,
                    target_epoch=str(getattr(self, "_target_epoch", "") or ""),
                    run_id=str(getattr(self, "run_id", "") or "")),
                checkpoint_id=checkpoint_id,
                conclude=bool(intent_id) and not checkpoint)
            last_error = None
            break
        except Exception as exc:
            last_error = exc
            self._worker_result_commit_error = (
                f"{type(exc).__name__}: {exc}"
            )[:1000]
            if attempt == 0:
                await asyncio.sleep(0)
    if last_error is not None:
        raise last_error
    if not isinstance(commit, dict):
        self._worker_result_commit_error = "invalid commit receipt"
        raise RuntimeError("shared graph returned an invalid worker-result receipt")
    if not checkpoint:
        self._last_worker_result_commit = dict(commit)
    self._worker_result_commit_error = ""
    if not checkpoint:
        self._worker_result_committed = True
    else:
        self._worker_checkpoint_index = int(
            getattr(self, "_worker_checkpoint_index", 0)
        ) + 1
    fact_seqs: list[int] = []
    for seq in (list(commit.get("facts") or [])
                + list(commit.get("candidates") or [])):
        try:
            seq = int(seq)
        except (TypeError, ValueError):
            continue
        if seq > 0:
            fact_seqs.append(seq)
    if fact_seqs:
        self._last_fact_seq = max(fact_seqs)
        bind_branch_facts = getattr(
            shared_graph, "bind_open_branch_facts", None)
        if (callable(bind_branch_facts) and intent_id
                and getattr(self.challenge, "mode", "ctf") == "ctf"):
            branch_binding = bind_branch_facts(
                actor=self.solver_id,
                source_intent=intent_id,
                fact_seqs=fact_seqs,
            )
            if (isinstance(branch_binding, dict)
                    and branch_binding.get("branch_ids")):
                try:
                    await self._emit(
                        EventType.BLACKBOARD_DELTA,
                        **blackboard_delta_payload(
                            "branch_facts_bound",
                            actor=self.solver_id,
                            source_intent=intent_id,
                            branch_ids=branch_binding["branch_ids"],
                            from_facts=branch_binding["fact_seqs"],
                        ),
                    )
                except asyncio.CancelledError:
                    pass
                except Exception:
                    pass
    if fact_seqs or commit.get("dead_ends") or commit.get("pocs"):
        try:
            await self._emit(
                EventType.SHARED_GRAPH_DELTA,
                **shared_graph_delta_payload(
                    f"worker_result:{commit.get('result') or status}",
                    verified=bool(commit.get("facts")),
                    confidence=1.0 if commit.get("facts") else 0.4,
                    actor=self.solver_id,
                    fact_seq=int(
                        (commit.get("seqs") or {}).get("summary") or -1)))
        except asyncio.CancelledError:
            pass
        except Exception:
            pass
    if checkpoint:
        # Only clear data covered by this durable receipt. Session state, tool
        # history, accepted Flags and the final-result latch remain untouched.
        self._pending_observations = []
        self._pending_observation_index = {}
        self._pending_dead_ends = []
        self._pending_pocs = []
        self._pending_need_input = None
    return commit


def _summarize_async(self, text: str, *, node_kind: str,
                     fact_seq: int = -1, intent_id: str = "") -> None:
    """Fire-and-forget a deepseek-flash zh gist for a fact/intent node.

    Only runs in a web/bus context (the deck renders the gist); a bare CLI
    race with no bus skips it. Never blocks the worker: the summary lands a
    few seconds later via NODE_SUMMARIZED and is stored once on the graph.
    Skips trivially short text — a 30-char fact is already its own gist."""
    if self.bus is None or len((text or "").strip()) < 48:
        return
    from muteki.solver.summarizer import summarize_node
    try:
        asyncio.create_task(summarize_node(
            text, node_kind=node_kind, fact_seq=fact_seq, intent_id=intent_id,
            shared_graph=self.shared_graph, bus=self.bus,
            run_id=self.run_id, challenge_id=self.challenge.id))
    except RuntimeError:
        # no running loop (shouldn't happen on the async path) — skip silently
        pass


def _rejected_flags(self) -> "set[str]":
    """Flag values the operator marked as FALSE POSITIVES on the shared graph.

    Reads the SAME durable, respawn-surviving log the coordinator's flag
    reconciliation uses (shared_graph.invalidated_flags → EV_FLAG_INVALIDATED) —
    ONE source of truth, not a parallel one. NOT this worker's `_already_found`
    (per-instance; a fresh worker after a false-positive reopen inherits the
    SURVIVING flags but never the rejected ones). Best-effort: an unreachable
    graph yields an empty set rather than blocking acceptance."""
    sg = getattr(self, "shared_graph", None)
    if sg is None:
        return set()
    try:
        return set(sg.invalidated_flags() or set())
    except Exception:
        return set()


def _accepted_flags_for_outcome(self) -> "list[str]":
    """Return this worker's accepted flags for the run outcome."""
    return list(self.graph.flags)


async def _accept_flag(self, flag: str) -> bool:
    """Record + broadcast ONE distinct flag. Dedup against this worker's
    already-accepted set (a flag found twice, or one a sibling already
    broadcast, is a no-op). Returns True if it was new. Does NOT emit the
    terminal lifecycle event — that fires once in run() with ALL flags, so a
    multi-flag worker can accept several and finish once."""
    if flag in self._already_found:
        return False
    if flag in self._rejected_flags():
        await self._emit_bb(
            "flag_reaccept_blocked", flag=flag,
            reason="operator marked this flag false-positive; permanently rejected")
        return False
    # Persist first. If the host is interrupted before the request file is
    # removed, the next drain sees the durable graph row and safely consumes the
    # duplicate. A persistence error leaves the request available for retry.
    if self.shared_graph is not None:
        self.shared_graph.flag_found(
            actor=self.solver_id, flag=flag,
            intent_id=getattr(self, "_intent_id", "") or None,
            # The Worker closes its Step only after publishing its final Fact.
            # Coordinator cancellation still salvages a solved result if the
            # same-session conclusion cannot complete before its deadline.
            complete_intent=False,
        )
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            try:
                self.shared_graph.finding_found(
                    actor=self.solver_id,
                    finding=flag_finding_payload(
                        flag=flag,
                        intent_id=_current_intent_id(self),
                    ),
                    intent_id=_current_intent_id(self),
                )
            except Exception:
                pass
    # Latch the accepted submission before the first await below. Coordinator
    # completion may observe the durable Flag immediately and cancel this task;
    # cancellation cleanup must already know that this Worker completed the goal.
    self._stream_accepted.append(flag)
    self._already_found.add(flag)
    self.graph.add_flag(flag)
    await self._emit(EventType.SOLVE_GRAPH_DELTA,
                     **solve_graph_delta_payload("flag", flag=flag))
    await self._emit(EventType.INSIGHT_BUS_EVENT,
                     **insight_payload("FlagFound", flag=flag, by=self.solver_id))
    await self._emit_bb("flag_found", flag=flag)
    if self.insight is not None:
        try:
            await self.insight.flag_found(self.solver_id, flag)
        except Exception:
            pass
    return True
