from __future__ import annotations

import base64
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path, PurePosixPath
from typing import Any, Callable, Iterable, Optional

from .artifacts import SrcArtifactStore
from .contracts import ArtifactRef
from .reports import RENDERER_VERSION, REPORT_SCHEMA, SrcReportService, artifact_ref, utcnow
from .store import SrcStoreError


def _json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, indent=2).encode("utf-8")


def _filename(value: str, fallback: str = "artifact.bin") -> str:
    name = Path(str(value or "").replace("\\", "/")).name
    cleaned_chars: list[str] = []
    for ch in name:
        if ch.isalnum() or ch in {".", "_", "-"}:
            cleaned_chars.append(ch)
        elif not cleaned_chars or cleaned_chars[-1] != "-":
            cleaned_chars.append("-")
    cleaned = "".join(cleaned_chars).strip(".-")
    return cleaned or fallback


def _fence(value: str) -> str:
    longest = 0
    current = 0
    for ch in value:
        if ch == "`":
            current += 1
            if current > longest:
                longest = current
        else:
            current = 0
    return "`" * max(3, longest + 1)


def _prose(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, sort_keys=True)


def _redact_bytes(content: bytes, rules: list[dict[str, Any]]) -> tuple[bytes, dict[str, int]]:
    counts = {str(rule["value"]): 0 for rule in rules}
    for rule in rules:
        value = str(rule["value"]).encode()
        counts[str(rule["value"])] += content.count(value)
        content = content.replace(value, str(rule["replacement"]).encode())
    return content, counts


def _redact_data(value: Any, rules: list[dict[str, Any]]) -> tuple[Any, dict[str, int]]:
    counts = {str(rule["value"]): 0 for rule in rules}

    def walk(item: Any) -> Any:
        if isinstance(item, str):
            result = item
            for rule in rules:
                text = str(rule["value"])
                counts[text] += result.count(text)
                result = result.replace(text, str(rule["replacement"]))
            return result
        if isinstance(item, list):
            return [walk(value) for value in item]
        if isinstance(item, dict):
            return {key: walk(value) for key, value in item.items()}
        return item

    return walk(value), counts


class SubmissionRenderer:
    def __init__(self, reports: SrcReportService, artifacts: SrcArtifactStore) -> None:
        self.reports = reports
        self.artifacts = artifacts

    def _read(self, task_id: str, ref: ArtifactRef | dict[str, Any]) -> bytes:
        return self.artifacts.read(artifact_ref(ref), expected_task_id=task_id)

    def render(
        self, report: dict[str, Any], *, redactions: Optional[list[dict[str, Any]]] = None,
        allow_partial: bool = False,
    ) -> dict[str, Any]:
        task_id = report["task_id"]
        document = json.loads(json.dumps(report["document"]))
        template = report["template"]
        mapping = template["field_mapping"]
        issues: list[dict[str, Any]] = []
        replacements: list[dict[str, Any]] = []
        redactions = list(redactions or [])
        normalized_rules: list[dict[str, str]] = []
        for rule in redactions:
            value = str(rule.get("value") or "")
            if not value:
                raise SrcStoreError("version_conflict", "redaction value cannot be empty")
            replacement = str(rule.get("replacement") or "[已脱敏]")
            normalized_rules.append({"value": value, "replacement": replacement})
        document, doc_counts = _redact_data(document, normalized_rules)
        claims_redacted, claim_counts = _redact_data(report["claims"], normalized_rules)
        review_redacted, review_counts = _redact_data(report["review"], normalized_rules)
        cvss_redacted, cvss_counts = _redact_data(report["cvss"], normalized_rules)
        dedup_redacted, dedup_counts = _redact_data(report["deduplication"], normalized_rules)
        for index, rule in enumerate(normalized_rules):
            value = rule["value"]
            total_count = (
                doc_counts.get(value, 0) + claim_counts.get(value, 0)
                + review_counts.get(value, 0) + cvss_counts.get(value, 0)
                + dedup_counts.get(value, 0)
            )
            replacements.append({
                "rule_index": index,
                "value_sha256": hashlib.sha256(value.encode("utf-8")).hexdigest(),
                "replacement": rule["replacement"],
                "occurrences": total_count,
            })
        lines = [f"# {_prose(document['title'])}", ""]
        lines.extend([
            f"- 报告 ID：`{report['report_id']}`（版本 {report['report_seq']}）",
            f"- 目标：{_prose(document['target'])}（{_prose(document['target_version'])}）",
            f"- 计划范围：{_prose(document['scope'])}",
            f"- 攻击者最低角色与前提：{_prose(document['attacker'])}",
            f"- 准备度：{_prose(report['review'].get('readiness'))}",
            "",
        ])
        for field in ("function", "mechanism", "normal_behavior", "actual_behavior", "confirmed_impact", "conditional_impact"):
            if document.get(field):
                lines.extend([f"## {_prose(mapping.get(field, field))}", "", _prose(document[field]), ""])
        lines.extend(["## 逐步复现", ""])
        for index, step in enumerate(document["steps"], 1):
            if not isinstance(step, dict):
                step = {"action": step}
            lines.append(f"### 步骤 {index}")
            for key in ("action", "identity", "object_source", "input", "expected", "actual", "evidence"):
                if step.get(key) is not None:
                    lines.append(f"- {key}：{_prose(step[key])}")
            lines.append("")
        for field in ("decisive_evidence", "cvss", "remediation", "counterevidence", "limitations", "restoration"):
            if document.get(field):
                lines.extend([f"## {_prose(mapping.get(field, field))}", "", _prose(document[field]), ""])
        poc = document.get("poc") or {}
        runnable = bool(poc) and not redactions
        if poc:
            lines.extend(["## 可运行 PoC", ""])
            for key in ("dependencies", "initialization", "success_assertion", "cleanup"):
                if poc.get(key):
                    lines.append(f"- {key}：{_prose(poc[key])}")
            verification = poc.get("verification") or {}
            lines.append(f"- 执行验证：{_prose(verification or {'status': 'not_verified'})}")
            lines.append("")
            if poc.get("content_ref"):
                try:
                    content = self._read(task_id, poc["content_ref"])
                    try:
                        text = content.decode("utf-8")
                    except UnicodeDecodeError:
                        text = ""
                        lines.append("PoC 为二进制文件，请使用包内附件，不在 Markdown 中转写。")
                    if text:
                        fence = _fence(text)
                        lines.extend([fence + str(poc.get("language") or ""), text, fence])
                    lines.append("")
                except Exception as exc:
                    if not allow_partial:
                        raise
                    issues.append({"path": "poc/content", "code": getattr(exc, "code", "artifact_missing"), "reason": str(exc)})
            if poc.get("command"):
                lines.extend(["执行命令：", "", "```sh", _prose(poc["command"]), "```", ""])
        lines.extend(["## 附件目录", ""])
        for ref in report.get("evidence_refs") or []:
            lines.append(f"- `{ref.get('name') or ref.get('artifact_id')}`：`{ref.get('sha256')}`，{ref.get('media_type')}，{ref.get('size')} bytes")
        for ref in document.get("attachments") or []:
            lines.append(f"- {_prose(ref)}")
        if report.get("template_missing"):
            lines.extend(["", "## 模板缺项", "", "、".join(report["template_missing"]), ""])
        if redactions:
            lines.extend(["", "## 派生脱敏说明", "", "本版本经过用户明确选择的脱敏；相关 PoC 可能不能直接运行。原始冻结证据未被覆盖。", ""])
        markdown = "\n".join(lines)
        for rule in normalized_rules:
            markdown = markdown.replace(str(rule["value"]), str(rule["replacement"]))
        canonical = {
            "schema": REPORT_SCHEMA, "report_id": report["report_id"],
            "report_seq": report["report_seq"], "document": document,
            "claims": claims_redacted, "review": review_redacted,
            "cvss": cvss_redacted, "deduplication": dedup_redacted,
            "source_watermarks": report["source_watermarks"],
            "template": template, "renderer_version": RENDERER_VERSION,
            "derived_from": report["digest"], "redactions": replacements,
        }
        return {
            "markdown": markdown, "canonical": canonical, "issues": issues,
            "runnable": runnable, "redactions": replacements,
        }


class SrcSubmissionService:
    def __init__(self, reports: SrcReportService, artifacts: SrcArtifactStore) -> None:
        self.reports = reports
        self.artifacts = artifacts
        self.renderer = SubmissionRenderer(reports, artifacts)

    def render(
        self, task_id: str, report_id: str, *,
        redactions: Optional[list[dict[str, Any]]] = None,
        allow_partial: bool = False,
    ) -> dict[str, Any]:
        return self.renderer.render(
            self.reports.get(task_id, report_id),
            redactions=redactions, allow_partial=allow_partial,
        )

    def export(
        self, task_id: str, report_id: str, *, format: str,
        allow_partial: bool = False, redactions: Optional[list[dict[str, Any]]] = None,
        permission_checker: Optional[Callable[[dict[str, Any]], None]] = None,
    ) -> dict[str, Any]:
        report = self.reports.get(task_id, report_id)
        if permission_checker is not None:
            permission_checker(report)
        if format == "zip":
            return self._zip(report, allow_partial=allow_partial, redactions=redactions)
        if format == "raw":
            content = self._raw(report, redactions or [])
            return self._result(content, "message/http; charset=utf-8", f"src-{report_id}.http", {"derived_http1": True, "redactions": redactions or []})
        if format == "har":
            content = _json(self._har(report, redactions or []))
            return self._result(content, "application/json", f"src-{report_id}.har", {"wire_bytes_complete": False, "redactions": redactions or []})
        if format == "jsonl":
            content = self._jsonl(report, redactions or [])
            return self._result(content, "application/x-ndjson", f"src-{report_id}.jsonl", {"complete_objects": True, "redactions": redactions or []})
        raise SrcStoreError("version_conflict", f"unsupported export format {format!r}")

    @staticmethod
    def _result(content: bytes, media_type: str, filename: str, detail: dict[str, Any]) -> dict[str, Any]:
        return {
            "bytes": content, "media_type": media_type, "filename": filename,
            "sha256": hashlib.sha256(content).hexdigest(), "size": len(content), **detail,
        }

    def _exchanges(self, report: dict[str, Any]) -> list[dict[str, Any]]:
        exchanges = list(report["document"].get("exchanges") or [])
        if not exchanges:
            raise SrcStoreError("artifact_missing", "report has no selected HTTP exchanges")
        return exchanges

    def _raw(self, report: dict[str, Any], redactions: list[dict[str, Any]]) -> bytes:
        chunks = []
        for exchange in self._exchanges(report):
            if exchange.get("request_raw_ref"):
                chunks.append(self.artifacts.read(artifact_ref(exchange["request_raw_ref"]), expected_task_id=report["task_id"]))
            if exchange.get("response_raw_ref"):
                chunks.append(self.artifacts.read(artifact_ref(exchange["response_raw_ref"]), expected_task_id=report["task_id"]))
        content, counts = _redact_bytes(b"\r\n\r\n".join(chunks), redactions)
        for value, count in counts.items():
            if count <= 0:
                raise SrcStoreError("version_conflict", f"redaction value was not found in raw HTTP: {value}")
        return content

    def _har(self, report: dict[str, Any], redactions: list[dict[str, Any]]) -> dict[str, Any]:
        entries = []
        counts = {str(rule["value"]): 0 for rule in redactions}
        for exchange in self._exchanges(report):
            task_id = report["task_id"]
            request_raw = self.artifacts.read(artifact_ref(exchange["request_raw_ref"]), expected_task_id=task_id) if exchange.get("request_raw_ref") else b""
            response_raw = self.artifacts.read(artifact_ref(exchange["response_raw_ref"]), expected_task_id=task_id) if exchange.get("response_raw_ref") else b""
            request_raw, request_counts = _redact_bytes(request_raw, redactions)
            response_raw, response_counts = _redact_bytes(response_raw, redactions)
            request_body = request_raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in request_raw else request_raw
            response_body = response_raw.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in response_raw else response_raw
            for counts_part in (request_counts, response_counts):
                for key, count in counts_part.items():
                    counts[key] += count
            entries.append({
                "startedDateTime": exchange.get("started_at"),
                "time": exchange.get("duration_ms", 0),
                "request": {
                    "method": exchange["method"], "url": exchange["observed_url"],
                    "httpVersion": exchange.get("protocol", "http/1.1"),
                    "headers": [{"name": pair[0], "value": pair[1]} for pair in exchange.get("request_headers") or []],
                    "queryString": list(exchange.get("query") or []),
                    "cookies": [], "headersSize": -1, "bodySize": len(request_body),
                    "postData": {"mimeType": "application/octet-stream", "text": base64.b64encode(request_body).decode(), "encoding": "base64"},
                },
                "response": {
                    "status": exchange.get("status_code") or 0, "statusText": "",
                    "httpVersion": exchange.get("protocol", "http/1.1"),
                    "headers": [{"name": pair[0], "value": pair[1]} for pair in exchange.get("response_headers") or []],
                    "cookies": [], "redirectURL": "", "headersSize": -1, "bodySize": len(response_body),
                    "content": {"size": len(response_body), "mimeType": "application/octet-stream", "text": base64.b64encode(response_body).decode(), "encoding": "base64"},
                },
                "cache": {}, "timings": {"send": 0, "wait": exchange.get("duration_ms", 0), "receive": 0},
                "comment": "HAR is an interchange projection. Refer to the original Artifact for wire-byte guarantees.",
                "_muteki": {
                    "exchange_id": exchange["exchange_id"], "capture_state": exchange.get("capture_state"),
                    "request_sha256": artifact_ref(exchange["request_raw_ref"]).sha256,
                    "response_sha256": artifact_ref(exchange["response_raw_ref"]).sha256 if exchange.get("response_raw_ref") else None,
                    "request_raw_base64": base64.b64encode(request_raw).decode(),
                    "response_raw_base64": base64.b64encode(response_raw).decode() if response_raw else None,
                    "derived_redaction": bool(redactions),
                },
            })
        result = {"log": {"version": "1.2", "creator": {"name": "Muteki SRC", "version": RENDERER_VERSION}, "entries": entries}}
        result, data_counts = _redact_data(result, redactions)
        for key, count in data_counts.items():
            counts[key] += count
        for value, count in counts.items():
            if count <= 0:
                raise SrcStoreError("version_conflict", f"redaction value was not found in HAR: {value}")
        return result

    def _jsonl(self, report: dict[str, Any], redactions: list[dict[str, Any]]) -> bytes:
        rows = [{"type": "frozen_report", "object": report}]
        rows.extend({"type": "claim", "report_id": report["report_id"], "object": claim} for claim in report["claims"])
        rows.extend({"type": "http_exchange", "report_id": report["report_id"], "object": item} for item in report["document"].get("exchanges") or [])
        rows.extend({"type": "artifact", "report_id": report["report_id"], "object": ref} for ref in report["evidence_refs"])
        rows, _counts = _redact_data(rows, redactions)
        return b"".join(json.dumps(row, ensure_ascii=False, separators=(",", ":")).encode("utf-8") + b"\n" for row in rows)

    def _zip(self, report: dict[str, Any], *, allow_partial: bool, redactions: Optional[list[dict[str, Any]]]) -> dict[str, Any]:
        task_id = report["task_id"]
        issues: list[dict[str, Any]] = []
        try:
            rendered = self.renderer.render(report, redactions=redactions, allow_partial=allow_partial)
        except Exception as exc:
            if not allow_partial:
                raise
            rendered = self.renderer.render(report, redactions=redactions, allow_partial=True)
        issues.extend(rendered["issues"])
        files: dict[str, dict[str, Any]] = {}

        def add(path: str, content: bytes, *, media_type: str, classification: str, source_version: str, purpose: str) -> None:
            pure = PurePosixPath(path)
            if pure.is_absolute() or ".." in pure.parts or not path:
                raise SrcStoreError("version_conflict", "unsafe ZIP destination")
            if path in files and files[path]["sha256"] != hashlib.sha256(content).hexdigest():
                raise SrcStoreError("artifact_hash_mismatch", f"ZIP path digest collision: {path}")
            files[path] = {
                "content": content, "path": path, "media_type": media_type,
                "classification": classification, "size": len(content),
                "sha256": hashlib.sha256(content).hexdigest(),
                "source_version": source_version, "purpose": purpose,
            }

        add("report.md", rendered["markdown"].encode("utf-8"), media_type="text/markdown", classification="derived", source_version=report["digest"], purpose="readable submission")
        add("report.json", _json(rendered["canonical"]), media_type="application/json", classification="derived", source_version=report["digest"], purpose="canonical submission")
        reviewed_images = {
            str(row.payload.get("sha256") or "")
            for row in self.reports.store.all(task_id, "image_review")
            if row.payload.get("status") == "reviewed" and row.payload.get("reviewed_by") and row.payload.get("reviewed_at")
        }
        normalized_rules = [
            {
                "value": str(rule.get("value") or ""),
                "replacement": str(rule.get("replacement") or "[已脱敏]"),
            }
            for rule in (redactions or [])
        ]
        redaction_counts = {
            rule["value"]: int(rendered["redactions"][idx].get("occurrences") or 0)
            for idx, rule in enumerate(normalized_rules)
        }
        all_evidence_refs = list(report.get("evidence_refs") or [])
        seen_artifact_hashes = {artifact_ref(ref).sha256 for ref in all_evidence_refs}
        for exchange in report["document"].get("exchanges") or []:
            for ref_key in ("request_raw_ref", "response_raw_ref"):
                raw_ref = exchange.get(ref_key)
                if isinstance(raw_ref, dict) and raw_ref.get("sha256") and raw_ref["sha256"] not in seen_artifact_hashes:
                    all_evidence_refs.append(raw_ref)
                    seen_artifact_hashes.add(raw_ref["sha256"])
        for ref in all_evidence_refs:
            artifact = artifact_ref(ref)
            path = f"evidence/{artifact.sha256[:16]}/{_filename(artifact.name)}"
            if artifact.media_type.startswith("image/") and artifact.sha256 not in reviewed_images:
                if not allow_partial:
                    raise SrcStoreError("version_conflict", f"image has not been reviewed: {artifact.sha256}")
                issues.append({"path": path, "code": "image_not_reviewed", "reason": "图片没有实际查看记录", "artifact_sha256": artifact.sha256})
                continue
            try:
                content = self.artifacts.read(artifact, expected_task_id=task_id)
                if normalized_rules:
                    for rule in normalized_rules:
                        value = rule["value"].encode()
                        redaction_counts[rule["value"]] += content.count(value)
                        content = content.replace(value, rule["replacement"].encode())
                add(path, content, media_type=artifact.media_type, classification="original" if artifact.original and not normalized_rules else "derived", source_version=str(artifact.version), purpose=artifact.name)
            except Exception as exc:
                if not allow_partial:
                    raise
                issues.append({"path": path, "code": getattr(exc, "code", "artifact_missing"), "reason": str(exc), "artifact_sha256": artifact.sha256})
        poc = rendered["canonical"]["document"].get("poc") or {}
        if poc:
            add("poc/dependencies.json", _json({
                "dependencies": poc.get("dependencies"), "initialization": poc.get("initialization"),
                "success_assertion": poc.get("success_assertion"), "cleanup": poc.get("cleanup"),
                "command": poc.get("command"), "verification": poc.get("verification") or {"status": "not_verified"},
            }), media_type="application/json", classification="derived", source_version=report["digest"], purpose="PoC execution instructions")
            if poc.get("content_ref"):
                try:
                    content = self.artifacts.read(artifact_ref(poc["content_ref"]), expected_task_id=task_id)
                    if normalized_rules:
                        for rule in normalized_rules:
                            value = rule["value"].encode()
                            redaction_counts[rule["value"]] += content.count(value)
                            content = content.replace(value, rule["replacement"].encode())
                    add(f"poc/{_filename(str(poc.get('filename') or 'poc.py'))}", content,
                        media_type="text/x-python", classification="original" if not normalized_rules else "derived",
                        source_version=report["digest"], purpose="executable PoC")
                except Exception as exc:
                    if not allow_partial:
                        raise
                    issues.append({"path": "poc/content", "code": getattr(exc, "code", "artifact_missing"), "reason": str(exc)})
        for value, count in redaction_counts.items():
            if count <= 0:
                raise SrcStoreError("version_conflict", f"redaction value was not found in the selected evidence: {value}")
        issues = list({self.reports.store.digest(issue): issue for issue in issues}.values())
        complete = not issues
        attachments = [{key: value for key, value in entry.items() if key != "content"} for entry in files.values()]
        manifest = {
            "schema": "muteki.src.evidence-bundle.v1", "report_id": report["report_id"],
            "report_digest": report["digest"], "renderer_version": RENDERER_VERSION,
            "template": report["template"], "source_watermarks": report["source_watermarks"],
            "complete": complete, "status": "complete" if complete else "partial",
            "expected_entry_count": len(attachments) + len(issues),
            "actual_entry_count": len(attachments), "omissions": issues,
            "redactions": rendered["redactions"], "poc_runnable": rendered["runnable"] and complete,
            "poc": {"command": poc.get("command"), "verification": poc.get("verification") or {"status": "not_verified"}},
            "attachments": attachments,
        }
        output = io.BytesIO()
        with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for path, entry in sorted(files.items()):
                archive.writestr(path, entry["content"])
            archive.writestr("manifest.json", _json(manifest))
        result = self._result(output.getvalue(), "application/zip", f"src-{report['report_id']}-evidence.zip", {
            "complete": complete, "manifest": manifest,
        })
        result["manifest"] = manifest
        return result


def verify_bundle(data: bytes, *, require_complete: bool = True, open_images: bool = True) -> dict[str, Any]:
    checked = 0
    images = 0
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        names = [info.filename for info in archive.infolist()]
        if len(names) != len(set(names)):
            raise SrcStoreError("version_conflict", "duplicate entries in evidence bundle")
        manifest = json.loads(archive.read("manifest.json"))
        if require_complete and not manifest.get("complete"):
            raise SrcStoreError("capture_partial", "evidence bundle is partial", detail={"omissions": manifest.get("omissions")})
        if manifest.get("actual_entry_count") != len(manifest.get("attachments") or []):
            raise SrcStoreError("artifact_hash_mismatch", "manifest actual count does not match attachments")
        if manifest.get("expected_entry_count") != len(manifest.get("attachments") or []) + len(manifest.get("omissions") or []):
            raise SrcStoreError("artifact_hash_mismatch", "manifest expected count is inconsistent")
        declared_paths = {entry["path"] for entry in manifest.get("attachments") or []} | {"manifest.json"}
        if set(names) != declared_paths:
            raise SrcStoreError("artifact_hash_mismatch", "bundle contains undeclared or missing entries")
        for entry in manifest["attachments"]:
            path = entry["path"]
            pure = PurePosixPath(path)
            if pure.is_absolute() or ".." in pure.parts:
                raise SrcStoreError("version_conflict", "unsafe path in evidence bundle")
            content = archive.read(path)
            if len(content) != entry["size"] or hashlib.sha256(content).hexdigest() != entry["sha256"]:
                raise SrcStoreError("artifact_hash_mismatch", f"bundle entry hash mismatch: {path}")
            checked += 1
            if open_images and entry["media_type"].startswith("image/"):
                from PIL import Image
                with Image.open(io.BytesIO(content)) as image:
                    image.verify()
                images += 1
    return {"checked_entries": checked, "checked_images": images, "manifest": manifest}


def _substitute_poc_arg(part: str, *, python: str, poc_dir: str, report_dir: str) -> str:
    return (
        str(part)
        .replace("{python}", python)
        .replace("{poc_dir}", poc_dir)
        .replace("{report_dir}", report_dir)
    )


def execute_packaged_poc(data: bytes, *, timeout: float = 30.0) -> dict[str, Any]:
    verification = verify_bundle(data)
    manifest = verification["manifest"]
    poc = manifest.get("poc") or {}
    with tempfile.TemporaryDirectory(prefix="muteki-src-poc-") as temp:
        root = Path(temp)
        with zipfile.ZipFile(io.BytesIO(data)) as archive:
            for entry in manifest["attachments"]:
                target = root / entry["path"]
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(archive.read(entry["path"]))
        dependencies = json.loads((root / "poc/dependencies.json").read_text()) if (root / "poc/dependencies.json").is_file() else {}
        command = poc.get("command") or dependencies.get("command")
        if not command:
            raise SrcStoreError("artifact_missing", "bundle has no explicit executable PoC command")
        if isinstance(command, str):
            command = [command]
        argv = [
            _substitute_poc_arg(part, python=sys.executable, poc_dir=str(root / "poc"), report_dir=str(root))
            for part in command
        ]
        try:
            completed = subprocess.run(argv, cwd=root, capture_output=True, timeout=timeout, check=False)
        except subprocess.TimeoutExpired as exc:
            raise SrcStoreError(
                "outcome_unknown", f"PoC execution exceeded {timeout} seconds",
                detail={
                    "stdout": (exc.stdout or b"").decode("utf-8", errors="replace") if isinstance(exc.stdout, bytes) else str(exc.stdout or ""),
                    "stderr": (exc.stderr or b"").decode("utf-8", errors="replace") if isinstance(exc.stderr, bytes) else str(exc.stderr or ""),
                },
            ) from exc
        assertion = dependencies.get("success_assertion") or {}
        stdout = completed.stdout.decode("utf-8", errors="replace")
        stderr = completed.stderr.decode("utf-8", errors="replace")
        success = completed.returncode == int(assertion.get("exit_code") or 0)
        if success and assertion.get("stdout_contains"):
            success = str(assertion["stdout_contains"]) in stdout
        return {
            "success": success, "exit_code": completed.returncode,
            "stdout": stdout, "stderr": stderr, "command": argv,
            "checked_entries": verification["checked_entries"],
        }


__all__ = ["SrcSubmissionService", "SubmissionRenderer", "execute_packaged_poc", "verify_bundle"]
