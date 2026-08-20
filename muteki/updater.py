"""无 Git 依赖的本地托管安装、升级与回滚。"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Iterator

from muteki.version import get_version, project_root

DEFAULT_REPOSITORY = "FishCodeTech/muteki"
MANIFEST_SCHEMA = 1
INSTALL_SCHEMA = 1
_VERSION_RE = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?$")


class UpdateError(RuntimeError):
    """可直接展示给终端或 Web 用户的升级错误。"""


def default_install_root() -> Path:
    configured = os.environ.get("MUTEKI_INSTALL_ROOT")
    if configured:
        return Path(configured).expanduser().resolve()
    return (Path.home() / ".local" / "share" / "muteki").resolve()


def _now() -> str:
    from datetime import datetime, timezone

    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def _version_tuple(value: str) -> tuple[int, int, int, int, str]:
    normalized = value.strip().removeprefix("v")
    match = _VERSION_RE.fullmatch(normalized)
    if not match:
        raise UpdateError(f"发布版本格式无效：{value}")
    suffix = normalized.split("-", 1)[1].split("+", 1)[0] if "-" in normalized else ""
    return int(match.group(1)), int(match.group(2)), int(match.group(3)), 1 if not suffix else 0, suffix


def _atomic_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    temporary.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, TypeError):
        return {}
    return value if isinstance(value, dict) else {}


def _github_token() -> str:
    return (
        os.environ.get("MUTEKI_GITHUB_TOKEN")
        or os.environ.get("GH_TOKEN")
        or os.environ.get("GITHUB_TOKEN")
        or ""
    ).strip()


def _release_tag(version: str) -> str:
    value = version.strip()
    return value if value.startswith("v") else f"v{value}"


def _image_registry(images: dict[str, str] | None = None) -> str:
    configured = os.environ.get("MUTEKI_IMAGE_REGISTRY", "").strip().rstrip("/")
    if configured:
        return configured
    if images:
        web = str(images.get("web") or "")
        marker = "/muteki-web:"
        index = web.find(marker)
        if index > 0:
            return web[:index]
    return "ghcr.io/fishcodetech"


def _github_release_asset_url(page_url: str) -> str | None:
    """把 GitHub Release 浏览器下载地址换成 API asset URL，避免跳转时丢掉鉴权头。"""
    parsed = urllib.parse.urlparse(page_url)
    if parsed.netloc.lower() not in {"github.com", "www.github.com"}:
        return None
    parts = [item for item in parsed.path.split("/") if item]
    if len(parts) < 6 or parts[2] != "releases":
        return None
    token = _github_token()
    if not token:
        return None
    owner, repo = parts[0], parts[1]
    if parts[3] == "latest" and parts[4] == "download":
        filename = "/".join(parts[5:])
        api = f"https://api.github.com/repos/{owner}/{repo}/releases/latest"
    elif parts[3] == "download":
        filename = "/".join(parts[5:])
        api = f"https://api.github.com/repos/{owner}/{repo}/releases/tags/{parts[4]}"
    else:
        return None
    request = urllib.request.Request(
        api,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "muteki-updater/0.3",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=45) as response:
        payload = json.loads(response.read().decode("utf-8"))
    if not isinstance(payload, dict):
        return None
    for asset in payload.get("assets") or []:
        if isinstance(asset, dict) and str(asset.get("name") or "") == filename:
            url = str(asset.get("url") or "").strip()
            return url or None
    return None


def _urlopen(url: str):
    parsed = urllib.parse.urlparse(url)
    token = _github_token()
    github = parsed.netloc.lower() in {"github.com", "www.github.com"}
    asset_url = None
    if token and github:
        try:
            asset_url = _github_release_asset_url(url)
        except (OSError, urllib.error.URLError, json.JSONDecodeError, UnicodeDecodeError, ValueError) as exc:
            raise UpdateError(f"无法读取 GitHub Release：{exc}") from exc
        if not asset_url:
            raise UpdateError("该 GitHub Release 没有发布清单文件 release-manifest.json")
    target = asset_url or url
    headers = {
        "User-Agent": "muteki-updater/0.3",
        "Accept": "application/octet-stream" if asset_url else "application/json, application/octet-stream",
    }
    if token and asset_url:
        headers["Authorization"] = f"Bearer {token}"
        headers["X-GitHub-Api-Version"] = "2022-11-28"
    request = urllib.request.Request(target, headers=headers)
    return urllib.request.urlopen(request, timeout=45)


@dataclass(frozen=True)
class ReleaseArtifact:
    url: str
    sha256: str
    size: int


@dataclass(frozen=True)
class ReleaseManifest:
    version: str
    commit: str
    published_at: str
    artifact: ReleaseArtifact
    images: dict[str, str]
    minimum_updater_version: str
    source_url: str

    @classmethod
    def from_payload(cls, payload: dict[str, Any], source_url: str) -> "ReleaseManifest":
        if int(payload.get("schema_version") or 0) != MANIFEST_SCHEMA:
            raise UpdateError("发布清单版本不受支持")
        version = str(payload.get("version") or "").strip().removeprefix("v")
        _version_tuple(version)
        raw_artifact = payload.get("artifact")
        if not isinstance(raw_artifact, dict):
            raise UpdateError("发布清单缺少应用包信息")
        raw_url = str(raw_artifact.get("url") or "").strip()
        sha256 = str(raw_artifact.get("sha256") or "").strip().lower()
        try:
            size = int(raw_artifact.get("size") or 0)
        except (TypeError, ValueError):
            size = 0
        if not raw_url or not re.fullmatch(r"[0-9a-f]{64}", sha256) or size <= 0:
            raise UpdateError("发布清单中的应用包校验信息无效")
        resolved_url = urllib.parse.urljoin(source_url, raw_url)
        images = payload.get("images")
        minimum = str(payload.get("minimum_updater_version") or "0.3.0").removeprefix("v")
        _version_tuple(minimum)
        if _version_tuple(minimum) > _version_tuple(get_version()):
            raise UpdateError(f"该版本要求升级工具至少为 {minimum}，当前版本为 {get_version()}")
        return cls(
            version=version,
            commit=str(payload.get("commit") or ""),
            published_at=str(payload.get("published_at") or ""),
            artifact=ReleaseArtifact(url=resolved_url, sha256=sha256, size=size),
            images={str(k): str(v) for k, v in images.items()} if isinstance(images, dict) else {},
            minimum_updater_version=minimum,
            source_url=source_url,
        )


class UpdateLock:
    def __init__(self, path: Path, *, stale_after: int = 7200) -> None:
        self.path = path
        self.stale_after = stale_after
        self.acquired = False

    def __enter__(self) -> "UpdateLock":
        self.path.parent.mkdir(parents=True, exist_ok=True)
        for _ in range(2):
            try:
                descriptor = os.open(self.path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
            except FileExistsError:
                try:
                    stale = time.time() - self.path.stat().st_mtime > self.stale_after
                except OSError:
                    stale = False
                if stale:
                    self.path.unlink(missing_ok=True)
                    continue
                raise UpdateError("已有升级任务正在执行")
            with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
                json.dump({"pid": os.getpid(), "started_at": _now()}, handle)
            self.acquired = True
            return self
        raise UpdateError("无法取得升级任务锁")

    def __exit__(self, *_args: object) -> None:
        if self.acquired:
            self.path.unlink(missing_ok=True)


class UpdateManager:
    def __init__(self, install_root: Path | None = None, manifest_url: str | None = None) -> None:
        self.root = (install_root or default_install_root()).expanduser().resolve()
        self.metadata_path = self.root / "install.json"
        self.state_path = self.root / "update-state.json"
        self.manifest_override = manifest_url or os.environ.get("MUTEKI_UPDATE_MANIFEST_URL")

    @property
    def installed(self) -> bool:
        return self.metadata_path.is_file() and (self.root / "current").exists()

    def metadata(self) -> dict[str, Any]:
        return _read_json(self.metadata_path)

    def current_version(self) -> str:
        value = str(self.metadata().get("current_version") or "").strip()
        return value.removeprefix("v") if value else get_version()

    def manifest_url(self, target_version: str | None = None) -> str:
        if self.manifest_override:
            return self.manifest_override.format(version=(target_version or "latest").removeprefix("v"))
        repository = os.environ.get("MUTEKI_RELEASE_REPOSITORY", DEFAULT_REPOSITORY).strip("/")
        if target_version:
            tag = _release_tag(target_version)
            return f"https://github.com/{repository}/releases/download/{tag}/release-manifest.json"
        return f"https://github.com/{repository}/releases/latest/download/release-manifest.json"

    def fetch_manifest(self, target_version: str | None = None) -> ReleaseManifest:
        url = self.manifest_url(target_version)
        try:
            with _urlopen(url) as response:
                raw = response.read(1024 * 1024 + 1)
        except (OSError, urllib.error.URLError) as exc:
            hint = "。非公开 Release 需要设置 MUTEKI_GITHUB_TOKEN" if "github.com" in url and not _github_token() else ""
            raise UpdateError(f"无法读取发布清单：{exc}{hint}") from exc
        if len(raw) > 1024 * 1024:
            raise UpdateError("发布清单超过 1 MiB")
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise UpdateError("发布清单不是有效 JSON") from exc
        if not isinstance(payload, dict):
            raise UpdateError("发布清单必须是 JSON 对象")
        manifest = ReleaseManifest.from_payload(payload, url)
        if target_version and manifest.version != target_version.removeprefix("v"):
            raise UpdateError(f"发布清单版本不匹配：期望 {target_version}，实际 {manifest.version}")
        return manifest

    def _write_state(self, **values: Any) -> dict[str, Any]:
        payload = {
            **_read_json(self.state_path),
            "schema_version": 1,
            "status": "idle",
            "current_version": self.current_version(),
            "updated_at": _now(),
            **values,
        }
        _atomic_json(self.state_path, payload)
        return payload

    def status(self) -> dict[str, Any]:
        state = _read_json(self.state_path)
        metadata = self.metadata()
        active_version = get_version()
        selected_version = self.current_version()
        deployment = str(os.environ.get("MUTEKI_DEPLOYMENT_MODE") or metadata.get("deployment") or "local")
        return {
            "status": str(state.get("status") or "idle"),
            "current_version": selected_version,
            "active_version": active_version,
            "latest_version": state.get("latest_version"),
            "available": bool(state.get("available", False)),
            "progress": state.get("progress"),
            "message": state.get("message"),
            "error": state.get("error"),
            "checked_at": state.get("checked_at"),
            "updated_at": state.get("updated_at"),
            "restart_required": bool(state.get("restart_required", False) and active_version != selected_version),
            "install_kind": "compose" if deployment == "compose" else "managed" if self.installed else "source",
            "install_root": str(self.root),
            "channel": str(metadata.get("channel") or "stable"),
            "previous_version": metadata.get("previous_version"),
            "deployment": deployment,
        }

    def check(self, target_version: str | None = None) -> dict[str, Any]:
        self._write_state(status="checking", message="正在读取发布清单", error=None)
        try:
            manifest = self.fetch_manifest(target_version)
            current = self.current_version()
            if target_version:
                available = manifest.version != current or not self.installed
            else:
                available = _version_tuple(manifest.version) > _version_tuple(current) or not self.installed
            return self._write_state(
                status="available" if available else "current",
                current_version=current,
                latest_version=manifest.version,
                available=available,
                checked_at=_now(),
                message=f"可升级到 {manifest.version}" if available else "当前已是最新版本",
                error=None,
                manifest={
                    "version": manifest.version,
                    "commit": manifest.commit,
                    "published_at": manifest.published_at,
                    "images": manifest.images,
                    "source_url": manifest.source_url,
                },
            )
        except Exception as exc:
            self._write_state(status="error", available=False, error=str(exc), message="检查更新失败")
            raise

    def _download(self, manifest: ReleaseManifest, destination: Path) -> None:
        digest = hashlib.sha256()
        received = 0
        try:
            with _urlopen(manifest.artifact.url) as response, destination.open("wb") as output:
                while True:
                    chunk = response.read(1024 * 1024)
                    if not chunk:
                        break
                    received += len(chunk)
                    if received > manifest.artifact.size:
                        raise UpdateError("下载内容超过发布清单声明的大小")
                    digest.update(chunk)
                    output.write(chunk)
                    progress = max(1, min(70, int(received * 70 / manifest.artifact.size)))
                    self._write_state(status="downloading", progress=progress, message=f"正在下载 {manifest.version}")
        except (OSError, urllib.error.URLError) as exc:
            raise UpdateError(f"应用包下载失败：{exc}") from exc
        if received != manifest.artifact.size:
            raise UpdateError(f"应用包大小不匹配：期望 {manifest.artifact.size}，实际 {received}")
        if digest.hexdigest() != manifest.artifact.sha256:
            raise UpdateError("应用包 SHA-256 校验失败")

    @staticmethod
    def _safe_members(archive: tarfile.TarFile, target: Path) -> Iterator[tarfile.TarInfo]:
        base = target.resolve()
        for member in archive.getmembers():
            if member.issym() or member.islnk() or member.isdev():
                raise UpdateError(f"应用包包含不允许的链接或设备文件：{member.name}")
            resolved = (target / member.name).resolve()
            if resolved != base and base not in resolved.parents:
                raise UpdateError(f"应用包包含越界路径：{member.name}")
            yield member

    def _extract(self, archive_path: Path, staging: Path) -> Path:
        staging.mkdir(parents=True, exist_ok=False)
        try:
            with tarfile.open(archive_path, "r:gz") as archive:
                archive.extractall(staging, members=self._safe_members(archive, staging), filter="data")
        except (tarfile.TarError, OSError) as exc:
            raise UpdateError(f"无法解压应用包：{exc}") from exc
        roots = [path for path in staging.iterdir() if path.is_dir()]
        candidate = roots[0] if len(roots) == 1 and (roots[0] / "pyproject.toml").is_file() else staging
        if not (candidate / "pyproject.toml").is_file() or not (candidate / "muteki").is_dir():
            raise UpdateError("应用包缺少 pyproject.toml 或 muteki 目录")
        return candidate

    def _prepare(self, release_dir: Path) -> None:
        self._write_state(status="preparing", progress=78, message="正在安装运行依赖")
        skip_sync = os.environ.get("MUTEKI_UPGRADE_SKIP_SYNC", "").lower() in {"1", "true", "yes"}
        if not skip_sync:
            uv = shutil.which("uv")
            if not uv:
                raise UpdateError("未找到 uv，请先安装 uv 后重新执行升级")
            result = subprocess.run(
                [uv, "sync", "--frozen", "--no-dev"],
                cwd=release_dir,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=1200,
            )
            if result.returncode:
                tail = "\n".join(result.stdout.splitlines()[-20:])
                raise UpdateError(f"运行依赖安装失败：\n{tail}")
            python = release_dir / ".venv" / "bin" / "python"
        else:
            python = Path(sys.executable)
        environment = os.environ.copy()
        environment["MUTEKI_RELEASE_ROOT"] = str(release_dir)
        if skip_sync:
            environment["PYTHONPATH"] = str(release_dir)
        result = subprocess.run(
            [str(python), "-m", "muteki.cli", "version", "--json"],
            cwd=release_dir,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=60,
        )
        if result.returncode:
            raise UpdateError(f"新版本启动检查失败：{result.stdout.strip()}")

    def _set_link(self, name: str, target: Path) -> None:
        link = self.root / name
        temporary = self.root / f".{name}.{uuid.uuid4().hex}"
        relative = os.path.relpath(target, self.root)
        os.symlink(relative, temporary)
        os.replace(temporary, link)

    def _initial_metadata(self) -> dict[str, Any]:
        existing = self.metadata()
        source = project_root()
        env_file = source / ".env"
        source_sessions = source / "sessions"
        default_data = self.root / "data"
        return {
            "schema_version": INSTALL_SCHEMA,
            "channel": str(existing.get("channel") or "stable"),
            "deployment": str(os.environ.get("MUTEKI_DEPLOYMENT_MODE") or existing.get("deployment") or "local"),
            "data_root": str(existing.get("data_root") or (source_sessions.parent if source_sessions.is_dir() else default_data)),
            "sessions_root": str(existing.get("sessions_root") or (source_sessions if source_sessions.is_dir() else default_data / "sessions")),
            "control_root": str(existing.get("control_root") or default_data / "coordinator-control"),
            "env_file": str(existing.get("env_file") or (env_file if env_file.is_file() else self.root / "config" / ".env")),
            "installed_at": str(existing.get("installed_at") or _now()),
            **existing,
        }

    def _install_launcher(self, release_dir: Path) -> Path:
        source = release_dir / "scripts" / "muteki-launcher"
        if not source.is_file():
            raise UpdateError("应用包缺少稳定启动器")
        bin_dir = Path(os.environ.get("MUTEKI_BIN_DIR") or Path.home() / ".local" / "bin").expanduser().resolve()
        bin_dir.mkdir(parents=True, exist_ok=True)
        destination = bin_dir / "muteki"
        temporary = bin_dir / f".muteki.{uuid.uuid4().hex}"
        shutil.copy2(source, temporary)
        temporary.chmod(0o755)
        os.replace(temporary, destination)
        return destination

    def upgrade(self, target_version: str | None = None, *, force: bool = False) -> dict[str, Any]:
        self.root.mkdir(parents=True, exist_ok=True)
        with UpdateLock(self.root / ".update.lock"):
            try:
                manifest = self.fetch_manifest(target_version)
                current_version = self.current_version()
                if self.installed and manifest.version == current_version and not force:
                    return self._write_state(
                        status="current", latest_version=manifest.version, available=False,
                        progress=100, message="当前已是指定版本", error=None,
                    )
                self._write_state(
                    status="downloading", latest_version=manifest.version, available=True,
                    progress=1, message=f"准备升级到 {manifest.version}", error=None,
                )
                releases = self.root / "releases"
                cache = self.root / "cache"
                releases.mkdir(parents=True, exist_ok=True)
                cache.mkdir(parents=True, exist_ok=True)
                with tempfile.NamedTemporaryFile(prefix="muteki-", suffix=".tar.gz", dir=cache, delete=False) as handle:
                    archive_path = Path(handle.name)
                staging = releases / f".{manifest.version}.staging-{uuid.uuid4().hex}"
                backup: Path | None = None
                try:
                    self._download(manifest, archive_path)
                    extracted = self._extract(archive_path, staging)
                    release_dir = releases / manifest.version
                    if extracted != staging:
                        normalized = releases / f".{manifest.version}.normalized-{uuid.uuid4().hex}"
                        os.replace(extracted, normalized)
                        shutil.rmtree(staging)
                        staging = normalized
                    release_meta = {
                        "schema_version": 1,
                        "version": manifest.version,
                        "commit": manifest.commit,
                        "built_at": manifest.published_at or _now(),
                        "manifest_url": manifest.source_url,
                        "artifact_sha256": manifest.artifact.sha256,
                        "images": manifest.images,
                    }
                    _atomic_json(staging / ".muteki-release.json", release_meta)
                    self._prepare(staging)
                    self._write_state(status="switching", progress=92, message="正在切换应用版本")
                    if release_dir.exists():
                        backup = releases / f".{manifest.version}.replaced-{uuid.uuid4().hex}"
                        os.replace(release_dir, backup)
                    os.replace(staging, release_dir)
                    current_link = self.root / "current"
                    old_target = current_link.resolve() if current_link.exists() else None
                    if old_target and old_target != release_dir:
                        self._set_link("previous", old_target)
                    self._set_link("current", release_dir)
                    metadata = self._initial_metadata()
                    metadata.update({
                        "current_version": manifest.version,
                        "previous_version": current_version if old_target and current_version != manifest.version else metadata.get("previous_version"),
                        "updated_at": _now(),
                        "release": release_meta,
                    })
                    Path(metadata["sessions_root"]).mkdir(parents=True, exist_ok=True)
                    Path(metadata["control_root"]).mkdir(parents=True, exist_ok=True)
                    Path(metadata["env_file"]).parent.mkdir(parents=True, exist_ok=True)
                    _atomic_json(self.metadata_path, metadata)
                    launcher = self._install_launcher(release_dir)
                    if backup:
                        shutil.rmtree(backup, ignore_errors=True)
                    return self._write_state(
                        status="installed", current_version=manifest.version,
                        latest_version=manifest.version, available=False, progress=100,
                        restart_required=True, message=f"{manifest.version} 已安装，重启服务后生效",
                        error=None, launcher=str(launcher),
                        manifest={
                            "version": manifest.version,
                            "commit": manifest.commit,
                            "published_at": manifest.published_at,
                            "images": manifest.images,
                            "source_url": manifest.source_url,
                        },
                    )
                finally:
                    archive_path.unlink(missing_ok=True)
                    if staging.exists():
                        shutil.rmtree(staging, ignore_errors=True)
            except Exception as exc:
                self._write_state(status="error", progress=None, error=str(exc), message="升级失败")
                raise

    def rollback(self) -> dict[str, Any]:
        with UpdateLock(self.root / ".update.lock"):
            current_link = self.root / "current"
            previous_link = self.root / "previous"
            if not current_link.exists() or not previous_link.exists():
                raise UpdateError("没有可回滚的上一版本")
            current_target = current_link.resolve()
            previous_target = previous_link.resolve()
            if not previous_target.is_dir():
                raise UpdateError("上一版本目录不存在")
            current_version = self.current_version()
            previous_release = _read_json(previous_target / ".muteki-release.json")
            previous_version = str(previous_release.get("version") or previous_target.name).removeprefix("v")
            self._set_link("current", previous_target)
            self._set_link("previous", current_target)
            metadata = self._initial_metadata()
            metadata.update({
                "current_version": previous_version,
                "previous_version": current_version,
                "updated_at": _now(),
                "release": previous_release,
            })
            _atomic_json(self.metadata_path, metadata)
            return self._write_state(
                status="rolled_back", current_version=previous_version,
                latest_version=previous_version, available=False, progress=100,
                restart_required=True, error=None,
                message=f"已回滚到 {previous_version}，重启服务后生效",
            )

    def _compose_apply(self, version: str, images: dict[str, str] | None = None) -> None:
        compose_file = project_root() / "docker-compose.release.yml"
        if not compose_file.is_file():
            raise UpdateError(f"缺少版本化容器编排文件：{compose_file}")
        docker = shutil.which("docker")
        if not docker:
            raise UpdateError("未找到 docker 命令")
        environment = os.environ.copy()
        environment["MUTEKI_RELEASE_VERSION"] = _release_tag(version)
        environment["MUTEKI_IMAGE_REGISTRY"] = _image_registry(images)
        steps = ((["pull"], "拉取版本镜像", 45), (["up", "-d", "--wait"], "切换容器版本", 80))
        for arguments, label, progress in steps:
            self._write_state(status="preparing", message=f"正在{label}", progress=progress)
            result = subprocess.run(
                [docker, "compose", "-f", str(compose_file), *arguments],
                cwd=compose_file.parent,
                env=environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=1800,
            )
            if result.returncode:
                tail = "\n".join(result.stdout.splitlines()[-30:])
                raise UpdateError(f"{label}失败：\n{tail}")

    def compose_upgrade(self, target_version: str | None = None) -> dict[str, Any]:
        self.root.mkdir(parents=True, exist_ok=True)
        with UpdateLock(self.root / ".update.lock"):
            try:
                manifest = self.fetch_manifest(target_version)
                current = self.current_version()
                self._write_state(status="preparing", latest_version=manifest.version, progress=5, message="正在准备容器升级", error=None)
                self._compose_apply(manifest.version, images=manifest.images)
                metadata = self._initial_metadata()
                metadata.update({
                    "deployment": "compose",
                    "current_version": manifest.version,
                    "previous_version": current if current != manifest.version else metadata.get("previous_version"),
                    "updated_at": _now(),
                    "release": {"version": manifest.version, "commit": manifest.commit, "images": manifest.images},
                })
                _atomic_json(self.metadata_path, metadata)
                return self._write_state(
                    status="installed", current_version=manifest.version, latest_version=manifest.version,
                    available=False, progress=100, restart_required=False, error=None,
                    message=f"容器服务已升级到 {manifest.version}",
                )
            except Exception as exc:
                self._write_state(status="error", progress=None, error=str(exc), message="容器升级失败")
                raise

    def compose_rollback(self) -> dict[str, Any]:
        self.root.mkdir(parents=True, exist_ok=True)
        with UpdateLock(self.root / ".update.lock"):
            metadata = self.metadata()
            previous = str(metadata.get("previous_version") or "").removeprefix("v")
            if not previous:
                raise UpdateError("没有可回滚的上一容器版本")
            current = self.current_version()
            release = metadata.get("release") if isinstance(metadata.get("release"), dict) else {}
            images = release.get("images") if isinstance(release, dict) else None
            self._compose_apply(previous, images=images if isinstance(images, dict) else None)
            metadata.update({
                "deployment": "compose",
                "current_version": previous,
                "previous_version": current,
                "updated_at": _now(),
            })
            _atomic_json(self.metadata_path, metadata)
            return self._write_state(
                status="rolled_back", current_version=previous, latest_version=previous,
                available=False, progress=100, restart_required=False, error=None,
                message=f"容器服务已回滚到 {previous}",
            )


def apply_managed_environment(install_root: Path | None = None) -> dict[str, Any]:
    manager = UpdateManager(install_root)
    metadata = manager.metadata()
    if not metadata:
        return {}
    os.environ["MUTEKI_INSTALL_ROOT"] = str(manager.root)
    current = manager.root / "current"
    if current.exists():
        os.environ["MUTEKI_RELEASE_ROOT"] = str(current.resolve())
    mappings = {
        "MUTEKI_ENV_FILE": "env_file",
        "MUTEKI_SESSIONS_ROOT": "sessions_root",
        "MUTEKI_COORDINATOR_CONTROL_ROOT": "control_root",
    }
    for environment_name, key in mappings.items():
        value = metadata.get(key)
        if value and environment_name not in os.environ:
            os.environ[environment_name] = str(value)
    release = metadata.get("release")
    images = release.get("images") if isinstance(release, dict) else None
    if isinstance(images, dict):
        worker = images.get("worker")
        if worker and "MUTEKI_WORKER_IMAGE" not in os.environ:
            os.environ["MUTEKI_WORKER_IMAGE"] = str(worker)
        if metadata.get("current_version") and "MUTEKI_WORKER_IMAGE_VERSION" not in os.environ:
            os.environ["MUTEKI_WORKER_IMAGE_VERSION"] = _release_tag(str(metadata["current_version"]))
    return metadata
