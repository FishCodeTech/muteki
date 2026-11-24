#!/usr/bin/env python3
"""Run the released Python 0.4.7 updater through a native 0.5.0 installation."""

from __future__ import annotations

import argparse
import functools
import hashlib
import http.server
import json
import os
import shutil
import socket
import sqlite3
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


LEGACY_RELEASE_ROOT = "https://github.com/FishCodeTech/muteki/releases/download/v0.4.7"


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, _format: str, *_args: object) -> None:
        return


def download(url: str, destination: Path) -> None:
    request = urllib.request.Request(url, headers={"User-Agent": "muteki-legacy-upgrade-verifier"})
    with urllib.request.urlopen(request) as response, destination.open("wb") as output:
        shutil.copyfileobj(response, output)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def extract_archive(archive_path: Path, destination: Path) -> Path:
    with tarfile.open(archive_path, "r:gz") as archive:
        for member in archive.getmembers():
            if not member.isfile() and not member.isdir():
                raise RuntimeError(f"归档包含不允许的文件类型：{member.name}")
            path = Path(member.name)
            if path.is_absolute() or ".." in path.parts:
                raise RuntimeError(f"归档包含越界路径：{member.name}")
        archive.extractall(destination, filter="data")
    roots = [path for path in destination.iterdir() if path.is_dir()]
    if len(roots) != 1:
        raise RuntimeError(f"归档顶层目录数量不正确：{roots}")
    return roots[0]


def run_json(arguments: list[str], *, cwd: Path, env: dict[str, str]) -> dict[str, Any]:
    result = subprocess.run(
        arguments,
        cwd=cwd,
        env=env,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise RuntimeError(
            f"命令未返回 JSON：{' '.join(arguments)}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        ) from error
    if not isinstance(payload, dict):
        raise RuntimeError(f"命令返回的 JSON 不是对象：{payload!r}")
    return payload


def initialize_legacy_install(
    legacy_source: Path,
    install: Path,
    work: Path,
    env: dict[str, str],
) -> None:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    log_path = work / "legacy-web.log"
    metadata = json.loads((install / "install.json").read_text(encoding="utf-8"))
    environment = env.copy()
    environment["MUTEKI_RELEASE_ROOT"] = str(legacy_source)
    environment["MUTEKI_STATE_ROOT"] = str(metadata["state_root"])
    environment["MUTEKI_SESSIONS_ROOT"] = str(metadata["sessions_root"])
    environment["MUTEKI_COORDINATOR_CONTROL_ROOT"] = str(metadata["control_root"])
    with log_path.open("wb") as log:
        process = subprocess.Popen(
            [str(legacy_source / "run.sh"), "web", "--backend-only", "--port", str(port)],
            cwd=legacy_source,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 60
        try:
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/health") as response:
                        payload = json.loads(response.read())
                    if payload.get("version") == "0.4.7":
                        return
                except (OSError, urllib.error.URLError, json.JSONDecodeError):
                    pass
                time.sleep(0.2)
        finally:
            process.terminate()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    tail = "\n".join(log_path.read_text(errors="replace").splitlines()[-40:])
    raise RuntimeError(f"正式 v0.4.7 Web 没有完成初始化：\n{tail}")


def verify_native_web(
    launcher: Path,
    install: Path,
    state: Path,
    work: Path,
    env: dict[str, str],
) -> None:
    with socket.socket() as api_probe, socket.socket() as ui_probe:
        api_probe.bind(("127.0.0.1", 0))
        ui_probe.bind(("127.0.0.1", 0))
        api_port = api_probe.getsockname()[1]
        ui_port = ui_probe.getsockname()[1]
    environment = env.copy()
    environment["MUTEKI_API_PORT"] = str(api_port)
    environment["MUTEKI_UI_PORT"] = str(ui_port)
    environment["MUTEKI_UI_HOST"] = "127.0.0.1"
    log_path = work / "native-web.log"
    with log_path.open("wb") as log:
        process = subprocess.Popen(
            [str(launcher), "web"],
            cwd=work,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + 90
        verified = False
        try:
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    break
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{ui_port}/api/health") as response:
                        payload = json.loads(response.read())
                    expected_identity = hashlib.sha256(str(state.resolve()).encode()).hexdigest()
                    if payload.get("version") == "0.5.0" and payload.get("storage_identity") == expected_identity:
                        with urllib.request.urlopen(f"http://127.0.0.1:{ui_port}/") as response:
                            verified = b"Muteki" in response.read()
                        if verified:
                            break
                except (OSError, urllib.error.URLError, json.JSONDecodeError):
                    pass
                time.sleep(0.2)
        finally:
            process.terminate()
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if verified:
            return
    tail = "\n".join(log_path.read_text(errors="replace").splitlines()[-40:])
    raise RuntimeError(f"升级后的原生 Web 没有使用迁移状态目录启动：\n{tail}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", default="0.5.0")
    parser.add_argument("--assets", type=Path, required=True)
    args = parser.parse_args()

    version = args.version.removeprefix("v")
    root = Path(__file__).resolve().parents[1]
    assets = args.assets.resolve()
    with tempfile.TemporaryDirectory(prefix="muteki-legacy-upgrade-") as temporary:
        work = Path(temporary)
        legacy_manifest_path = work / "v0.4.7-release-manifest.json"
        legacy_archive_path = work / "muteki-0.4.7.tar.gz"
        download(f"{LEGACY_RELEASE_ROOT}/release-manifest.json", legacy_manifest_path)
        download(f"{LEGACY_RELEASE_ROOT}/muteki-0.4.7.tar.gz", legacy_archive_path)
        legacy_manifest = json.loads(legacy_manifest_path.read_text(encoding="utf-8"))
        if legacy_manifest.get("version") != "0.4.7":
            raise RuntimeError("公开旧版清单不是 v0.4.7")
        artifact = legacy_manifest.get("artifact")
        if (
            not isinstance(artifact, dict)
            or sha256(legacy_archive_path) != artifact.get("sha256")
            or legacy_archive_path.stat().st_size != artifact.get("size")
        ):
            raise RuntimeError("公开 v0.4.7 归档与其发布清单不匹配")

        legacy_extract = work / "legacy"
        legacy_extract.mkdir()
        legacy_source = extract_archive(legacy_archive_path, legacy_extract)
        serve = work / "serve"
        serve.mkdir()
        native_assets = []
        for path in assets.rglob(f"Muteki-Web-{version}-*.tar.gz"):
            target = serve / path.name
            if target.exists():
                raise RuntimeError(f"发现重名发布包：{path.name}")
            shutil.copy2(path, target)
            native_assets.append(target)
        if not native_assets:
            raise RuntimeError(f"{assets} 中没有 {version} 原生 Web 发布包")

        handler = functools.partial(QuietHandler, directory=str(serve))
        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            origin = f"http://127.0.0.1:{server.server_port}"
            subprocess.run(
                [
                    sys.executable,
                    str(root / "scripts" / "package-legacy-upgrade.py"),
                    "--version",
                    version,
                    "--assets",
                    str(serve),
                    "--output",
                    str(serve),
                    "--asset-base-url",
                    origin,
                ],
                cwd=root,
                check=True,
            )
            install = work / "install"
            bin_dir = work / "bin"
            source_environment = os.environ.copy()
            source_environment["PYTHONPATH"] = str(legacy_source)
            source_environment["MUTEKI_BIN_DIR"] = str(bin_dir)
            legacy_install = run_json(
                [
                    sys.executable,
                    "-m",
                    "muteki.cli",
                    "install",
                    "v0.4.7",
                    "--json",
                    "--root",
                    str(install),
                    "--manifest-url",
                    f"{LEGACY_RELEASE_ROOT}/release-manifest.json",
                ],
                cwd=legacy_source,
                env=source_environment,
            )
            if legacy_install.get("status") != "installed" or legacy_install.get("current_version") != "0.4.7":
                raise RuntimeError(f"无法建立正式 v0.4.7 托管安装：{legacy_install}")
            launcher_environment = os.environ.copy()
            launcher_environment["MUTEKI_INSTALL_ROOT"] = str(install)
            subprocess.run(
                ["uv", "sync", "--frozen", "--no-dev"],
                cwd=legacy_source,
                check=True,
            )
            initialize_legacy_install(legacy_source, install, work, launcher_environment)
            legacy_metadata = json.loads((install / "install.json").read_text(encoding="utf-8"))
            legacy_state = Path(legacy_metadata["state_root"]).resolve()
            worker_config = legacy_state / "_worker_config.json"
            config = json.loads(worker_config.read_text(encoding="utf-8")) if worker_config.is_file() else {}
            credentials = config.setdefault("credentials", [])
            credentials.append({"id": "legacy-upgrade-probe", "label": "Legacy upgrade probe"})
            config.setdefault("seats", [])
            config.setdefault("worker_profiles", [])
            worker_config.write_text(json.dumps(config, ensure_ascii=False) + "\n", encoding="utf-8")
            legacy_running = run_json(
                [str(bin_dir / "muteki"), "version", "--json"],
                cwd=work,
                env=launcher_environment,
            )
            if legacy_running.get("version") != "0.4.7":
                raise RuntimeError(f"托管安装没有运行正式 v0.4.7：{legacy_running}")
            upgrade = run_json(
                [
                    str(bin_dir / "muteki"),
                    "upgrade",
                    f"v{version}",
                    "--json",
                    "--manifest-url",
                    f"{origin}/release-manifest.json",
                ],
                cwd=work,
                env=launcher_environment,
            )
            if upgrade.get("status") != "installed" or upgrade.get("current_version") != version:
                raise RuntimeError(f"v0.4.7 更新器没有安装 {version}：{upgrade}")
            installed = run_json(
                [str(bin_dir / "muteki"), "version", "--json"],
                cwd=work,
                env=launcher_environment,
            )
            if installed.get("version") != version:
                raise RuntimeError(f"升级后的命令没有运行 {version}：{installed}")
            current = (install / "current").resolve()
            native = current / "native"
            if not (native / "bin" / "muteki-server").is_file() or not (native / "frontend" / "server.js").is_file():
                raise RuntimeError("升级后的托管安装缺少原生服务或前端")
            migrated_metadata = json.loads((install / "install.json").read_text(encoding="utf-8"))
            native_state = Path(migrated_metadata["native_state_root"]).resolve()
            if Path(migrated_metadata["state_root"]).resolve() != legacy_state or native_state == legacy_state:
                raise RuntimeError("升级没有同时保留旧数据根和独立的 Rust 状态目录")
            with sqlite3.connect(native_state / "muteki.sqlite") as database:
                imported = database.execute(
                    "SELECT value FROM documents WHERE collection='credentials' AND id='legacy-upgrade-probe'"
                ).fetchone()
            if imported is None or json.loads(imported[0]).get("label") != "Legacy upgrade probe":
                raise RuntimeError("v0.4.7 配置没有导入 Rust 状态库")
            subprocess.run(
                [str(bin_dir / "muteki"), "rollback"],
                cwd=work,
                env=launcher_environment,
                check=True,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            rolled_back = run_json(
                [str(bin_dir / "muteki"), "version", "--json"],
                cwd=work,
                env=launcher_environment,
            )
            if rolled_back.get("version") != "0.4.7":
                raise RuntimeError(f"回滚后的稳定命令没有运行 v0.4.7：{rolled_back}")
            repeated = run_json(
                [
                    str(bin_dir / "muteki"),
                    "upgrade",
                    f"v{version}",
                    "--json",
                    "--manifest-url",
                    f"{origin}/release-manifest.json",
                ],
                cwd=work,
                env=launcher_environment,
            )
            if repeated.get("status") != "installed" or repeated.get("current_version") != version:
                raise RuntimeError(f"回滚后的重复升级没有安装 {version}：{repeated}")
            verify_native_web(bin_dir / "muteki", install, native_state, work, launcher_environment)
            print(
                json.dumps(
                    {
                        "legacy_version": "0.4.7",
                        "installed_version": installed["version"],
                        "platform_package": json.loads((native / "release.json").read_text(encoding="utf-8")),
                        "direct_command_upgrade": True,
                        "legacy_data_import": True,
                        "managed_web_startup": True,
                        "rollback_to_0.4.7": True,
                        "repeat_upgrade": True,
                    },
                    ensure_ascii=False,
                    indent=2,
                )
            )
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
