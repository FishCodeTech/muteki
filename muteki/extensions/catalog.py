"""扩展 catalog 来源（任务书 11.3，EXT-01）。

catalog 是一份配置出来的扩展清单：单个 ``catalog.yaml`` 或一个目录下的
多个 ``*.yaml``，每个条目描述一个可安装版本及其底层来源
（local-dir / archive / git / http，全部带校验元数据）。

catalog 只做「id + version → 来源描述」的解析，不做 fetch；
fetch 与校验在 ``installer.py``。
"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

import json

import yaml
from pydantic import Field

from muteki.platform.contracts.base import ContractModel
from muteki.extensions.trust import SignatureError, verify_ed25519

#: catalog 条目里允许的来源类型（与 installer.Source 对应）。
KNOWN_SOURCE_KINDS = frozenset({"local-dir", "archive", "git", "http"})


class CatalogError(ValueError):
    """catalog 加载 / 解析失败。"""


class CatalogSourceSpec(ContractModel):
    """catalog 条目指向的底层来源。"""

    # local-dir | archive | git | http
    kind: str = ""
    # local-dir / archive 用 path；git / http 用 url
    path: str = ""
    url: str = ""
    # git 固定 commit / tag；http / archive 的内容 sha256
    ref: str = ""
    sha256: str = ""


class CatalogEntry(ContractModel):
    """一个可安装扩展版本的 catalog 条目。"""

    id: str = ""
    version: str = ""
    description: str = ""
    source: CatalogSourceSpec = Field(default_factory=CatalogSourceSpec)
    publisher: str = ""
    published_at: str = ""
    signature_algorithm: str = ""
    signature: str = ""
    trust_status: str = "unsigned"
    revoked: bool = False
    revoke_reason: str = ""


class CatalogPublisher(ContractModel):
    id: str
    public_key: str
    algorithm: str = "ed25519"
    revoked: bool = False


class CatalogRevocation(ContractModel):
    id: str
    version: str = ""
    sha256: str = ""
    reason: str = ""


class ExtensionCatalog:
    """catalog 清单的读取与解析。"""

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root)
        self._entries: list[CatalogEntry] = []
        self._load()

    def _load(self) -> None:
        files: list[Path] = []
        if self.root.is_file():
            files = [self.root]
        elif self.root.is_dir():
            files = sorted(
                p for p in self.root.glob("*.yaml") if p.is_file()
            ) + sorted(p for p in self.root.glob("*.yml") if p.is_file())
        if not files:
            raise CatalogError(f"catalog not found: {self.root}")
        entries: list[CatalogEntry] = []
        publishers: dict[str, CatalogPublisher] = {}
        revocations: list[CatalogRevocation] = []
        for path in files:
            try:
                data = yaml.safe_load(path.read_text(encoding="utf-8"))
            except (OSError, yaml.YAMLError) as exc:
                raise CatalogError(f"cannot load catalog {path}: {exc}") from exc
            items = data.get("extensions") if isinstance(data, dict) else None
            if isinstance(data, dict):
                for raw_publisher in data.get("publishers") or []:
                    publisher = CatalogPublisher.model_validate(raw_publisher)
                    prior = publishers.get(publisher.id)
                    if prior is not None and prior != publisher:
                        raise CatalogError(
                            f"publisher {publisher.id!r} has conflicting trust records")
                    publishers[publisher.id] = publisher
                revocations.extend(
                    CatalogRevocation.model_validate(item)
                    for item in (data.get("revocations") or [])
                )
            if items is None and isinstance(data, list):
                items = data
            if not isinstance(items, list):
                raise CatalogError(
                    f"catalog {path} must be a list or a mapping with "
                    "'extensions' list"
                )
            for raw in items:
                try:
                    entry = CatalogEntry.model_validate(raw)
                except ValueError as exc:
                    raise CatalogError(
                        f"invalid catalog entry in {path}: {exc}"
                    ) from exc
                self._validate_entry(entry, path)
                entries.append(entry)
        verified = [
            self._verify_entry(entry, publishers, revocations)
            for entry in entries
        ]
        seen: set[tuple[str, str]] = set()
        for entry in verified:
            identity = (entry.id, entry.version)
            if identity in seen:
                raise CatalogError(
                    f"catalog contains duplicate immutable version "
                    f"{entry.id}@{entry.version}")
            seen.add(identity)
        self._entries = verified

    @staticmethod
    def _signed_payload(entry: CatalogEntry) -> bytes:
        payload: dict[str, Any] = {
            "id": entry.id,
            "version": entry.version,
            "publisher": entry.publisher,
            "published_at": entry.published_at,
            "signature_algorithm": entry.signature_algorithm,
            "source": entry.source.model_dump(
                mode="json", exclude={"schema_version"}),
        }
        return json.dumps(
            payload, ensure_ascii=False, sort_keys=True,
            separators=(",", ":"),
        ).encode()

    @classmethod
    def _verify_entry(
        cls,
        entry: CatalogEntry,
        publishers: dict[str, CatalogPublisher],
        revocations: list[CatalogRevocation],
    ) -> CatalogEntry:
        matching = [item for item in revocations if item.id == entry.id
                    and (not item.version or item.version == entry.version)
                    and (not item.sha256
                         or item.sha256.lower() == entry.source.sha256.lower())]
        if matching:
            return entry.model_copy(update={
                "revoked": True,
                "revoke_reason": matching[-1].reason or "catalog revocation",
                "trust_status": "revoked",
            })
        if not entry.signature:
            return entry.model_copy(update={"trust_status": "unsigned"})
        publisher = publishers.get(entry.publisher)
        if publisher is None:
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: unknown publisher "
                f"{entry.publisher!r}")
        if publisher.revoked:
            return entry.model_copy(update={
                "revoked": True,
                "revoke_reason": "publisher trust revoked",
                "trust_status": "revoked",
            })
        algorithm = entry.signature_algorithm or publisher.algorithm
        if algorithm != "ed25519" or publisher.algorithm != "ed25519":
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: unsupported "
                f"signature algorithm {algorithm!r}")
        try:
            valid = verify_ed25519(
                publisher.public_key, entry.signature,
                cls._signed_payload(entry))
        except SignatureError as exc:
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: {exc}") from exc
        if not valid:
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: signature invalid")
        return entry.model_copy(update={
            "signature_algorithm": "ed25519",
            "trust_status": "verified",
        })

    @staticmethod
    def _validate_entry(entry: CatalogEntry, path: Path) -> None:
        if not entry.id.strip() or not entry.version.strip():
            raise CatalogError(
                f"catalog entry in {path} requires non-empty id and version"
            )
        source = entry.source
        if source.kind not in KNOWN_SOURCE_KINDS:
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: unknown source "
                f"kind {source.kind!r} (known: {sorted(KNOWN_SOURCE_KINDS)})"
            )
        if source.kind in ("local-dir", "archive") and not source.path.strip():
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: "
                f"{source.kind} source requires path"
            )
        if source.kind in ("git", "http") and not source.url.strip():
            raise CatalogError(
                f"catalog entry {entry.id}@{entry.version}: "
                f"{source.kind} source requires url"
            )

    def list(self) -> list[CatalogEntry]:
        return list(self._entries)

    def resolve(
        self, extension_id: str, version: Optional[str] = None
    ) -> CatalogEntry:
        """按 id（可选 version）解析条目；version 为空取清单中最新声明。"""
        matches = [e for e in self._entries if e.id == extension_id]
        if version is not None:
            matches = [e for e in matches if e.version == version]
        if not matches:
            raise CatalogError(
                f"catalog has no entry for {extension_id!r}"
                + (f" version {version!r}" if version else "")
            )
        # 同 id 多版本时按 semver 排序取最新（catalog 声明即候选清单）
        def _key(entry: CatalogEntry) -> tuple[int, int, int]:
            parts = [int(p) for p in entry.version.split(".")[:3]]
            while len(parts) < 3:
                parts.append(0)
            return parts[0], parts[1], parts[2]

        return max(matches, key=_key)


__all__ = [
    "KNOWN_SOURCE_KINDS",
    "CatalogEntry",
    "CatalogError",
    "CatalogPublisher",
    "CatalogRevocation",
    "CatalogSourceSpec",
    "ExtensionCatalog",
]
