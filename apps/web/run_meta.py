"""Durable user-set conversation metadata — pin / archive / custom title.

These are OPERATOR preferences, not part of the event-sourced solve. Keeping them
out of the per-run JSONL (which is the immutable solve log) means the rail can be
reorganized freely without polluting the replayable history. One small JSON file
under the sessions root, loaded on startup and rewritten on every mutation (the
table is tiny — a few dozen rows at most).
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Optional


class RunMetaStore:
    def __init__(self, root: str | Path = "sessions") -> None:
        self.path = Path(root) / "_rail_meta.json"
        self._data: dict[str, dict[str, Any]] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                self._data = {k: v for k, v in raw.items() if isinstance(v, dict)}
        except (json.JSONDecodeError, OSError):
            # a corrupt meta file must never break startup — just start fresh
            self._data = {}

    def _flush(self) -> None:
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=0), encoding="utf-8")
        tmp.replace(self.path)  # atomic on POSIX

    def get(self, run_id: str) -> dict[str, Any]:
        """Meta for one run with defaults filled in (never raises / KeyErrors)."""
        m = self._data.get(run_id, {})
        return {
            "pinned": bool(m.get("pinned", False)),
            "pinned_at": m.get("pinned_at"),
            "archived": bool(m.get("archived", False)),
            # when the run was archived (epoch float). Used by the retention sweep
            # for the "archived for N days → delete" step. None when not archived.
            "archived_at": m.get("archived_at"),
            # operator-set custom title; overrides the auto/challenge name when set
            "custom_name": m.get("custom_name") or None,
            # the folder this run is filed under (None = top-level "Recent"), plus
            # an operator-set sort key within its section (drag-to-reorder).
            "folder_id": m.get("folder_id") or None,
            "order": m.get("order"),
        }

    def all(self) -> dict[str, dict[str, Any]]:
        return {rid: self.get(rid) for rid in self._data}

    def _mutate(self, run_id: str, **changes: Any) -> dict[str, Any]:
        m = dict(self._data.get(run_id, {}))
        m.update(changes)
        # drop falsy/None keys so the file stays minimal
        m = {k: v for k, v in m.items() if v not in (None, False, "")}
        if m:
            self._data[run_id] = m
        else:
            self._data.pop(run_id, None)
        self._flush()
        return self.get(run_id)

    def set_pinned(self, run_id: str, pinned: bool, *, now: float) -> dict[str, Any]:
        return self._mutate(
            run_id,
            pinned=pinned,
            pinned_at=(now if pinned else None),
        )

    def set_archived(self, run_id: str, archived: bool, *,
                     now: Optional[float] = None) -> dict[str, Any]:
        # archiving a pinned run also unpins it — an archived row never shows in
        # either the pinned or recent section, so a stale pin would be invisible.
        changes: dict[str, Any] = {"archived": archived}
        if archived:
            changes["pinned"] = False
            changes["pinned_at"] = None
            changes["archived_at"] = now  # epoch float (for the retention sweep)
        else:
            changes["archived_at"] = None
        return self._mutate(run_id, **changes)

    def set_name(self, run_id: str, name: Optional[str]) -> dict[str, Any]:
        return self._mutate(run_id, custom_name=(name or "").strip() or None)

    def set_folder(self, run_id: str, folder_id: Optional[str]) -> dict[str, Any]:
        return self._mutate(run_id, folder_id=(folder_id or "").strip() or None)

    def set_order(self, run_id: str, order: Optional[int]) -> dict[str, Any]:
        return self._mutate(run_id, order=int(order) if order is not None else None)

    def clear_folder_for_all(self, folder_id: str) -> None:
        """When a folder is deleted, unfile every run that was in it."""
        changed = False
        for rid, m in list(self._data.items()):
            if m.get("folder_id") == folder_id:
                m2 = {k: v for k, v in m.items() if k != "folder_id"}
                if m2:
                    self._data[rid] = m2
                else:
                    self._data.pop(rid, None)
                changed = True
        if changed:
            self._flush()

    def forget(self, run_id: str) -> None:
        """Drop all meta for a deleted run."""
        if run_id in self._data:
            self._data.pop(run_id, None)
            self._flush()


class FolderStore:
    """Operator-created rail folders (id → name + order). A tiny JSON side-table,
    separate from the per-run meta so folders persist independently of any run."""

    def __init__(self, root: str | Path = "sessions") -> None:
        self.path = Path(root) / "_folders.json"
        self._data: dict[str, dict[str, Any]] = {}
        self._seq = 0
        self._load()

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                self._data = {k: v for k, v in raw.items() if isinstance(v, dict)}
                for fid in self._data:
                    m = re.match(r"f(\d+)$", fid)
                    if m:
                        self._seq = max(self._seq, int(m.group(1)))
        except (json.JSONDecodeError, OSError):
            self._data = {}

    def _flush(self) -> None:
        tmp = self.path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(self._data, ensure_ascii=False, indent=2), encoding="utf-8")
        tmp.replace(self.path)

    def list(self) -> list[dict[str, Any]]:
        return [
            {"id": fid, "name": m.get("name", ""), "order": m.get("order", 0)}
            for fid, m in sorted(self._data.items(), key=lambda kv: kv[1].get("order", 0))
        ]

    def create(self, name: str) -> dict[str, Any]:
        name = (name or "").strip() or "Folder"
        self._seq += 1
        fid = f"f{self._seq:03d}"
        order = (max((m.get("order", 0) for m in self._data.values()), default=0) + 1)
        self._data[fid] = {"name": name, "order": order}
        self._flush()
        return {"id": fid, "name": name, "order": order}

    def update(self, fid: str, *, name: Optional[str] = None,
               order: Optional[int] = None) -> bool:
        m = self._data.get(fid)
        if m is None:
            return False
        if name is not None:
            m["name"] = name.strip() or m.get("name", "Folder")
        if order is not None:
            m["order"] = int(order)
        self._flush()
        return True

    def delete(self, fid: str) -> bool:
        if fid in self._data:
            self._data.pop(fid, None)
            self._flush()
            return True
        return False
