"""Pentest finding, report, reproduction, PoC, and review result publication."""
from __future__ import annotations

import asyncio
import hashlib
from collections.abc import Mapping
from pathlib import Path
from typing import Any, Optional

from muteki.solver.gate import finding_key as _finding_key
from muteki.swarm.graph_defs import REVIEW_FACT_MARKERS
from muteki.solver.result_codes import RESULT_SOLVED
from muteki.solver.types import SolveOutcome
from muteki.solver.worker_result import PocClaim
from muteki.solver.vuln_report import (
    VALUE_REJECT_DUPLICATE,
    VALUE_REJECT_INCOMPLETE,
    VALUE_REJECT_NOT_REPRODUCIBLE,
    completeness_code,
    missing_report_fields,
    parse_report_files,
    replay_attempted,
    report_id_from_intent,
    report_sse_fields,
    witness_in_corpus,
)
from muteki.solver.workspace import (
    materialize_shared_artifact,
    workspace_root_for_worker,
)

def _pentest_findings_ready(self) -> bool:
    """True after this worker submitted at least one complete report.

    Product success is still Coordinator accept (repro + value). This only
    stops the submitter from concluding as a CTF 'no flag' miss.
    """
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return False
    return bool(self._already_submitted_reports)


async def _finish_pentest_finding(
    self, *, session: "Optional[str]", wd: Path, steps: int, label: str,
) -> SolveOutcome:
    detail = "Exploit report submitted."
    await self._commit_worker_result(status=RESULT_SOLVED, result_detail=detail)
    lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
    if lfs is not None:
        detail = f"{detail} to_fact_seq={lfs}"
    await self._emit_bb(
        "intent_concluded", intent_id=self._intent_id,
        worker=self.solver_id, result=RESULT_SOLVED,
        to_fact_seq=lfs,
        result_detail=detail)
    self._note_worker_stop("solved")
    await self._emit_finished(flag=None, flags=[], solved=True)
    return SolveOutcome(
        True, None, steps, self.graph,
        f"solved via {self.driver.name} {label}",
        session=session, engine=self.driver.name, workdir=str(wd),
        flags=[], worker_result=self._last_worker_result)


async def _submit_report_path(self, raw_path: str) -> bool:
    text = (raw_path or "").strip().strip("`\"'")
    if not text or text.startswith("<") or text.endswith(">"):
        return False
    path = self._resolve_report_path(text)
    if path is None:
        await self._emit_bb(
            "report_rejected",
            code=VALUE_REJECT_INCOMPLETE,
            reason="report file missing or outside the worker directory")
        if self.shared_graph is not None:
            try:
                self.shared_graph.report_rejected(
                    actor=self.solver_id, report_id="unknown",
                    code=VALUE_REJECT_INCOMPLETE,
                    detail="report file missing or outside the worker directory",
                    intent_id=getattr(self, "_intent_id", "") or None)
            except Exception:
                pass
        return False
    reports = parse_report_files(path)
    if not reports:
        await self._reject_report_payload(
            {}, VALUE_REJECT_INCOMPLETE, "incomplete: parse", path)
        return False
    accepted = False
    for report in reports:
        accepted = await self._accept_or_reject_report(report, path) or accepted
    return accepted


async def _reject_report_payload(
    self, report: dict, code: str, detail: str, path: "Optional[Path]" = None,
) -> None:
    await self._emit_bb(
        "report_rejected",
        report_id=(report or {}).get("report_id", ""),
        title=(report or {}).get("title", ""),
        code=code,
        reason=detail)
    if self.shared_graph is not None:
        try:
            self.shared_graph.report_rejected(
                actor=self.solver_id,
                report_id=str((report or {}).get("report_id") or (path.name if path else "unknown")),
                code=code, detail=detail,
                intent_id=getattr(self, "_intent_id", "") or None)
        except Exception:
            pass


async def _accept_or_reject_report(self, report: dict, path: Path) -> bool:
    try:
        code = completeness_code(
            report,
            scope=getattr(self.challenge, "scope", "") or "",
            target=self._target() or "",
        )
    except Exception as exc:
        await self._reject_report_payload(
            report, VALUE_REJECT_INCOMPLETE,
            f"completeness check failed: {exc}"[:200], path)
        return False
    if code != "ok":
        missing = missing_report_fields(report or {})
        detail = code if code != VALUE_REJECT_INCOMPLETE else (
            "incomplete: " + ",".join(missing or ["parse"])
        )
        await self._reject_report_payload(report, code, detail, path)
        return False
    report_id = str(report.get("report_id") or "")
    if not report_id or report_id in self._already_submitted_reports:
        return False
    report["submitter"] = self.solver_id
    report["path"] = str(path)
    if self.shared_graph is not None:
        try:
            states = self.shared_graph.report_states()
            existing = states.get(report_id) or {}
            if existing.get("status") in {"accepted", "submitted", "reproduced", "value_accepted"}:
                await self._emit_bb(
                    "report_rejected",
                    report_id=report_id, code=VALUE_REJECT_DUPLICATE,
                    reason="duplicate report identity")
                self.shared_graph.report_rejected(
                    actor=self.solver_id, report_id=report_id,
                    code=VALUE_REJECT_DUPLICATE, detail="duplicate report identity",
                    intent_id=getattr(self, "_intent_id", "") or None)
                return False
            self.shared_graph.report_submitted(
                actor=self.solver_id, report=report,
                intent_id=getattr(self, "_intent_id", "") or None)
        except Exception as exc:
            await self._emit_bb(
                "report_rejected", report_id=report_id,
                code=VALUE_REJECT_INCOMPLETE,
                reason=f"report persistence failed: {exc}"[:200])
            return False
    self._already_submitted_reports.add(report_id)
    await self._emit_bb("report_submitted", **report_sse_fields(report))
    return True


async def _submit_repro_decision(
    self, reproduced: bool, witness: str, reason: str,
) -> bool:
    if self.mode != "report_reproducer":
        return False
    report_id = report_id_from_intent(
        getattr(self, "_intent_id", "") or self.intent_id_assigned or "")
    if not report_id or getattr(self, "_repro_decided", False):
        return False
    self._repro_decided = True
    report = {}
    if self.shared_graph is not None:
        try:
            report = self.shared_graph.report_record(report_id) or {}
        except Exception:
            report = {}
    corpus = self._finding_evidence_corpus()
    commands = list(getattr(self, "_raw_tool_commands", []) or [])
    claimed_witness = witness.strip() or str(report.get("witness") or "").strip()
    ok = bool(
        reproduced
        and witness_in_corpus(claimed_witness, corpus)
        and replay_attempted(report, commands)
    )
    replay = report.get("replay") if isinstance(report.get("replay"), Mapping) else {}
    expected_command = str(replay.get("command") or "").strip().lower()
    actual_command = ""
    for command in reversed(commands):
        normalized = str(command or "").strip().lower()
        if not normalized:
            continue
        if (not expected_command
                or expected_command[:40] in normalized
                or normalized[:40] in expected_command):
            actual_command = str(command)
            break
    if not actual_command and commands:
        actual_command = str(commands[-1])
    witness_digest = hashlib.sha256(
        claimed_witness.encode("utf-8")).hexdigest() if claimed_witness else ""
    repro_receipt = {
        "command": actual_command,
        "target": str(report.get("resource_id") or self._target() or ""),
        "response_summary": (
            f"verifier output contained a {len(claimed_witness)}-character witness "
            f"(sha256:{witness_digest[:16]})"
            if claimed_witness else "no verified witness"
        ),
        "evidence": {
            "kind": "verifier_terminal_output",
            "witness_sha256": witness_digest,
            "witness_length": len(claimed_witness),
            "command_count": len(commands),
        },
    }
    if not ok:
        if not reproduced:
            detail = reason or "independent reproduction failed"
        elif not replay_attempted(report, commands):
            detail = "verifier did not re-run a replay command"
        else:
            detail = "witness missing from verifier command output"
        await self._emit_bb(
            "report_repro_failed", report_id=report_id,
            code=VALUE_REJECT_NOT_REPRODUCIBLE, reason=detail,
            **repro_receipt)
        if self.shared_graph is not None:
            self.shared_graph.report_repro_decision(
                actor=self.solver_id, report_id=report_id,
                reproduced=False, detail=detail,
                intent_id=getattr(self, "_intent_id", "") or None,
                **repro_receipt)
        return True
    await self._emit_bb(
        "report_reproduced", report_id=report_id,
        title=report.get("title", ""),
        finding_class=report.get("finding_class", ""),
        **repro_receipt)
    if self.shared_graph is not None:
        self.shared_graph.report_repro_decision(
            actor=self.solver_id, report_id=report_id,
            reproduced=True, witness=claimed_witness,
            intent_id=getattr(self, "_intent_id", "") or None,
            **repro_receipt)
    return True


async def _finalize_verifier_repro(self) -> None:
    if (self.mode != "report_reproducer"
            or getattr(self, "_repro_decided", False)):
        return
    await self._submit_repro_decision(
        False, "", "report reproducer did not submit a decision through the Skill")


async def _accept_finding(self, finding: dict) -> bool:
    if not finding:
        return False
    key = _finding_key(finding)
    if not key or key in self._already_found_findings:
        return False
    self._already_found_findings.add(key)
    if self.shared_graph is not None:
        try:
            self.shared_graph.finding_found(
                actor=self.solver_id, finding=finding,
                intent_id=getattr(self, "_intent_id", "") or None)
        except Exception:
            pass
    try:
        self.graph.add_finding(finding)
    except Exception:
        pass
    await self._emit_bb(
        "finding_found",
        finding_class=finding.get("finding_class", ""),
        resource_id=finding.get("resource_id", ""),
        identity_a=finding.get("identity_a", ""),
        identity_b=finding.get("identity_b", ""))
    return True

async def _handle_poc_save(self, path_text: str, entry_command: str,
                           status: str, note: str) -> bool:
    cwd = (self._current_workdir
           or (Path(self._workdir).resolve() if self._workdir else None))
    if cwd is None:
        return False
    try:
        src = (cwd / path_text).resolve() if not Path(path_text).is_absolute() else Path(path_text).resolve()
        src.relative_to(cwd)
    except (OSError, ValueError):
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note="POC_SAVE path must stay inside this worker cwd")
        return False
    if not src.exists() or not src.is_file():
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note="POC_SAVE path is not a regular file")
        return False
    marker_key = f"{src}:{entry_command}:{status}:{note}"
    if marker_key in self._published_pocs:
        return True
    self._published_pocs.add(marker_key)

    # The PoC save does blocking filesystem + hashing work (read_text,
    # write_text, sha256-stream + possible copytree in materialize_shared_
    # artifact). This runs while the host drains a live Skill request, so doing
    # it inline would stall every other
    # worker's stream during a large PoC write (#13). Push the whole sync block
    # to a thread, exactly like the subprocess paths already do.
    def _save_blocking() -> "Optional[tuple[dict, str, str]]":
        status_ = (status or "available").strip().lower()
        if status_ not in {"available", "wip", "directional", "spent"}:
            status_ = "available"
        local_note = note
        save_src = src
        try:
            root = workspace_root_for_worker(cwd)
            art = materialize_shared_artifact(
                root, save_src, name=src.name, kind="poc", status=status_,
                metadata={
                    "entry_command": entry_command,
                    "intent_id": getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", ""),
                    "solver_id": self.solver_id,
                },
            )
        except (OSError, FileNotFoundError):
            return None
        return art, status_, local_note

    result = await asyncio.to_thread(_save_blocking)
    if result is None:
        return False
    artifact, clean_status, note = result
    poc_id = f"poc-{artifact['sha256'][:12]}"
    intent_id = getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or None
    # The artifact/CAS materialization above stays immediate; only the graph
    # row defers into this worker's atomic end-of-life result commit.
    self._pending_pocs.append(PocClaim(
        poc_id=poc_id, path=str(artifact["path"]), entry_command=entry_command,
        status=clean_status, note=note, artifact_id=artifact["sha256"],
        name=src.name))
    await self._emit_bb(
        "poc_saved", poc_id=poc_id, intent_id=intent_id, name=src.name,
        path=str(artifact["path"]), artifact_id=artifact["sha256"],
        entry_command=entry_command, status=clean_status, note=note)
    return True


async def _mark_claimed_pocs_spent(self, reason: str) -> None:
    if self.shared_graph is None or not self._claimed_pocs:
        return
    for poc_id in list(self._claimed_pocs):
        try:
            self.shared_graph.conclude_poc(
                actor=self.solver_id, poc_id=poc_id, status="spent",
                note=f"direction dead-end: {reason[:160]}")
        except Exception:
            continue
        await self._emit_bb(
            "poc_concluded", poc_id=poc_id, status="spent",
            note=f"direction dead-end: {reason[:160]}")


async def _apply_review_actions(self, actions: list[tuple[str, Any]]) -> int:
    if self.shared_graph is None:
        return 0
    proposed = 0
    for marker, payload in actions:
        try:
            if marker not in REVIEW_FACT_MARKERS:
                await self._emit_bb(
                    "review_action_rejected", marker=marker,
                    reason="marker is outside Review authority")
                continue
            payload = dict(payload or {})
            seq = self.shared_graph.add_review_proposal(
                actor=self.solver_id, marker=marker, payload=payload)
            await self._emit_bb(
                "review_proposal",
                seq=seq,
                marker=marker,
                tier="tier1",
                route_hash=str(payload.get("route_hash") or ""),
                summary=str(
                    payload.get("summary") or payload.get("reason")
                    or payload.get("goal") or payload.get("directive")
                    or marker
                )[:240],
            )
            proposed += 1
        except Exception as exc:  # noqa: BLE001
            try:
                seq = self.shared_graph.add_review_proposal(
                    actor=self.solver_id, marker="REVIEW_FINDING",
                    payload={"kind": "invalid_action", "severity": "warn",
                             "summary": f"{marker} rejected: {exc}"})
                await self._emit_bb("review_proposal", seq=seq,
                                    marker="REVIEW_FINDING", tier="tier1",
                                    severity="warn",
                                    summary=f"{marker} rejected: {exc}")
                proposed += 1
            except Exception:
                pass
    return proposed
