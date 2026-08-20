#!/usr/bin/env python3
"""构建可由 ``muteki upgrade`` 安装的版本化应用包和发布清单。"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
from datetime import datetime, timezone
from pathlib import Path


def command(*arguments: str, cwd: Path) -> str:
    return subprocess.check_output(arguments, cwd=cwd, text=True).strip()


def tracked_files(root: Path) -> list[Path]:
    raw = subprocess.check_output(["git", "ls-files", "-z"], cwd=root)
    return [Path(value.decode("utf-8")) for value in raw.split(b"\0") if value]


def copy_source(root: Path, destination: Path) -> None:
    excluded_roots = {"dist", ".git", ".venv", "sessions", "node_modules", ".next"}
    for relative in tracked_files(root):
        if any(part in excluded_roots for part in relative.parts):
            continue
        source = root / relative
        target = destination / relative
        if source.is_symlink():
            resolved = source.resolve()
            if not resolved.is_file():
                raise RuntimeError(f"发布文件中的符号链接无法解析：{relative}")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(resolved, target)
        elif source.is_file():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", type=Path, default=Path("dist"))
    parser.add_argument("--repository", default="FishCodeTech/muteki")
    args = parser.parse_args()

    root = Path(__file__).resolve().parents[1]
    version = args.version.removeprefix("v")
    if not version or any(character not in "0123456789.-+abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ" for character in version):
        raise SystemExit("版本号包含不允许的字符")
    output = args.output if args.output.is_absolute() else root / args.output
    output.mkdir(parents=True, exist_ok=True)
    commit = command("git", "rev-parse", "HEAD", cwd=root)
    built_at = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    release_name = f"muteki-{version}"
    archive_path = output / f"{release_name}.tar.gz"

    with tempfile.TemporaryDirectory(prefix="muteki-release-") as temporary:
        staging = Path(temporary) / release_name
        staging.mkdir()
        copy_source(root, staging)
        release = {
            "schema_version": 1,
            "version": version,
            "commit": commit,
            "built_at": built_at,
        }
        (staging / ".muteki-release.json").write_text(
            json.dumps(release, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        with tarfile.open(archive_path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
            archive.add(staging, arcname=release_name, recursive=True)

    tag = f"v{version}"
    owner = args.repository.split("/", 1)[0].lower()
    manifest = {
        "schema_version": 1,
        "version": version,
        "commit": commit,
        "published_at": built_at,
        "minimum_updater_version": "0.3.0",
        "artifact": {
            "url": archive_path.name,
            "sha256": sha256(archive_path),
            "size": archive_path.stat().st_size,
        },
        "images": {
            "web": f"ghcr.io/{owner}/muteki-web:{tag}",
            "ui": f"ghcr.io/{owner}/muteki-ui:{tag}",
            "worker": f"ghcr.io/{owner}/muteki-worker:{tag}",
            "worker_slim": f"ghcr.io/{owner}/muteki-worker-slim:{tag}",
        },
    }
    manifest_path = output / "release-manifest.json"
    manifest_path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(archive_path)
    print(manifest_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
