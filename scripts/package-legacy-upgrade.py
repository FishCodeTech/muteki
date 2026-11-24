#!/usr/bin/env python3
"""Build the platform-neutral bridge consumed by the Python 0.4.7 updater."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ASSET_PATTERN = re.compile(
    r"^Muteki-Web-(?P<version>\d+\.\d+\.\d+)-"
    r"(?P<platform>darwin|linux)-(?P<arch>arm64|amd64)\.tar\.gz$"
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def git_commit(root: Path) -> str:
    result = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=root,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    return result.stdout.strip()


def discover_assets(directory: Path, version: str, base_url: str) -> dict[str, dict[str, Any]]:
    platforms: dict[str, dict[str, Any]] = {}
    for path in sorted(directory.rglob("Muteki-Web-*.tar.gz")):
        match = ASSET_PATTERN.fullmatch(path.name)
        if not match or match.group("version") != version:
            continue
        key = f"{match.group('platform')}-{match.group('arch')}"
        if key in platforms:
            raise RuntimeError(f"发现重复的 {key} 原生发布包")
        platforms[key] = {
            "url": f"{base_url.rstrip('/')}/{path.name}",
            "sha256": sha256(path),
            "size": path.stat().st_size,
        }
    if not platforms:
        raise RuntimeError(f"{directory} 中没有 Muteki-Web-{version} 原生发布包")
    return platforms


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--assets", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repository", default="FishCodeTech/muteki")
    parser.add_argument("--asset-base-url")
    args = parser.parse_args()

    version = args.version.removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise SystemExit(f"版本号无效：{args.version}")
    root = Path(__file__).resolve().parents[1]
    assets = args.assets.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    tag = f"v{version}"
    base_url = args.asset_base_url or f"https://github.com/{args.repository}/releases/download/{tag}"
    platforms = discover_assets(assets, version, base_url)
    commit = git_commit(root)
    built_at = datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")
    archive_name = f"muteki-{version}-upgrade.tar.gz"
    archive_path = output / archive_name
    bridge_source = root / "packages" / "legacy-upgrade-bridge"

    with tempfile.TemporaryDirectory(prefix="muteki-upgrade-bridge-") as temporary:
        staging = Path(temporary) / f"muteki-{version}"
        shutil.copytree(bridge_source, staging)
        os.chmod(staging / "scripts" / "muteki-launcher", 0o755)
        (staging / "native-assets.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "version": version,
                    "platforms": platforms,
                },
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        (staging / ".muteki-release.json").write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "version": version,
                    "commit": commit,
                    "built_at": built_at,
                },
                ensure_ascii=False,
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        with tarfile.open(archive_path, "w:gz", format=tarfile.PAX_FORMAT) as archive:
            archive.add(staging, arcname=staging.name, recursive=True)

    owner = args.repository.split("/", 1)[0].lower()
    manifest = {
        "schema_version": 1,
        "version": version,
        "commit": commit,
        "published_at": built_at,
        "minimum_updater_version": "0.3.0",
        "artifact": {
            "url": archive_name,
            "sha256": sha256(archive_path),
            "size": archive_path.stat().st_size,
        },
        "images": {
            "web": f"ghcr.io/{owner}/muteki-web:{tag}",
            "worker": f"ghcr.io/{owner}/muteki-worker:{tag}",
        },
    }
    manifest_path = output / "release-manifest.json"
    manifest_path.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    print(json.dumps({"archive": str(archive_path), "manifest": str(manifest_path), "platforms": sorted(platforms)}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
