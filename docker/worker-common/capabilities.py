#!/usr/bin/env python3
"""Build-time Worker inventory. Only successful launches count as capabilities."""
import argparse
import json
import os
import pathlib
import platform
import pwd
import re
import shutil
import subprocess


def probe(binary):
    path = shutil.which(binary)
    if not path:
        return {"available": False, "path": None, "error": "not installed"}
    try:
        result = subprocess.run([path, "--version"], capture_output=True, text=True,
                                timeout=60, check=False)
        version = (result.stdout + result.stderr).strip()
        return {"available": result.returncode == 0 and bool(version), "path": path,
                "version": version, "exit_code": result.returncode}
    except (OSError, subprocess.TimeoutExpired) as error:
        return {"available": False, "path": path, "error": str(error)}


def version_key(engine, raw):
    if engine == "cursor":
        match = re.search(r"\b(\d{4})\.(\d{2})\.(\d{2})(?:-[a-z0-9]+)?\b", raw, re.I)
        if match:
            return tuple(int(part) for part in match.groups())
    else:
        match = re.search(r"\d+(?:\.\d+){1,3}(?:(?:a|b|alpha|beta|rc|dev)\d+)?(?:-[0-9a-z][0-9a-z._-]*)?", raw, re.I)
        if match and re.fullmatch(r"\d+(?:\.\d+){1,3}", match[0]):
            parts = tuple(int(part) for part in match[0].split("."))
            return parts + (0,) * (4 - len(parts))
    raise ValueError(f"{engine}: unsupported stable version {raw!r}")


def check_versions(engines, minimums, inventory=None):
    errors = []
    for row in engines:
        engine = row["engine"]
        minimum = minimums[engine]
        pinned = row["version"]
        if version_key(engine, pinned) < version_key(engine, minimum):
            errors.append(f"{engine}: pinned {pinned} is below minimum {minimum}")
        if inventory is not None:
            installed = inventory[engine]["version"]
            if version_key(engine, installed) < version_key(engine, minimum):
                errors.append(f"{engine}: installed {installed!r} is below minimum {minimum}")
    if errors:
        raise SystemExit("Worker version admission failed:\n" + "\n".join(errors))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--registry", default="/opt/muteki/engines.json")
    parser.add_argument("--output", default="/opt/muteki/IMAGE_CAPABILITIES.json")
    parser.add_argument("--minimum-versions", default="/opt/muteki/minimum_versions.json")
    parser.add_argument("--engine-list", action="store_true")
    parser.add_argument("--check-pins", action="store_true")
    parser.add_argument("--check-only", action="store_true")
    parser.add_argument("--versions-file")
    args = parser.parse_args()
    engines = json.loads(pathlib.Path(args.registry).read_text())
    if not engines:
        raise SystemExit("Worker engine registry is empty")
    if args.engine_list:
        for row in engines:
            print(f"{row['engine']}:{row['binary']}")
        return
    minimums = json.loads(pathlib.Path(args.minimum_versions).read_text())
    check_versions(engines, minimums)
    if args.check_pins:
        print("Worker version pins meet the runtime minimum versions")
        return
    inventory = {row["engine"]: probe(row["binary"]) for row in engines}
    failed = [name for name, result in inventory.items() if not result["available"]]
    if failed:
        raise SystemExit("Required Worker CLIs failed: " + json.dumps({name: inventory[name] for name in failed}))
    check_versions(engines, minimums, inventory)
    for name, result in inventory.items():
        print(f"OK engine {name}: {result['version']} (minimum {minimums[name]})")
    if args.versions_file:
        pathlib.Path(args.versions_file).write_text("".join(
            f"{name}={result['version'].replace(chr(10), ' ')}\n" for name, result in inventory.items()
        ))
    if args.check_only:
        return
    user = pwd.getpwnam("kali")
    browser = {name: probe(name) for name in ["agent-browser", "chromium"]}
    if any(not result["available"] for result in browser.values()):
        raise SystemExit("Required browser components failed: " + json.dumps(browser))
    manifest = {"schema_version": 1, "architecture": platform.machine(),
                "users": {"root": {"uid": 0, "gid": 0},
                          "kali": {"uid": user.pw_uid, "gid": user.pw_gid}},
                "execution_user": {"name": "root", "uid": 0, "gid": 0},
                "supervisor": "/opt/muteki/runtime_agent", "engines": inventory,
                "browser": browser, "tools": {name: shutil.which(name) for name in
                ["python3", "curl", "git", "jq", "rg", "nmap", "gdb", "sage"]}}
    output = pathlib.Path(args.output)
    temporary = output.with_suffix(".tmp")
    temporary.write_text(json.dumps(manifest, indent=2) + "\n")
    os.replace(temporary, output)


if __name__ == "__main__":
    main()
