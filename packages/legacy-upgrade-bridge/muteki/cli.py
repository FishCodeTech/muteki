"""Bootstrap the platform-specific native Muteki package after a legacy upgrade."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any


def project_root() -> Path:
    return Path(__file__).resolve().parents[1]


def install_root(root: Path) -> Path:
    configured = os.environ.get("MUTEKI_INSTALL_ROOT")
    if configured:
        return Path(configured).expanduser().resolve()
    if root.parent.name == "releases":
        return root.parent.parent.resolve()
    return (Path.home() / ".local" / "share" / "muteki").resolve()


def read_metadata(root: Path) -> dict[str, Any]:
    payload = json.loads((root / "native-assets.json").read_text(encoding="utf-8"))
    if not isinstance(payload, dict) or payload.get("schema_version") != 1:
        raise RuntimeError("原生升级资产清单版本不受支持")
    return payload


def platform_key() -> str:
    system = {"Darwin": "darwin", "Linux": "linux"}.get(platform.system())
    machine = {
        "arm64": "arm64",
        "aarch64": "arm64",
        "x86_64": "amd64",
        "AMD64": "amd64",
    }.get(platform.machine())
    if not system or not machine:
        raise RuntimeError(f"暂无适用于 {platform.system()}/{platform.machine()} 的原生 Web 包")
    return f"{system}-{machine}"


def asset_for_host(metadata: dict[str, Any]) -> dict[str, Any]:
    platforms = metadata.get("platforms")
    asset = platforms.get(platform_key()) if isinstance(platforms, dict) else None
    if not isinstance(asset, dict):
        raise RuntimeError(f"{metadata.get('version')} 发布未包含 {platform_key()} 原生 Web 包")
    url = str(asset.get("url") or "")
    sha256 = str(asset.get("sha256") or "")
    size = asset.get("size")
    parsed = urllib.parse.urlparse(url)
    loopback_http = parsed.scheme == "http" and parsed.hostname in {"127.0.0.1", "localhost", "::1"}
    if parsed.scheme != "https" and not loopback_http:
        raise RuntimeError("原生升级资产必须使用 HTTPS")
    if len(sha256) != 64 or any(character not in "0123456789abcdef" for character in sha256):
        raise RuntimeError("原生升级资产 SHA-256 无效")
    if not isinstance(size, int) or size <= 0:
        raise RuntimeError("原生升级资产大小无效")
    return {"url": url, "sha256": sha256, "size": size}


def download(asset: dict[str, Any], destination: Path) -> None:
    digest = hashlib.sha256()
    received = 0
    request = urllib.request.Request(asset["url"], headers={"User-Agent": "muteki-upgrade-bridge/0.5"})
    with urllib.request.urlopen(request) as response, destination.open("wb") as output:
        while chunk := response.read(1024 * 1024):
            received += len(chunk)
            if received > asset["size"]:
                raise RuntimeError("原生升级资产超过清单声明的大小")
            digest.update(chunk)
            output.write(chunk)
    if received != asset["size"]:
        raise RuntimeError(f"原生升级资产大小不匹配，期望 {asset['size']}，实际 {received}")
    if digest.hexdigest() != asset["sha256"]:
        raise RuntimeError("原生升级资产 SHA-256 校验失败")


def extract_native(archive_path: Path, destination: Path, version: str) -> None:
    extraction = Path(tempfile.mkdtemp(prefix=".native-extract-", dir=destination.parent))
    try:
        with tarfile.open(archive_path, "r:gz") as archive:
            for member in archive.getmembers():
                if not member.isfile() and not member.isdir():
                    raise RuntimeError(f"原生升级资产包含不允许的文件类型：{member.name}")
                member_path = Path(member.name)
                if member_path.is_absolute() or ".." in member_path.parts:
                    raise RuntimeError(f"原生升级资产包含越界路径：{member.name}")
            archive.extractall(extraction, filter="data")
        roots = [path for path in extraction.iterdir() if path.is_dir()]
        if len(roots) != 1:
            raise RuntimeError("原生升级资产必须包含一个顶层目录")
        candidate = roots[0]
        release = json.loads((candidate / "release.json").read_text(encoding="utf-8"))
        if release.get("schema_version") != 1 or release.get("version") != version:
            raise RuntimeError("原生升级资产版本与发布清单不匹配")
        for relative in ("bin/node", "bin/muteki-server", "frontend/server.js", "launcher.mjs"):
            if not (candidate / relative).is_file():
                raise RuntimeError(f"原生升级资产缺少 {relative}")
        os.replace(candidate, destination)
    finally:
        shutil.rmtree(extraction, ignore_errors=True)


def ensure_native(root: Path, metadata: dict[str, Any]) -> Path:
    destination = root / "native"
    lock_path = root / ".native-install.lock"
    with lock_path.open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if destination.is_dir():
            release = json.loads((destination / "release.json").read_text(encoding="utf-8"))
            if release.get("version") != metadata.get("version"):
                raise RuntimeError("已安装的原生包版本与升级桥不一致")
            return destination
        asset = asset_for_host(metadata)
        descriptor, temporary_name = tempfile.mkstemp(prefix=".native-", suffix=".tar.gz", dir=root)
        os.close(descriptor)
        temporary = Path(temporary_name)
        try:
            download(asset, temporary)
            extract_native(temporary, destination, str(metadata.get("version") or ""))
        finally:
            temporary.unlink(missing_ok=True)
        return destination


def atomic_json(path: Path, payload: dict[str, Any]) -> None:
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(payload, output, ensure_ascii=False, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def prepare_native_state(native: Path, managed_root: Path) -> Path:
    state = managed_root / "data" / "native-state"
    state.mkdir(parents=True, exist_ok=True)
    metadata_path = managed_root / "install.json"
    if not metadata_path.is_file():
        return state
    metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    if not isinstance(metadata, dict):
        raise RuntimeError("托管安装元数据无效")
    marker = state / ".legacy-import-complete.json"
    migrated = False
    source_root_value = metadata.get("data_root")
    source_root = Path(source_root_value).expanduser().resolve() if isinstance(source_root_value, str) else None
    current_version = str(metadata.get("current_version") or "")
    if not marker.is_file() and current_version.startswith("0.4"):
        legacy_state_value = metadata.get("state_root")
        legacy_state = Path(legacy_state_value).expanduser().resolve() if isinstance(legacy_state_value, str) else None
        platform_db = source_root / "state" / "_platform" / "platform.db" if source_root else None
        if platform_db and platform_db.is_file():
            result = subprocess.run(
                [
                    str(native / "bin" / "muteki-server"),
                    "--state-dir",
                    str(state),
                    "--import-from",
                    str(source_root),
                    "--import-only",
                ],
                cwd=native,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
            )
            if result.returncode:
                tail = "\n".join(result.stdout.splitlines()[-30:])
                raise RuntimeError(f"v0.4.x 数据迁移失败：\n{tail}")
            migrated = True
        elif legacy_state and legacy_state.is_dir() and any(legacy_state.iterdir()):
            raise RuntimeError("v0.4.x 数据目录缺少 platform.db，无法确认历史数据迁移")
    if not marker.is_file():
        atomic_json(
            marker,
            {
                "schema_version": 1,
                "source_version": current_version or None,
                "source_root": str(source_root) if source_root else None,
                "migrated": migrated,
            },
        )
    metadata["native_state_root"] = str(state)
    if source_root:
        metadata["legacy_data_root"] = str(source_root)
    atomic_json(metadata_path, metadata)
    return state


def main() -> int:
    root = project_root()
    metadata = read_metadata(root)
    native = ensure_native(root, metadata)
    managed_root = install_root(root)
    state = prepare_native_state(native, managed_root)
    node = native / "bin" / "node"
    launcher = native / "launcher.mjs"
    environment = os.environ.copy()
    environment["MUTEKI_INSTALL_ROOT"] = str(managed_root)
    environment["MUTEKI_RELEASE_ROOT"] = str(native)
    environment["MUTEKI_STATE_DIR"] = str(state)
    os.execve(node, [str(node), str(launcher), *sys.argv[1:]], environment)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
