"""Agent Plugin 安装器：来源 fetch、校验、解包到版本目录。

安装流程：

```text
fetch → verify hash/signature metadata → unpack to versioned directory
→ validate root plugin.json / Muteki namespace / dependencies → (config validation / activate /
health check 由 registry.py 的生命周期编排完成) → mark active
```

来源（``Source``）：

- ``local-dir``：本地目录；
- ``archive``：本地归档文件（.tar.gz/.tgz/.tar/.zip），给出 sha256 时校验；
- ``git``：Git URL，必须固定 commit 或 tag（``ref``），拒绝裸分支漂移；
- ``http``：HTTP(S) 归档，``sha256`` 必填，下载后强校验；
- ``catalog``：先经 ``catalog.ExtensionCatalog`` 解析成上述底层来源。

解包拒绝路径穿越（``..``、绝对路径、symlink 逃逸）。签名基础设施未落地时
如实记录 ``signature: unsigned``，不伪造已验签状态。
"""

from __future__ import annotations

import hashlib
import json
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from muteki.extensions.catalog import ExtensionCatalog
from muteki.extensions.manifest import (
    CORE_VERSION,
    ManifestError,
    load_manifest,
    validate_manifest,
)
from muteki.extensions.ui import UIContributionError, load_ui_contributions
from muteki.platform.contracts.extensions import ExtensionManifest

#: HTTP 下载大小上限与超时（防御异常来源）。
MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024
HTTP_TIMEOUT_SECONDS = 30.0


class InstallError(RuntimeError):
    """安装流程失败；code 供统一错误 envelope 使用。"""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


@dataclass(frozen=True)
class Source:
    """一个扩展包来源描述。"""

    kind: str                       # local-dir | archive | git | http | catalog
    path: str = ""                  # local-dir / archive
    url: str = ""                   # git / http
    ref: str = ""                   # git 固定 commit / tag
    sha256: str = ""                # archive / http 的内容摘要
    # catalog 来源的解析参数
    catalog_root: str = ""
    extension_id: str = ""
    version: str = ""
    publisher: str = ""
    published_at: str = ""
    signature: str = ""
    signature_algorithm: str = ""
    trust_status: str = "unsigned"
    revoked: bool = False
    revoke_reason: str = ""


@dataclass
class InstalledPackage:
    """一次安装的结果记录。"""

    manifest: ExtensionManifest = None  # type: ignore[assignment]
    install_dir: str = ""
    source_kind: str = ""
    # fetch 阶段的校验证据：sha256 / git ref / 签名状态（未落地时为 unsigned）
    verification: dict[str, Any] = field(default_factory=dict)


def sha256_file(path: str | Path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha256_tree(path: str | Path) -> str:
    """计算扩展包内容的确定性摘要，供预览与最终安装做一致性检查。"""
    root = Path(path)
    digest = hashlib.sha256()
    for item in sorted(p for p in root.rglob("*") if p.is_file()):
        relative = item.relative_to(root).as_posix()
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        with item.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        digest.update(b"\0")
    return digest.hexdigest()


class ExtensionInstaller:
    """把 Agent Plugin 安装到 ``install_root/<plugin-name>/<version>/``。"""

    def __init__(
        self,
        install_root: str | Path,
        *,
        core_version: str = CORE_VERSION,
        capabilities: Optional[dict[str, int]] = None,
    ) -> None:
        self.install_root = Path(install_root)
        self._core_version = core_version
        self._capabilities = capabilities

    # -- 入口 -----------------------------------------------------------------

    def install(
        self, source: Source, *, overwrite: bool = False
    ) -> InstalledPackage:
        """fetch → verify → unpack → validate plugin.json，返回安装记录。"""
        resolved = self._resolve(source)
        with tempfile.TemporaryDirectory(prefix="muteki-ext-") as tmp:
            workdir = Path(tmp)
            package_dir, verification = self._fetch(resolved, workdir)
            manifest, verification = self._inspect(
                package_dir, resolved, verification)
            target = self.install_root / manifest.id / manifest.version
            if target.exists():
                if not overwrite:
                    raise InstallError(
                        "extension.already_installed",
                        f"{manifest.id}@{manifest.version} already installed "
                        f"at {target}",
                    )
                shutil.rmtree(target)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(package_dir, target)
        return InstalledPackage(
            manifest=manifest,
            install_dir=str(target),
            source_kind=resolved.kind,
            verification=verification,
        )

    def preview(self, source: Source) -> InstalledPackage:
        """完成下载、校验、解包和 plugin.json/依赖检查，但不写安装目录。"""
        resolved = self._resolve(source)
        with tempfile.TemporaryDirectory(prefix="muteki-ext-preview-") as tmp:
            package_dir, verification = self._fetch(resolved, Path(tmp))
            manifest, verification = self._inspect(
                package_dir, resolved, verification)
        return InstalledPackage(
            manifest=manifest,
            source_kind=resolved.kind,
            verification=verification,
        )

    def _inspect(
        self,
        package_dir: Path,
        source: Source,
        verification: dict[str, Any],
    ) -> tuple[ExtensionManifest, dict[str, Any]]:
        try:
            manifest = load_manifest(package_dir)
            validate_manifest(
                manifest,
                package_dir=package_dir,
                core_version=self._core_version,
                capabilities=self._capabilities,
            )
        except ManifestError as exc:
            raise InstallError(exc.error.code, exc.error.message) from exc
        if source.extension_id and manifest.id != source.extension_id:
            raise InstallError(
                "extension.catalog_identity_mismatch",
                f"catalog selected {source.extension_id!r}, but plugin.json "
                f"declares {manifest.id!r}",
            )
        if source.version and manifest.plugin_version != source.version:
            raise InstallError(
                "extension.catalog_version_mismatch",
                f"catalog selected version {source.version!r}, but plugin.json "
                f"declares {manifest.plugin_version!r}",
            )
        if manifest.ui:
            try:
                load_ui_contributions(package_dir, manifest.ui)
            except UIContributionError as exc:
                raise InstallError(exc.code, exc.message) from exc
        checked = dict(verification)
        if source.revoked:
            raise InstallError(
                "extension.source_revoked",
                f"catalog entry is revoked: {source.revoke_reason or 'no reason provided'}",
            )
        if source.trust_status == "verified":
            checked["signature"] = "verified:ed25519"
        else:
            checked.setdefault("signature", "unsigned")
        checked["trust_status"] = source.trust_status or "unsigned"
        checked["publisher"] = source.publisher or "unknown"
        checked["published_at"] = source.published_at
        checked["revoked"] = bool(source.revoked)
        checked["content_sha256"] = sha256_tree(package_dir)
        checked["agent_plugin"] = {
            "schema": manifest.plugin_schema,
            "manifest": "plugin.json",
            "name": manifest.id,
            "client_namespace": manifest.client_namespace,
            "components": self._portable_components(package_dir),
        }
        checked["source"] = {
            key: value for key, value in {
                "kind": source.kind,
                "path": source.path,
                "url": source.url,
                "ref": source.ref,
                "sha256": source.sha256,
                "extension_id": source.extension_id,
                "version": source.version,
            }.items() if value
        }
        checked["immutable"] = bool(
            source.kind == "git" and checked.get("commit")
            or source.kind in {"archive", "http"}
            and checked.get("sha256") not in {None, "", "not-provided"}
        )
        checked["phases"] = [
            "downloaded", "verified", "unpacked", "plugin_manifest_validated",
            "dependencies_checked", "pending_activation",
        ]
        # 该结构必须始终能进入 JSON receipt / record。
        json.dumps(checked, ensure_ascii=False)
        return manifest, checked

    @staticmethod
    def _portable_components(package_dir: Path) -> dict[str, Any]:
        """发现 Agent Plugins 1.0.0 固定位置，组件错误不阻断 Muteki 命名空间。"""
        root = package_dir.resolve()
        skills_path = root / "skills"
        skills: list[str] = []
        skills_state = "absent"
        if skills_path.exists():
            if skills_path.is_dir() and root in skills_path.resolve().parents:
                skills_state = "ready"
                for child in sorted(skills_path.iterdir()):
                    skill_file = child / "SKILL.md"
                    if (child.is_dir() and skill_file.is_file()
                            and root in skill_file.resolve().parents):
                        skills.append(child.name)
            else:
                skills_state = "invalid"
        mcp_path = root / "mcp.json"
        mcp_state = "absent"
        if mcp_path.exists():
            mcp_state = (
                "ready"
                if mcp_path.is_file() and root in mcp_path.resolve().parents
                else "invalid"
            )
        return {
            "skills": skills,
            "skills_state": skills_state,
            "mcp_state": mcp_state,
        }

    # -- 来源解析 ---------------------------------------------------------------

    def _resolve(self, source: Source) -> Source:
        if source.kind != "catalog":
            return source
        if not source.catalog_root or not source.extension_id:
            raise InstallError(
                "extension.invalid_source",
                "catalog source requires catalog_root and extension_id",
            )
        try:
            entry = ExtensionCatalog(source.catalog_root).resolve(
                source.extension_id, source.version or None
            )
        except Exception as exc:
            raise InstallError(
                "extension.catalog_unavailable", str(exc)) from exc
        return Source(
            kind=entry.source.kind,
            path=entry.source.path,
            url=entry.source.url,
            ref=entry.source.ref,
            sha256=entry.source.sha256,
            publisher=entry.publisher,
            published_at=entry.published_at,
            signature=entry.signature,
            signature_algorithm=entry.signature_algorithm,
            trust_status=entry.trust_status,
            revoked=entry.revoked,
            revoke_reason=entry.revoke_reason,
            extension_id=entry.id,
            version=entry.version,
        )

    # -- fetch + verify ----------------------------------------------------------

    def _fetch(
        self, source: Source, workdir: Path
    ) -> tuple[Path, dict[str, Any]]:
        """按来源取包到 workdir 下的目录，返回 (包目录, 校验证据)。"""
        if source.kind == "local-dir":
            return self._fetch_local_dir(source, workdir)
        if source.kind == "archive":
            return self._fetch_archive(source, workdir)
        if source.kind == "git":
            return self._fetch_git(source, workdir)
        if source.kind == "http":
            return self._fetch_http(source, workdir)
        raise InstallError(
            "extension.invalid_source", f"unknown source kind: {source.kind!r}"
        )

    def _fetch_local_dir(
        self, source: Source, workdir: Path
    ) -> tuple[Path, dict[str, Any]]:
        origin = Path(source.path)
        if not origin.is_dir():
            raise InstallError(
                "extension.source_not_found",
                f"local-dir source is not a directory: {origin}",
            )
        target = workdir / "pkg"
        shutil.copytree(origin, target)
        return target, {"kind": "local-dir", "path": str(origin)}

    def _fetch_archive(
        self, source: Source, workdir: Path
    ) -> tuple[Path, dict[str, Any]]:
        archive = Path(source.path)
        if not archive.is_file():
            raise InstallError(
                "extension.source_not_found",
                f"archive source is not a file: {archive}",
            )
        verification: dict[str, Any] = {"kind": "archive", "path": str(archive)}
        if source.sha256:
            self._verify_sha256(archive, source.sha256)
            verification["sha256"] = source.sha256
        else:
            verification["sha256"] = "not-provided"
        verification["signature"] = "unsigned"
        return self._unpack(archive, workdir / "pkg"), verification

    def _fetch_git(
        self, source: Source, workdir: Path
    ) -> tuple[Path, dict[str, Any]]:
        if not source.url.strip():
            raise InstallError(
                "extension.invalid_source", "git source requires url"
            )
        ref = source.ref.strip()
        if not ref:
            raise InstallError(
                "extension.unpinned_git_ref",
                "git source must pin a commit or tag via ref "
                "(bare branches are not accepted)",
            )
        checkout = workdir / "pkg"
        try:
            subprocess.run(
                ["git", "clone", "--quiet", "--no-checkout", source.url,
                 str(checkout)],
                check=True, capture_output=True, text=True, timeout=120,
            )
            subprocess.run(
                ["git", "-C", str(checkout), "checkout", "--quiet", ref],
                check=True, capture_output=True, text=True, timeout=60,
            )
        except subprocess.CalledProcessError as exc:
            raise InstallError(
                "extension.git_fetch_failed",
                f"git fetch failed for {source.url}@{ref}: "
                f"{(exc.stderr or '').strip()}",
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise InstallError(
                "extension.git_fetch_failed",
                f"git fetch timed out for {source.url}@{ref}",
            ) from exc
        resolved = subprocess.run(
            ["git", "-C", str(checkout), "rev-parse", "HEAD"],
            check=True, capture_output=True, text=True,
        ).stdout.strip()
        is_commit = bool(re.fullmatch(r"[0-9a-fA-F]{7,40}", ref))
        is_tag = subprocess.run(
            ["git", "-C", str(checkout), "show-ref", "--verify", "--quiet",
             f"refs/tags/{ref}"],
            check=False, capture_output=True,
        ).returncode == 0
        if not is_commit and not is_tag:
            raise InstallError(
                "extension.unpinned_git_ref",
                "git ref must be an immutable commit hash or tag; branches are rejected",
            )
        # .git 不随安装目录保留（安装目录是内容快照，版本由目录名固定）
        shutil.rmtree(checkout / ".git", ignore_errors=True)
        return checkout, {"kind": "git", "url": source.url, "ref": ref,
                          "ref_kind": "commit" if is_commit else "tag",
                          "commit": resolved, "signature": "unsigned"}

    def _fetch_http(
        self, source: Source, workdir: Path
    ) -> tuple[Path, dict[str, Any]]:
        url = source.url.strip()
        if not url.startswith(("https://", "http://")):
            raise InstallError(
                "extension.invalid_source",
                f"http source requires an http(s) url: {source.url!r}",
            )
        if not source.sha256.strip():
            raise InstallError(
                "extension.missing_sha256",
                "http source requires a sha256 to verify the archive",
            )
        archive = workdir / "download.bin"
        try:
            with urllib.request.urlopen(url, timeout=HTTP_TIMEOUT_SECONDS) as resp:
                data = resp.read(MAX_DOWNLOAD_BYTES + 1)
        except (OSError, ValueError) as exc:
            raise InstallError(
                "extension.http_fetch_failed", f"cannot download {url}: {exc}"
            ) from exc
        if len(data) > MAX_DOWNLOAD_BYTES:
            raise InstallError(
                "extension.archive_too_large",
                f"archive exceeds {MAX_DOWNLOAD_BYTES} bytes",
            )
        archive.write_bytes(data)
        self._verify_sha256(archive, source.sha256)
        return self._unpack(archive, workdir / "pkg"), {
            "kind": "http", "url": url, "sha256": source.sha256,
            "signature": "unsigned",
        }

    @staticmethod
    def _verify_sha256(path: Path, expected: str) -> None:
        actual = sha256_file(path)
        if actual.lower() != expected.strip().lower():
            raise InstallError(
                "extension.sha256_mismatch",
                f"archive hash mismatch: expected {expected}, got {actual}",
            )

    # -- 解包（拒绝路径穿越） -------------------------------------------------------

    def _unpack(self, archive: Path, target: Path) -> Path:
        target.mkdir(parents=True, exist_ok=True)
        name = archive.name.lower()
        if zipfile.is_zipfile(archive):
            with zipfile.ZipFile(archive) as zf:
                for info in zf.infolist():
                    self._check_member(info.filename, archive)
                zf.extractall(target)
        elif tarfile.is_tarfile(archive) or name.endswith((".tar.gz", ".tgz", ".tar")):
            with tarfile.open(archive) as tf:
                for member in tf.getmembers():
                    self._check_member(member.name, archive)
                    if member.issym() or member.islnk():
                        raise InstallError(
                            "extension.unsafe_archive",
                            f"archive {archive} contains a link entry: "
                            f"{member.name}",
                        )
                tf.extractall(target, filter="data")
        else:
            raise InstallError(
                "extension.unsupported_archive",
                f"unsupported archive format (tar/tar.gz/tgz/zip): {archive}",
            )
        return self._single_root(target)

    @staticmethod
    def _check_member(member_name: str, archive: Path) -> None:
        parts = Path(member_name).parts
        if member_name.startswith("/") or ".." in parts:
            raise InstallError(
                "extension.unsafe_archive",
                f"archive {archive} contains an unsafe path: {member_name}",
            )

    @staticmethod
    def _single_root(target: Path) -> Path:
        """归档常见的单顶层目录约定：若只有一层目录则下沉为包根。"""
        entries = list(target.iterdir())
        if len(entries) == 1 and entries[0].is_dir():
            # 根 plugin.json 也可能直接在归档顶层；找不到才下沉。
            from muteki.extensions.manifest import MANIFEST_FILENAMES
            if not any((target / name).is_file() for name in MANIFEST_FILENAMES):
                return entries[0]
        return target


__all__ = [
    "ExtensionInstaller",
    "InstallError",
    "InstalledPackage",
    "Source",
    "sha256_file",
    "sha256_tree",
]
