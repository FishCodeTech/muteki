#!/usr/bin/env python3
"""Build a relocatable desktop runtime from locked dependencies and source."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
import zipfile


ROOT = Path(__file__).resolve().parents[1]


def run(*args: str, **kwargs):
    return subprocess.run(args, cwd=ROOT, check=True, **kwargs)


def node_distribution(build: Path, version: str) -> tuple[Path, str]:
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError("Node version must be an exact release number")
    system = {"Darwin": "darwin", "Windows": "win", "Linux": "linux"}[platform.system()]
    arch = {"aarch64": "arm64", "arm64": "arm64", "x86_64": "x64", "AMD64": "x64"}[platform.machine()]
    name = f"node-v{version}-{system}-{arch}"
    archive_name = name + (".zip" if os.name == "nt" else ".tar.gz")
    base = f"https://nodejs.org/dist/v{version}/"
    with urllib.request.urlopen(base + "SHASUMS256.txt", timeout=60) as response:
        sums = dict(line.split()[::-1] for line in response.read().decode().splitlines() if line.strip())
    expected = sums[archive_name]
    cache = build / "node-source"
    cache.mkdir(parents=True, exist_ok=True)
    archive = cache / archive_name
    if not archive.exists() or hashlib.sha256(archive.read_bytes()).hexdigest() != expected:
        temporary = archive.with_suffix(archive.suffix + ".partial")
        with urllib.request.urlopen(base + archive_name, timeout=60) as response, temporary.open("wb") as output:
            shutil.copyfileobj(response, output)
        if hashlib.sha256(temporary.read_bytes()).hexdigest() != expected:
            temporary.unlink()
            raise ValueError("Official Node distribution checksum mismatch")
        temporary.replace(archive)
    target = cache / name
    if target.exists():
        shutil.rmtree(target)
    if os.name == "nt":
        with zipfile.ZipFile(archive) as source:
            for entry in source.infolist():
                if not (cache / entry.filename).resolve().is_relative_to(target):
                    raise ValueError("Node archive path escapes its distribution")
            source.extractall(cache)
    else:
        with tarfile.open(archive) as source:
            source.extractall(cache, filter="data")
    return target, expected


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--python-version", default="3.13.13")
    parser.add_argument("--node-version", default="24.21.0")
    parser.add_argument("--allow-dirty", action="store_true")
    parser.add_argument("--next-dir", default=".next-desktop")
    parser.add_argument("--skip-web-build", action="store_true")
    args = parser.parse_args()
    dirty = run("git", "status", "--porcelain", capture_output=True, text=True).stdout
    if dirty and not args.allow_dirty:
        raise SystemExit("Commit source before building a release; --allow-dirty is for local acceptance only")
    build = ROOT / "apps/desktop/build"
    if not args.skip_web_build:
        env = dict(os.environ, MUTEKI_NEXT_DIST_DIR=args.next_dir, NEXT_PUBLIC_MUTEKI_API="", NEXT_TELEMETRY_DISABLED="1")
        declarations = ROOT / "apps/web/ui/next-env.d.ts"
        previous = declarations.read_text()
        try:
            subprocess.run(["npm", "run", "build"], cwd=ROOT / "apps/web/ui", env=env, check=True)
        finally:
            # Next rewrites this generated reference for an alternate distDir.
            # Preserve the normal Web development entry after a desktop build.
            current = declarations.read_text()
            expected = previous.replace('./.next/types/routes.d.ts', f'./{args.next_dir}/types/routes.d.ts')
            if current == expected:
                declarations.write_text(previous)
    destination = build / "runtime"
    source = build / "python-source"
    source.mkdir(parents=True, exist_ok=True)
    run("uv", "python", "install", args.python_version, "--install-dir", str(source), "--no-bin")
    installations = [item for item in source.iterdir() if item.is_dir() and item.name.startswith(f"cpython-{args.python_version}-")]
    if len(installations) != 1:
        raise SystemExit("Expected exactly one Python distribution for this build host")
    staging = build / "runtime.staging"
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir()
    shutil.copytree(installations[0], staging / "python", symlinks=True)
    python = staging / "python" / ("python.exe" if os.name == "nt" else f"bin/python{args.python_version.rsplit('.', 1)[0]}")
    requirements = build / "desktop-requirements.txt"
    run("uv", "export", "--frozen", "--no-dev", "--no-emit-project", "--output-file", str(requirements), stdout=subprocess.DEVNULL)
    run("uv", "pip", "install", "--python", str(python), "--break-system-packages", "--link-mode", "copy", "--require-hashes", "-r", str(requirements))
    if os.name != "nt":
        for entry in (staging / "python/bin").iterdir():
            if entry.is_symlink() or not entry.is_file():
                continue
            content = entry.read_bytes()
            if content.startswith(b"#!") and str(staging).encode() in content.split(b"\n", 1)[0]:
                entry.write_bytes(b"#!/usr/bin/env python3\n" + content.split(b"\n", 1)[1])
    code = staging / "app"
    excluded = {".git", ".venv", "node_modules", ".next", "state", "sessions", "dist", "build", "__pycache__"}
    files = run("git", "ls-files", "--cached", "--others", "--exclude-standard", "-z", capture_output=True).stdout.split(b"\0")
    hashes = {}
    for raw in files:
        if not raw:
            continue
        relative = Path(os.fsdecode(raw))
        if any(part in excluded for part in relative.parts):
            continue
        if relative.parts[0] not in {"muteki", "apps", "scripts", "prompts", "config", "configs"} and str(relative) not in {"pyproject.toml", "uv.lock", "LICENSE"}:
            continue
        if relative.parts[:3] == ("apps", "web", "ui"):
            continue
        source_file = ROOT / relative
        if not source_file.is_file():
            continue
        target = code / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source_file, target)
        hashes[str(relative)] = hashlib.sha256(target.read_bytes()).hexdigest()
    commit = run("git", "rev-parse", "HEAD", capture_output=True, text=True).stdout.strip()
    code_digest = hashlib.sha256(json.dumps(hashes, sort_keys=True).encode()).hexdigest()
    release = {"version": json.loads((ROOT / "apps/desktop/package.json").read_text())["version"], "commit": commit, "source_sha256": code_digest, "dirty": bool(dirty)}
    (code / ".muteki-release.json").write_text(json.dumps(release) + "\n")
    next_build = ROOT / "apps/web/ui" / args.next_dir
    shutil.copytree(next_build / "standalone", staging / "web", symlinks=False, ignore=shutil.ignore_patterns(".env", ".env.*"))
    if args.next_dir != ".next":
        (staging / "web" / args.next_dir).rename(staging / "web/.next")
    shutil.copytree(next_build / "static", staging / "web/.next/static", dirs_exist_ok=True)
    shutil.copytree(ROOT / "apps/web/ui/public", staging / "web/public", dirs_exist_ok=True)
    # Production caches are created in the environment, never shipped from a build.
    for cache in (staging / "web/.next/cache", staging / "web/.next/trace"):
        if cache.is_dir():
            shutil.rmtree(cache)
        elif cache.exists():
            cache.unlink()
    distribution, node_sha256 = node_distribution(build, args.node_version)
    node = staging / ("node.exe" if os.name == "nt" else "node")
    shutil.copy2(distribution / ("node.exe" if os.name == "nt" else "bin/node"), node)
    shutil.copy2(distribution / "LICENSE", staging / "node-LICENSE.txt")
    npm_root = distribution / ("node_modules/npm" if os.name == "nt" else "lib/node_modules/npm")
    shutil.copytree(npm_root, staging / "npm-package", symlinks=False)
    if os.name != "nt":
        for command, entry in (("npm", "npm-cli.js"), ("npx", "npx-cli.js")):
            launcher = staging / command
            launcher.write_text('#!/bin/sh\nexec "$(dirname "$0")/node" "$(dirname "$0")/npm-package/bin/' + entry + '" "$@"\n')
            launcher.chmod(0o755)
    else:
        for command, entry in (("npm", "npm-cli.js"), ("npx", "npx-cli.js")):
            (staging / f"{command}.cmd").write_text(f'@"%~dp0node.exe" "%~dp0npm-package\\bin\\{entry}" %*\r\n')
    manifest = {"version": 1, "platform": {"Darwin": "darwin", "Windows": "win32", "Linux": "linux"}[platform.system()],
                "arch": {"aarch64": "arm64", "arm64": "arm64", "x86_64": "x64", "AMD64": "x64"}[platform.machine()],
                "code": "app", "python": str(python.relative_to(staging)), "node": node.name, "next": "web/server.js",
                "release": f"{release['version']}+{code_digest[:12]}", "source": release,
                "nodeVersion": subprocess.check_output([str(node), "--version"], text=True).strip(),
                "nodeArchiveSha256": node_sha256}
    (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    # Relocation is part of every build, not an assumption about a copied venv.
    if destination.exists():
        shutil.rmtree(destination)
    staging.rename(destination)
    subprocess.run([str(destination / manifest["python"]), "-I", "-B", "-c", "import fastapi, uvicorn, pydantic, sqlite3, numpy; from magika import Magika; Magika(); print('Desktop Python runtime ready')"], cwd=build, check=True)
    subprocess.run([str(destination / manifest["node"]), str(destination / "npm-package/bin/npm-cli.js"), "--version"], cwd=build, check=True)
    print(destination)


if __name__ == "__main__":
    main()
