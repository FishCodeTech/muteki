"""PoC publication and review proposals shared by CTF and Pentest Workers."""
from __future__ import annotations

import asyncio
from hashlib import sha256
import json
from pathlib import Path, PurePosixPath
from typing import Any

from muteki.swarm.graph_defs import REVIEW_FACT_MARKERS
from muteki.solver.worker_result import PocClaim
from muteki.solver.workspace import materialize_shared_artifact, workspace_root_for_worker


class PocSaveError(ValueError):
    """可修正的产物发布错误；原因必须回传给 Worker。"""

    def __init__(self, code: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code


async def _handle_poc_save(self, path_text: str, entry_command: str,
                           status: str, note: str) -> bool:
    current = (self._current_workdir
               or (Path(self._workdir) if self._workdir else None))
    cwd = Path(current).resolve() if current is not None else None
    if cwd is None:
        raise PocSaveError("poc.cwd_missing", "Worker 工作目录尚未初始化")
    root = workspace_root_for_worker(cwd)
    # shared/ 是指向本 Run 的 shared/live 的投影，解析后不在私有 cwd 内。
    # 只允许这两个数据目录，不能把整张 workspace（含其他 Worker/控制面）放行。
    shared = root / "shared" / "live"
    try:
        if getattr(self, "container", None) is not None:
            mapped = self._spill_host_path(path_text)
            if mapped is None:
                mapper = getattr(self.container, "to_container_cwd", None)
                if not callable(mapper):
                    raise ValueError("容器未提供路径映射")
                relative = PurePosixPath(path_text).relative_to(
                    PurePosixPath(mapper(str(shared))))
                if ".." in relative.parts:
                    raise ValueError("共享路径不能越界")
                mapped = (cwd, shared.joinpath(*relative.parts))
            src = Path(mapped[1]).resolve()
        else:
            supplied = Path(path_text)
            if supplied.is_absolute():
                src = supplied.resolve()
            else:
                candidate_cwd = (cwd / supplied).resolve()
                candidate_shared = (shared / supplied).resolve()
                src = (
                    candidate_cwd
                    if candidate_cwd.exists() or not candidate_shared.exists()
                    else candidate_shared
                )
        if not (src.is_relative_to(cwd) or src.is_relative_to(shared)):
            raise ValueError("路径不属于允许的产物目录")
    except (OSError, ValueError) as exc:
        detail = "PoC 路径必须位于当前 Worker 工作目录或本 Run 的 shared/live 内"
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note=detail)
        raise PocSaveError("poc.path_outside_scope", detail) from exc
    if not src.exists() or not src.is_file():
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note="POC_SAVE path is not a regular file")
        raise PocSaveError("poc.not_regular_file", "PoC 路径不存在或不是普通文件；目录请先打包为文件")

    # The PoC save does blocking filesystem + hashing work (read_text,
    # write_text, sha256-stream + possible copytree in materialize_shared_
    # artifact). This runs while the host drains a live Skill request, so doing
    # it inline would stall every other
    # worker's stream during a large PoC write (#13). Push the whole sync block
    # to a thread, exactly like the subprocess paths already do.
    def _save_blocking() -> tuple[dict, str, str]:
        status_ = (status or "available").strip().lower()
        if status_ not in {"available", "wip", "directional", "spent"}:
            status_ = "available"
        local_note = note
        save_src = src
        try:
            browser_evidence = None
            if save_src.suffix.lower() in {".png", ".jpg", ".jpeg", ".webp"}:
                sidecar = save_src.with_name(save_src.name + ".muteki-browser.json")
                if sidecar.is_file():
                    value = json.loads(sidecar.read_text(encoding="utf-8"))
                    if not isinstance(value, dict) or value.get("schema") != "muteki-browser-screenshot-v1":
                        raise ValueError("浏览器截图元数据格式无效")
                    digest = sha256()
                    with save_src.open("rb") as handle:
                        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                            digest.update(chunk)
                    if value.get("image_sha256") != digest.hexdigest():
                        raise ValueError("浏览器截图元数据与图像不匹配")
                    browser_evidence = {key: value.get(key) for key in (
                        "run_id", "worker_id", "tab", "page_url", "captured_at",
                        "request_ids", "request_scope", "image_sha256", "capture_errors",
                    )}
            art = materialize_shared_artifact(
                root, save_src, name=src.name, kind="poc", status=status_,
                metadata={
                    "entry_command": entry_command,
                    "intent_id": getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", ""),
                    "solver_id": self.solver_id,
                    **({"browser_evidence": browser_evidence} if browser_evidence else {}),
                },
            )
        except OSError as exc:
            raise PocSaveError("poc.persistence_failed", f"PoC 保存失败：{exc}") from exc
        return art, status_, local_note

    try:
        result = await asyncio.to_thread(_save_blocking)
    except PocSaveError as exc:
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note=str(exc))
        raise
    except (ValueError, TypeError) as exc:
        await self._emit_bb("poc_saved", status="rejected", path=path_text,
                            note=str(exc))
        raise PocSaveError("poc.metadata_invalid", str(exc)) from exc
    artifact, clean_status, note = result
    poc_id = f"poc-{artifact['sha256'][:12]}"
    self._last_saved_poc_id = poc_id
    marker_key = f"{src}:{artifact['sha256']}:{entry_command}:{clean_status}:{note}"
    if marker_key in self._published_pocs:
        return True
    saved_ids = dict(getattr(self, "_saved_poc_ids_by_marker", None) or {})
    saved_ids[marker_key] = poc_id
    self._saved_poc_ids_by_marker = saved_ids
    self._published_pocs.add(marker_key)
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
                note=f"direction dead-end: {reason}")
        except Exception:
            continue
        await self._emit_bb(
            "poc_concluded", poc_id=poc_id, status="spent",
            note=f"direction dead-end: {reason}")


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
                ),
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
