"""Workbench-facing asset operations for the SRC research workbench.

These methods back the studio-prototype asset surface on real tasks. They are
operator-only: nothing here is registered in SRC_AGENT_DOMAIN_COMMANDS, and
the record kinds are protected via AGENT_FORBIDDEN_RECORDS.
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.platform.contracts.base import new_id, utcnow

from .assets import LIFECYCLE_ACTIVE, SrcAssetService
from .store import SrcStore, SrcStoreError
import hashlib as hashlib  # noqa: E402

IMPORTANCE_LEVELS = {"core", "high", "normal", "low"}
VERDICT_STATES = {"dismissed", "undetermined"}


def _text(value: Any) -> str:
    return value.strip() if isinstance(value, str) else ""


def _require(condition: bool, code: str, message: str, detail: Optional[dict[str, Any]] = None) -> None:
    if not condition:
        raise SrcStoreError(code, message, detail=detail)


class SrcWorkbenchAssetService:
    """Server-side asset operations that have no general-domain equivalent.

    `src` is the SrcService (same shape the other domain services receive).
    """

    def __init__(self, src: Any) -> None:
        self.src = src
        self.store = src.store
        self.assets = SrcAssetService(src)

    # -- importance ----------------------------------------------------------

    def set_importance(self, task_id: str, asset_ids: list[str], importance: str, *, actor_id: str) -> dict[str, Any]:
        if importance not in IMPORTANCE_LEVELS:
            raise SrcStoreError("asset_importance_invalid", f"importance must be one of {sorted(IMPORTANCE_LEVELS)}")
        if not asset_ids:
            raise SrcStoreError("asset_missing", "no asset_ids given")
        updated = []
        for asset_id in asset_ids:
            record = self.store.require(task_id, "asset", asset_id)
            if record.payload.get("lifecycle") in {"merged", "destroyed"}:
                raise SrcStoreError("artifact_missing", f"asset {asset_id} is no longer editable")
            payload = {**record.payload, "importance": importance, "importance_by": actor_id,
                       "importance_at": utcnow().isoformat()}
            updated.append(self.store.put(
                task_id, "asset", payload, record_id=asset_id,
                expected_revision=record.revision, natural_key=record.natural_key,
            ).payload)
        return {"updated": [row["asset_id"] for row in updated], "importance": importance}

    # -- workbench object links (task / run / report refs) -------------------

    def _link_key(self, ref_kind: str, ref_id: str) -> str:
        return f"{ref_kind}:{ref_id}"

    def batch_link(self, task_id: str, ref_kind: str, ref_id: str, asset_ids: list[str], *, actor_id: str) -> dict[str, Any]:
        ref_kind, ref_id = _text(ref_kind), _text(ref_id)
        if ref_kind not in {"task", "run", "report"}:
            raise SrcStoreError("workbench_link_invalid", "ref_kind must be task, run or report")
        if not ref_id:
            raise SrcStoreError("workbench_link_invalid", "ref_id is required")
        if not asset_ids:
            raise SrcStoreError("asset_missing", "no asset_ids given")
        for asset_id in asset_ids:
            self.assets._asset(task_id, asset_id)
        key = self._link_key(ref_kind, ref_id)
        record = self.store.by_natural_key(task_id, "workbench_link", key)
        if record is None:
            payload = {
                "link_id": new_id("wblink"), "task_id": task_id,
                "ref_kind": ref_kind, "ref_id": ref_id,
                "asset_ids": list(dict.fromkeys(asset_ids)),
                "created_by": actor_id, "created_at": utcnow().isoformat(),
            }
            self.store.put(task_id, "workbench_link", payload, record_id=payload["link_id"], natural_key=key)
            return payload
        payload = dict(record.payload)
        payload["asset_ids"] = list(dict.fromkeys([*payload.get("asset_ids", []), *asset_ids]))
        payload["updated_by"] = actor_id
        payload["updated_at"] = utcnow().isoformat()
        self.store.put(task_id, "workbench_link", payload, record_id=record.record_id,
                       expected_revision=record.revision, natural_key=key)
        return payload

    def links(self, task_id: str) -> dict[str, Any]:
        return {"items": [row.payload for row in self.store.all(task_id, "workbench_link")]}

    # -- duplicate verdicts (不是重复 / 无法判断) -----------------------------

    def duplicate_verdict(self, task_id: str, key: str, kind: str, ids: list[str], state: str, *,
                          actor_id: str, reason: str = "", missing_note: str = "") -> dict[str, Any]:
        key = _text(key)
        if not key or not ids:
            raise SrcStoreError("duplicate_verdict_invalid", "verdict key and member ids are required")
        if state not in VERDICT_STATES and state != "clear":
            raise SrcStoreError("duplicate_verdict_invalid", f"state must be one of {sorted(VERDICT_STATES)} or clear")
        record = self.store.by_natural_key(task_id, "duplicate_verdict", key)
        if state == "clear":
            if record is None:
                return {"key": key, "state": "cleared"}
            payload = {**record.payload, "state": "cleared", "cleared_by": actor_id,
                       "cleared_at": utcnow().isoformat()}
            self.store.put(task_id, "duplicate_verdict", payload, record_id=record.record_id,
                           expected_revision=record.revision, natural_key=key)
            return payload
        payload = {
            "verdict_id": record.record_id if record else new_id("sdupverdict"),
            "task_id": task_id, "key": key, "kind": kind, "ids": list(ids),
            "state": state, "reason": _text(reason), "missing_note": _text(missing_note),
            "decided_by": actor_id, "decided_at": utcnow().isoformat(),
        }
        self.store.put(task_id, "duplicate_verdict", payload, record_id=payload["verdict_id"],
                       expected_revision=record.revision if record else None, natural_key=key)
        return payload

    def verdicts(self, task_id: str) -> dict[str, Any]:
        return {"items": [row.payload for row in self.store.all(task_id, "duplicate_verdict")
                          if row.payload.get("state") in VERDICT_STATES]}

    def duplicates(self, task_id: str) -> dict[str, Any]:
        """Candidate duplicate groups: same asset type sharing one identity value.

        The natural key is unique by construction, so candidates group by the
        first scalar identity value (e.g. domain name, url, address) per type.
        """
        buckets: dict[str, list[dict[str, Any]]] = {}
        for record in self.store.all(task_id, "asset"):
            asset = record.payload
            if asset.get("lifecycle") not in {None, LIFECYCLE_ACTIVE}:
                continue
            identity = asset.get("identity") or {}
            primary = next((str(v).strip().lower() for v in identity.values()
                            if isinstance(v, str) and v.strip()), "")
            if not primary:
                continue
            key = f"{asset.get('type')}:{primary}"
            buckets.setdefault(key, []).append(asset)
        groups = []
        for key, rows in buckets.items():
            ids = [row["asset_id"] for row in rows]
            if len(set(ids)) < 2:
                continue
            groups.append({
                "key": key, "asset_ids": sorted(set(ids)),
                "reasons": [f"同一资产类型下标识值「{key.split(':', 1)[1]}」出现 {len(set(ids))} 条记录"],
            })
        return {"groups": groups}

    # -- durable operation history -------------------------------------------

    def record_history(self, task_id: str, entry: dict[str, Any], *, actor_id: str) -> dict[str, Any]:
        entry_id = _text(entry.get("id")) or new_id("wbhist")
        payload = {
            "entry_id": entry_id, "task_id": task_id,
            "action": _text(entry.get("action")), "summary": _text(entry.get("summary")),
            "scope": _text(entry.get("scope")), "refs": entry.get("refs") or [],
            "undo": entry.get("undo"), "at": entry.get("at") or utcnow().isoformat(),
            "actor_id": actor_id, "reverted_at": None,
        }
        record = self.store.by_natural_key(task_id, "workbench_history", entry_id)
        if record is not None:
            return record.payload
        self.store.put(task_id, "workbench_history", payload, record_id=entry_id, natural_key=entry_id)
        return payload

    def revert_history(self, task_id: str, entry_id: str, *, actor_id: str) -> dict[str, Any]:
        record = self.store.by_natural_key(task_id, "workbench_history", entry_id)
        if record is None:
            raise SrcStoreError("artifact_missing", f"history entry {entry_id} is unavailable")
        if record.payload.get("reverted_at"):
            raise SrcStoreError("version_conflict", "history entry was already reverted")
        payload = {**record.payload, "reverted_at": utcnow().isoformat(), "reverted_by": actor_id}
        self.store.put(task_id, "workbench_history", payload, record_id=record.record_id,
                       expected_revision=record.revision, natural_key=entry_id)
        return payload

    def history(self, task_id: str, *, cursor: int = 0, limit: int = 500) -> dict[str, Any]:
        rows = sorted(self.store.all(task_id, "workbench_history"),
                      key=lambda row: row.payload.get("at") or "", reverse=True)
        start = max(0, int(cursor))
        size = max(1, min(int(limit), 1000))
        page = rows[start:start + size]
        return {"items": [row.payload for row in page], "total": len(rows),
                "next_cursor": start + len(page) if start + len(page) < len(rows) else None}

    # -- merge revert ----------------------------------------------------------

    def unmerge(self, task_id: str, source_asset_id: str, *, actor_id: str) -> dict[str, Any]:
        """Inverts SrcAssetService.merge for one member asset.

        The member keeps its original asset_id: observations recorded with
        `original_asset_id` move back, the asset_alias is removed, and the
        member's record is restored to the active lifecycle.
        """
        source_record = self.store.require(task_id, "asset", source_asset_id)
        source = source_record.payload
        if source.get("lifecycle") != "merged":
            raise SrcStoreError("version_conflict", "asset is not in the merged lifecycle state")
        target_asset_id = source.get("merged_into")
        if not target_asset_id:
            raise SrcStoreError("artifact_missing", "merged asset has no merge target")
        with self.store.platform.transaction():
            for observation in self.store.all(task_id, "asset_observation"):
                original = observation.payload.get("original_asset_id")
                if observation.payload.get("asset_id") == target_asset_id and original == source_asset_id:
                    payload = dict(observation.payload)
                    payload["asset_id"] = source_asset_id
                    payload["unmerged_from"] = target_asset_id
                    payload.pop("original_asset_id", None)
                    self.store.put(task_id, "asset_observation", payload, record_id=observation.record_id,
                                   expected_revision=observation.revision, parent_id=source_asset_id,
                                   natural_key=observation.natural_key)
            for relation in self.store.all(task_id, "asset_relation"):
                payload = dict(relation.payload)
                if payload.get("remapped_from") != source_asset_id:
                    continue
                changed = False
                for field in ("from_asset_id", "to_asset_id"):
                    if payload.get(field) == target_asset_id:
                        payload[field] = source_asset_id
                payload.pop("remapped_from", None)
                self.store.put(task_id, "asset_relation", payload, record_id=relation.record_id,
                               expected_revision=relation.revision, parent_id=payload["from_asset_id"],
                               natural_key=relation.natural_key)
            alias = self.store.get(task_id, "asset_alias", source_asset_id)
            if alias is not None:
                self.store.platform.conn.execute(
                    "DELETE FROM src_records WHERE kind='asset_alias' AND record_id=?",
                    (source_asset_id,))
            restored = dict(source)
            restored.pop("merged_into", None)
            restored.pop("merge_reason", None)
            restored.pop("merged_by", None)
            restored["lifecycle"] = LIFECYCLE_ACTIVE
            restored["unmerged_by"] = actor_id
            restored = self.assets._project(restored, self.assets._observations(task_id, source_asset_id))
            self.store.put(task_id, "asset", restored, record_id=source_asset_id,
                           expected_revision=source_record.revision,
                           natural_key=source_record.natural_key)
            target_record = self.store.require(task_id, "asset", target_asset_id)
            target = dict(target_record.payload)
            target["aliases"] = [a for a in target.get("aliases", [])
                                 if a not in source.get("aliases", []) and a != source_record.natural_key]
            target["original_identifiers"] = [
                item for item in target.get("original_identifiers", [])
                if item not in source.get("original_identifiers", [])
            ]
            target = self.assets._project(target, self.assets._observations(task_id, target_asset_id))
            self.store.put(task_id, "asset", target, record_id=target_asset_id,
                           expected_revision=target_record.revision, natural_key=target_record.natural_key)
        return {"source": restored, "target": target}

    # -- import batch revert ----------------------------------------------------

    def batch_revert(self, task_id: str, batch_id: str, *, actor_id: str) -> dict[str, Any]:
        """Reverses one import batch atomically: removes the observations it
        created, re-projects touched assets, destroys assets that were created
        solely by this batch, and records an audit entry."""
        batch_record = self.store.require(task_id, "import_batch", batch_id)
        batch = batch_record.payload
        if batch.get("reverted_at"):
            return {"batch_id": batch_id, "removed_observations": 0, "removed_assets": 0,
                    "already_reverted": True}
        removed_obs = 0
        removed_assets = 0
        touched: set[str] = set()
        with self.store.platform.transaction():
            for observation in self.store.all(task_id, "asset_observation"):
                if observation.payload.get("source_kind") != batch.get("source_kind"):
                    continue
                if observation.payload.get("source_id") != batch.get("source_id"):
                    continue
                if not str(observation.payload.get("source_event_id") or "").startswith(f"{batch_id}:"):
                    continue
                touched.add(observation.payload.get("asset_id") or "")
                self.store.platform.conn.execute(
                    "DELETE FROM src_records WHERE kind='asset_observation' AND record_id=?",
                    (observation.record_id,))
                removed_obs += 1
            for asset_id in touched:
                if not asset_id:
                    continue
                asset_record = self.store.get(task_id, "asset", asset_id)
                if asset_record is None:
                    continue
                remaining = self.assets._observations(task_id, asset_id)
                if not remaining and asset_record.payload.get("lifecycle") == LIFECYCLE_ACTIVE:
                    self.store.platform.conn.execute(
                        "DELETE FROM src_records WHERE kind='asset' AND record_id=?", (asset_id,))
                    tombstone = {
                        "asset_id": asset_id, "type": asset_record.payload.get("type"),
                        "natural_key": asset_record.natural_key, "lifecycle": "destroyed",
                        "destroyed_by": actor_id, "destroyed_at": utcnow().isoformat(),
                        "identity": asset_record.payload.get("identity"),
                        "destroy_reason": f"import batch {batch_id} reverted",
                    }
                    self.store.put(task_id, "asset_tombstone", tombstone, record_id=asset_id,
                                   natural_key=asset_record.natural_key)
                    removed_assets += 1
                else:
                    projected = self.assets._project(dict(asset_record.payload), remaining)
                    self.store.put(task_id, "asset", projected, record_id=asset_id,
                                   expected_revision=asset_record.revision,
                                   natural_key=asset_record.natural_key)
            updated = {**batch, "status": "reverted", "reverted_at": utcnow().isoformat(),
                       "reverted_by": actor_id,
                       "revert": {"removed_observations": removed_obs, "removed_assets": removed_assets}}
            self.store.put(task_id, "import_batch", updated, record_id=batch_id,
                           expected_revision=batch_record.revision)
            self.store.put(task_id, "import_batch_revert", {
                "batch_id": batch_id, "reverted_by": actor_id, "reverted_at": updated["reverted_at"],
                "removed_observations": removed_obs, "removed_assets": removed_assets,
            }, parent_id=batch_id)
        return {"batch_id": batch_id, "removed_observations": removed_obs,
                "removed_assets": removed_assets}

    # -- immutable asset versions -----------------------------------------------

    def pin_version(self, task_id: str, asset_id: str, *, actor_id: str, label: str = "") -> dict[str, Any]:
        asset = self.assets._asset(task_id, asset_id)
        versions = [row for row in self.store.all(task_id, "asset_version")
                    if row.payload.get("asset_id") == asset_id]
        version = len(versions) + 1
        payload = {
            "version_id": new_id("sver"), "task_id": task_id, "asset_id": asset_id,
            "version": version, "label": _text(label) or f"v{version}",
            "snapshot": asset, "pinned_by": actor_id, "pinned_at": utcnow().isoformat(),
        }
        self.store.put(task_id, "asset_version", payload, record_id=payload["version_id"],
                       parent_id=asset_id, natural_key=f"{asset_id}:{version}")
        return payload

    def versions(self, task_id: str, asset_id: str) -> dict[str, Any]:
        rows = [row.payload for row in self.store.all(task_id, "asset_version")
                if row.payload.get("asset_id") == asset_id]
        rows.sort(key=lambda row: row.get("version") or 0)
        return {"items": rows}

    # -- task link state ---------------------------------------------------------

    def task_link(self, task_id: str, asset_id: str, state: str, *, actor_id: str) -> dict[str, Any]:
        if state not in {"attached", "detached"}:
            raise SrcStoreError("workbench_link_invalid", "state must be attached or detached")
        self.assets._asset(task_id, asset_id)
        link = self.store.by_natural_key(task_id, "task_asset_link", asset_id)
        payload = dict(link.payload if link else {"asset_id": asset_id, "task_id": task_id})
        payload.update({"state": state, "actor_id": actor_id, "updated_at": utcnow().isoformat()})
        self.store.put(task_id, "task_asset_link", payload, record_id=asset_id, natural_key=asset_id,
                       expected_revision=link.revision if link else None)
        return payload

    def task_links(self, task_id: str) -> dict[str, Any]:
        return {"items": [row.payload for row in self.store.all(task_id, "task_asset_link")]}

    # -- bulk asset detail ---------------------------------------------------------

    def bulk(self, task_id: str, *, cursor: int = 0, limit: int = 200) -> dict[str, Any]:
        """Full per-asset detail page for the workbench loader."""
        rows = [row for row in self.store.all(task_id, "asset")
                if row.payload.get("lifecycle") in {None, LIFECYCLE_ACTIVE, "archived"}]
        rows.sort(key=lambda row: row.record_id)
        start = max(0, int(cursor))
        size = max(1, min(int(limit), 500))
        page = rows[start:start + size]
        items = []
        for record in page:
            asset_id = record.record_id
            detail = self.assets.detail(task_id, asset_id)
            items.append({**detail["asset"], "observations": detail["observations"],
                          "relations": detail["relations"], "sources": detail["sources"],
                          "_record_revision": record.revision})
        total = len(rows)
        return {"items": items, "total": total,
                "next_cursor": start + len(page) if start + len(page) < total else None,
                "complete": start + len(page) >= total}

    # --- findings workbench: merges, stale flags, evidence verify, meta ---

    def merge_records(self, task_id: str, kind: str, primary_id: str, member_ids: Iterable[str], reason: str = "", *, actor_id: Optional[str] = None) -> dict[str, Any]:
        """Durable workbench merge for kinds without a native merge lifecycle (e.g. findings)."""
        self.src.binding(task_id)
        _require(kind in {"source", "clue"}, "merge_kind_invalid", "merge kind must be source or clue")
        merge_id = new_id("wmerge")
        payload = {
            "merge_id": merge_id, "kind": kind, "primary_id": primary_id,
            "member_ids": [str(m) for m in member_ids], "reason": str(reason or ""),
            "merged_by": actor_id, "merged_at": utcnow(), "undone_at": None, "undone_by": None,
        }
        self.store.put(task_id, "workbench_merge", payload, record_id=merge_id)
        return payload

    def unmerge_records(self, task_id: str, merge_id: str, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        record = self.store.require(task_id, "workbench_merge", merge_id)
        if record.payload.get("undone_at"):
            return {"merge_id": merge_id, "already_undone": True}
        payload = {**record.payload, "undone_at": utcnow(), "undone_by": actor_id}
        self.store.put(task_id, "workbench_merge", payload, record_id=merge_id, expected_revision=record.revision)
        return {"merge_id": merge_id, "undone_at": payload["undone_at"]}

    def merges(self, task_id: str, kind: str = "") -> dict[str, Any]:
        self.src.binding(task_id)
        return {"items": [r.payload for r in self.store.all(task_id, "workbench_merge") if not kind or r.payload.get("kind") == kind]}

    def save_finding_meta(self, task_id: str, finding_id: str, meta: dict[str, Any], *, actor_id: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        record_id = f"{finding_id}:meta"
        existing = self.store.get(task_id, "workbench_finding", record_id)
        payload = {**(existing.payload if existing else {}), **{k: v for k, v in meta.items() if v is not None},
                   "finding_id": finding_id, "updated_by": actor_id, "updated_at": utcnow()}
        self.store.put(task_id, "workbench_finding", payload,
                       record_id=record_id, natural_key=finding_id, expected_revision=(existing.revision if existing else None))
        return payload

    def finding_meta(self, task_id: str) -> dict[str, Any]:
        self.src.binding(task_id)
        return {"items": [r.payload for r in self.store.all(task_id, "workbench_finding")]}

    def finding_stale(self, task_id: str, finding_id: str, reason: str, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        """Flag a finding stale with a real dependency digest (claims + reason)."""
        self.src.binding(task_id)
        _require(bool(str(reason or "").strip()), "validation_failed", "stale reason is required")
        claims = [r.payload for r in self.store.all(task_id, "finding_claim") if r.payload.get("finding_id") == finding_id]
        digest = self.store.digest({"claims": [str(c.get("digest") or "") for c in claims], "reason": str(reason)})
        # Workbench staleness stands alone (findings._mark_stale needs a review
        # record); review_status counts these markers the same way when a
        # review exists, and the workbench reads them directly otherwise.
        marker = {
            "finding_id": finding_id, "reason": str(reason), "dependency_digest": digest,
            "source_refs": [], "marked_by": actor_id, "marked_at": utcnow(), "cleared": False,
        }
        self.store.put(task_id, "finding_stale", marker, parent_id=finding_id)
        return {"finding_id": finding_id, "stale": True, "reason": str(reason)}

    def finding_stale_clear(self, task_id: str, finding_id: str, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        cleared = 0
        with self.store.platform.transaction():
            for row in self.store.all(task_id, "finding_stale"):
                if getattr(row, "parent_id", None) != finding_id and row.payload.get("finding_id") != finding_id:
                    continue
                if row.payload.get("cleared"):
                    continue
                self.store.put(task_id, "finding_stale",
                               {**row.payload, "cleared": True, "cleared_by": actor_id, "cleared_at": utcnow()},
                               record_id=row.record_id, expected_revision=row.revision, parent_id=getattr(row, "parent_id", "") or "")
                cleared += 1
        return {"finding_id": finding_id, "cleared": cleared}

    def verify_artifact(self, task_id: str, ref_data: dict[str, Any], *, evidence_id: str = "", actor_id: Optional[str] = None) -> dict[str, Any]:
        """Server-side digest check: read the artifact and compare sha256."""
        self.src.binding(task_id)
        query = self.src.domain("query")
        try:
            content = query.read_artifact(task_id, ref_data)
        except SrcStoreError as exc:
            return {"state": "unreadable", "code": exc.code, "reason": exc.message}
        actual = hashlib.sha256(content).hexdigest()
        expected = str(ref_data.get("sha256") or "")
        state = "verified" if expected == actual else "mismatch"
        result = {"state": state, "expected": expected, "actual": actual, "size": len(content)}
        evidence_id = evidence_id or str(ref_data.get("evidence_id") or ref_data.get("artifact_id") or expected[:16])
        self.store.put(task_id, "workbench_evidence", {
            "evidence_id": evidence_id, "artifact_sha256": expected, "state": state,
            "checked_by": actor_id, "checked_at": utcnow(), "size": len(content),
        }, natural_key=evidence_id)
        return result

    def bulk_link(self, task_id: str, ref_kind: str, ref_id: str, object_ids: Iterable[str], *, object_kind: str = "asset", actor_id: Optional[str] = None) -> dict[str, Any]:
        """Generic workbench link: N objects -> one workbench object (task/run/report/finding)."""
        self.src.binding(task_id)
        _require(ref_kind in {"task", "run", "report", "finding"}, "link_ref_invalid", "unsupported link target")
        ids = [str(i) for i in object_ids]
        _require(bool(ids), "link_targets_required", "link needs at least one object")
        record_id = f"{ref_kind}:{ref_id}:{object_kind}"
        existing = self.store.get(task_id, "workbench_link", record_id)
        merged_ids = sorted(set((existing.payload.get("object_ids") if existing else []) or []) | set(ids))
        self.store.put(task_id, "workbench_link", {
            "ref_kind": ref_kind, "ref_id": ref_id, "object_kind": object_kind,
            "object_ids": merged_ids, "updated_by": actor_id, "updated_at": utcnow(),
        }, record_id=record_id, natural_key=record_id, expected_revision=(existing.revision if existing else None))
        return {"ref_kind": ref_kind, "ref_id": ref_id, "object_kind": object_kind, "object_ids": merged_ids}

    # --- tasks workbench: durable TODOs, question status, scope/approvals ---

    def save_todo(self, task_id: str, todo_id: str = "", payload: Optional[dict[str, Any]] = None, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        """Durable follow-up TODO for the workbench tasks workspace."""
        self.src.binding(task_id)
        data = dict(payload or {})
        todo_id = todo_id or str(data.get("todo_id") or "") or new_id("wtodo")
        existing = self.store.get(task_id, "workbench_todo", todo_id)
        merged = {**(existing.payload if existing else {}), **data}
        _require(bool(str(merged.get("title") or "").strip()), "todo_incomplete", "todo needs a title")
        body = {**merged,
                "todo_id": todo_id, "status": data.get("status") or (existing.payload.get("status") if existing else "open"),
                "updated_by": actor_id, "updated_at": utcnow()}
        self.store.put(task_id, "workbench_todo", body,
                       record_id=todo_id, natural_key=todo_id,
                       expected_revision=(existing.revision if existing else None))
        return body

    def todos(self, task_id: str) -> dict[str, Any]:
        self.src.binding(task_id)
        return {"items": [r.payload for r in self.store.all(task_id, "workbench_todo")]}

    def question_status(self, task_id: str, question_id: str, status: str, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        """Update a research permission question's status with a real revision bump."""
        self.src.binding(task_id)
        rows = [r for r in self.store.all(task_id, "research_question")
                if r.parent_id == question_id or r.record_id == question_id or r.payload.get("question_id") == question_id]
        _require(bool(rows), "artifact_missing", "research question not found")
        latest = max(rows, key=lambda r: int(r.payload.get("version") or r.revision))
        body = {**latest.payload, "status": status, "status_updated_by": actor_id, "status_updated_at": utcnow()}
        self.store.put(task_id, "research_question", body,
                       record_id=latest.record_id, parent_id=latest.parent_id,
                       natural_key=latest.natural_key,
                       expected_revision=latest.revision)
        return body

    # --- reports workbench: drafts, structured diff ---

    def save_report_draft(self, task_id: str, draft_id: str = "", payload: Optional[dict[str, Any]] = None,
                          base_revision: int = 0, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        """Server-persisted report draft; base_revision == existing revision or version_conflict."""
        self.src.binding(task_id)
        data = dict(payload or {})
        draft_id = draft_id or str(data.get("draft_id") or "") or new_id("wrptd")
        existing = self.store.get(task_id, "workbench_report_draft", draft_id)
        expected = base_revision if existing else None
        if existing and base_revision != existing.revision:
            raise SrcStoreError("version_conflict", "report draft revision does not match",
                                detail={"expected": base_revision, "actual": existing.revision})
        body = {**(existing.payload if existing else {}), **data,
                "draft_id": draft_id, "lifecycle": "draft",
                "updated_by": actor_id, "updated_at": utcnow()}
        self.store.put(task_id, "workbench_report_draft", body,
                       record_id=draft_id, natural_key=draft_id,
                       expected_revision=expected)
        return {**body, "revision": self.store.require(task_id, "workbench_report_draft", draft_id).revision}

    def discard_report_draft(self, task_id: str, draft_id: str, base_revision: int, *, actor_id: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        record = self.store.require(task_id, "workbench_report_draft", draft_id)
        if base_revision != record.revision:
            raise SrcStoreError("version_conflict", "report draft revision does not match",
                                detail={"expected": base_revision, "actual": record.revision})
        body = {**record.payload, "lifecycle": "discarded", "discarded_by": actor_id, "discarded_at": utcnow()}
        self.store.put(task_id, "workbench_report_draft", body, record_id=draft_id,
                       expected_revision=base_revision)
        return {"draft_id": draft_id, "lifecycle": "discarded"}

    def report_drafts(self, task_id: str) -> dict[str, Any]:
        self.src.binding(task_id)
        return {"items": [{**r.payload, "revision": r.revision} for r in self.store.all(task_id, "workbench_report_draft") if r.payload.get("lifecycle") != "discarded"]}

    def report_diff(self, task_id: str, from_report_id: str, to_report_id: str) -> dict[str, Any]:
        """Structured field-level diff between two frozen report versions."""
        self.src.binding(task_id)
        reports = self.src.domain("reports")
        a = reports.get(task_id, from_report_id)
        b = reports.get(task_id, to_report_id)
        fields: list[dict[str, Any]] = []
        keys = sorted(set((a.get("document") or {}).keys()) | set((b.get("document") or {}).keys()))
        for key in keys:
            va = (a.get("document") or {}).get(key)
            vb = (b.get("document") or {}).get(key)
            fields.append({"field": key, "changed": va != vb, "before": va, "after": vb})
        meta_keys = ["template", "cvss", "deduplication", "target_version"]
        for key in meta_keys:
            va, vb = a.get(key), b.get(key)
            if va != vb:
                fields.append({"field": key, "changed": True, "before": va, "after": vb})
        return {"from": from_report_id, "to": to_report_id,
                "from_version": a.get("version"), "to_version": b.get("version"),
                "fields": fields, "changed_count": sum(1 for f in fields if f["changed"])}


# --- workbench object refs + agent suggestions --------------------------------

_SRC_REF_KINDS = {
    "source": "asset", "passage": "asset", "clue": "finding_claim",
    "task": "workbench_todo", "report": "frozen_report",
    "evidence": "http_exchange", "import": "import_batch",
    "run": "run", "change": "monitor_change", "feedback": "platform_feedback",
    "plan": "research_plan",
}


def make_src_object_resolver(platform_store, thread_id: str):
    """Build a composer src_object resolver bound to this thread's SRC task."""
    def resolve(kind: str, ref_id: str) -> dict[str, Any]:
        src_store = SrcStore(platform_store)
        task_id = ""
        with platform_store.lock:
            row = platform_store.conn.execute(
                "SELECT task_id FROM src_records WHERE kind='task_binding' AND natural_key=? LIMIT 1",
                (thread_id,),
            ).fetchone()
        if row is not None:
            task_id = str(row["task_id"] if isinstance(row, dict) else row[0])
        if not task_id:
            raise SrcStoreError("task_binding_invalid", "thread has no bound SRC task")
        return resolve_workbench_ref(src_store, task_id, kind, ref_id)
    return resolve


def resolve_workbench_ref(store, task_id: str, kind: str, ref_id: str) -> dict[str, Any]:
    """Resolve a workbench object ref to a full-content context block."""
    import json as _json
    record_kind = _SRC_REF_KINDS.get(kind)
    if not record_kind:
        raise SrcStoreError("workbench_ref_invalid", f"unsupported workbench ref kind {kind!r}")
    if kind == "clue":
        # finding ids are group keys, not claim record ids — gather the group's
        # claims plus its workbench meta + stale markers into one dossier block.
        claims = [r.payload for r in store.all(task_id, "finding_claim")
                  if r.payload.get("finding_id") == ref_id]
        meta_rows = [r.payload for r in store.all(task_id, "workbench_finding")
                     if r.payload.get("finding_id") == ref_id]
        meta = max(meta_rows, key=lambda p: int(p.get("version") or 0), default={})
        stale = [r.payload for r in store.all(task_id, "finding_stale")
                 if r.payload.get("finding_id") == ref_id and not r.payload.get("cleared")]
        if not claims and not meta:
            raise SrcStoreError("workbench_ref_stale", f"clue {ref_id} is not available in this task")
        title = str(meta.get("title") or (claims[0].get("text") if claims else ref_id))
        body = _json.dumps({"finding_id": ref_id, "meta": meta, "claims": claims, "stale_markers": stale},
                           ensure_ascii=False, indent=2)
        return {"name": title, "description": f"SRC clue · {len(claims)} 条主张", "text": body, "status": "fresh"}
    record = store.get(task_id, record_kind, ref_id)
    if record is None:
        raise SrcStoreError("workbench_ref_stale", f"{kind} {ref_id} is not available in this task")
    payload = record.payload
    name = str(payload.get("title") or payload.get("display_name") or payload.get("canonical_key") or payload.get("question") or payload.get("text") or ref_id)
    body = _json.dumps(payload, ensure_ascii=False, indent=2)
    return {"name": name, "description": f"SRC {kind} · {record_kind}", "text": body, "status": "fresh"}


_SUGGESTION_KINDS = {"classify", "new_finding", "merge", "conflict_resolve", "plan_update", "report_draft", "retest_record"}
_SUGGESTION_REF_KINDS = {"finding_id": "finding_claim", "asset_id": "asset", "report_id": "frozen_report",
                        "primary_id": "asset", "suggestion_id": "workbench_suggestion"}



class _SuggestionOps:
    # --- agent-proposed suggestions -------------------------------------------

    def propose_suggestion(self, task_id: str, *, kind: str, proposal: dict[str, Any],
                           rationale: str = "", turn_id: str = "", source_message_id: str = "",
                           evidence_refs: Optional[list] = None,
                           actor_id: Optional[str] = None) -> dict[str, Any]:
        """Agent-proposed workbench change; data only — never applied server-side."""
        self.src.binding(task_id)
        _require(kind in _SUGGESTION_KINDS, "suggestion_kind_invalid",
                 f"suggestion kind must be one of {sorted(_SUGGESTION_KINDS)}")
        proposal = dict(proposal or {})
        # validate referenced object ids against real task records
        checks: dict[str, str] = {}
        for field in ("finding_id", "asset_id", "report_id"):
            if proposal.get(field):
                checks[str(proposal[field])] = _SUGGESTION_REF_KINDS[field]
        if kind == "merge":
            for field in ("primary_id",):
                if proposal.get(field):
                    checks[str(proposal[field])] = "asset" if proposal.get("object_kind") != "clue" else "finding_claim"
            for member in proposal.get("member_ids") or []:
                checks[str(member)] = checks.get(str(proposal.get("primary_id") or ""), "asset")
        if kind == "conflict" and proposal.get("asset_id"):
            checks[str(proposal["asset_id"])] = "asset"
        for ref_id, record_kind in checks.items():
            if self.store.get(task_id, record_kind, ref_id) is None:
                raise SrcStoreError("workbench_ref_stale", f"proposal references unavailable {record_kind} {ref_id}")
        suggestion_id = new_id("wsugg")
        payload = {
            "suggestion_id": suggestion_id, "kind": kind, "proposal": proposal,
            "rationale": str(rationale or ""), "turn_id": turn_id,
            "source_message_id": source_message_id,
            "evidence_refs": list(evidence_refs or []),
            "proposed_by": actor_id, "proposed_at": utcnow().isoformat(),
            "decision": "pending", "decided_by": None, "decided_at": None, "applied_ref": None,
        }
        self.store.put(task_id, "workbench_suggestion", payload, record_id=suggestion_id, natural_key=suggestion_id)
        return payload

    def resolve_suggestion(self, task_id: str, suggestion_id: str, decision: str, *,
                         base_revision: int, applied_ref: str = "", actor_id: Optional[str] = None) -> dict[str, Any]:
        self.src.binding(task_id)
        _require(decision in {"accepted", "dismissed"}, "suggestion_decision_invalid", "decision must be accepted or dismissed")
        record = self.store.require(task_id, "workbench_suggestion", suggestion_id)
        if record.payload.get("decision") != "pending":
            return {"suggestion_id": suggestion_id, "decision": record.payload["decision"], "already_resolved": True}
        if base_revision != record.revision:
            raise SrcStoreError("version_conflict", "suggestion revision does not match",
                                detail={"expected": base_revision, "actual": record.revision})
        payload = {**record.payload, "decision": decision, "decided_by": actor_id,
                   "decided_at": utcnow().isoformat(), "applied_ref": applied_ref or None}
        self.store.put(task_id, "workbench_suggestion", payload, record_id=suggestion_id, expected_revision=base_revision)
        return payload

    def suggestions(self, task_id: str) -> dict[str, Any]:
        self.src.binding(task_id)
        return {"items": [r.payload for r in self.store.all(task_id, "workbench_suggestion") if r.payload.get("decision") == "pending"]}


SrcWorkbenchAssetService.propose_suggestion = _SuggestionOps.propose_suggestion
SrcWorkbenchAssetService.resolve_suggestion = _SuggestionOps.resolve_suggestion
SrcWorkbenchAssetService.suggestions = _SuggestionOps.suggestions
