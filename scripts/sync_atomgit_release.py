#!/usr/bin/env python3
"""Mirror a published GitHub release to AtomGit using only the standard library.

--dry-run validates local assets and compares AtomGit without making changes.
Attachments are immutable: a same-name SHA-256 conflict fails before any write.
API reference: https://docs.atomgit.com/docs/apis/post-api-v-5-repos-owner-repo-releases/
"""
from __future__ import annotations

import argparse
from contextlib import nullcontext
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tarfile
import tempfile
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener


class SyncError(Exception):
    pass


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class ApiRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        if urlsplit(new_url).scheme != "https":
            raise SyncError("AtomGit redirected to a non-HTTPS URL")
        redirected = super().redirect_request(request, response, code, message, headers, new_url)
        if redirected and urlsplit(request.full_url).netloc != urlsplit(new_url).netloc:
            redirected.remove_header("Private-token")
        return redirected


class AtomGit:
    def __init__(self, repository: str, tag: str, token: str):
        self.root = "https://api.atomgit.com/api/v5/repos/" + "/".join(
            quote(part, safe="") for part in repository.split("/")
        ) + "/releases"
        self.tag = quote(tag, safe="")
        self.release = self.root + "/" + self.tag
        self.token = token
        self.opener = build_opener(ApiRedirectHandler())

    def request(self, method: str, url: str, *, payload=None, path=None,
                headers=None, authenticated=True, missing_ok=False):
        request_headers = dict(headers or {})
        if authenticated and self.token:
            request_headers["PRIVATE-TOKEN"] = self.token
        if payload is not None:
            request_headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode() if payload is not None else None
        try:
            # Explicit Content-Length lets urllib stream large desktop packages.
            with path.open("rb") if path else nullcontext() as stream:
                if path:
                    data = stream
                    request_headers["Content-Length"] = str(path.stat().st_size)
                with self.opener.open(Request(url, data=data, headers=request_headers, method=method), timeout=300) as response:
                    if method == "PUT" or response.status == 204:
                        return None
                    return json.load(response)
        except HTTPError as error:
            if missing_ok and error.code == 404:
                return None
            # URLs, response bodies and signed upload headers can contain secrets.
            raise SyncError(f"AtomGit {method} failed (HTTP {error.code})") from None
        except (OSError, URLError, ValueError):
            raise SyncError(f"AtomGit {method} failed (network or response error)") from None

    def get(self):
        return self.request("GET", self.root + "/tags/" + self.tag, missing_ok=True)

    def download(self, name: str, destination: Path):
        url = self.release + "/attach_files/" + quote(name, safe="") + "/download"
        try:
            request = Request(url, headers={"PRIVATE-TOKEN": self.token} if self.token else {})
            with self.opener.open(request, timeout=300) as response, destination.open("wb") as output:
                shutil.copyfileobj(response, output, 1024 * 1024)
        except (OSError, URLError):
            raise SyncError(f"Could not download AtomGit attachment {name}") from None

    def upload(self, name: str, path: Path):
        signed = self.request("GET", self.release + "/upload_url?" + urlencode({"file_name": name}))
        if not isinstance(signed, dict) or not isinstance(signed.get("headers"), dict) or urlsplit(signed.get("url", "")).scheme != "https":
            raise SyncError("AtomGit returned an invalid upload address")
        self.request("PUT", signed["url"], path=path, headers=signed["headers"], authenticated=False)


def assets_by_name(release: dict) -> dict:
    assets = {}
    for asset in release.get("assets") or []:
        if str(asset.get("type", "")).lower() == "source":
            continue
        name = asset["name"]
        if name in assets:
            raise SyncError(f"AtomGit contains duplicate attachments: {name}")
        assets[name] = asset
    return assets


def verify_file(path: Path, metadata: dict, label: str) -> None:
    if path.stat().st_size != metadata.get("size"):
        raise SyncError(f"{label} size mismatch: {path.name}")
    digest = metadata.get("sha256")
    if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise SyncError(f"{label} SHA-256 is invalid: {path.name}")
    if sha256(path) != digest:
        raise SyncError(f"{label} SHA-256 mismatch: {path.name}")


def verify_checksum(checksum: Path, asset: Path) -> None:
    fields = checksum.read_text(encoding="utf-8").strip().split()
    if len(fields) != 2 or fields[1].lstrip("*") != asset.name:
        raise SyncError(f"Invalid checksum file: {checksum.name}")
    if fields[0] != sha256(asset):
        raise SyncError(f"Checksum mismatch: {asset.name}")


def verify_rust_bundle(files: dict[str, Path], version: str, commit: str) -> set[str]:
    manifest_name = "release-manifest.json"
    upgrade_name = f"muteki-{version}-upgrade.tar.gz"
    desktop_name = f"Muteki-{version}-arm64.zip"
    targets = ("darwin-arm64", "linux-amd64", "linux-arm64")
    native_names = {
        target: f"Muteki-Web-{version}-{target}.tar.gz"
        for target in targets
    }
    expected = {manifest_name, upgrade_name, desktop_name, desktop_name + ".sha256"}
    expected.update(native_names.values())
    expected.update(name + ".sha256" for name in native_names.values())
    if set(files) != expected:
        raise SyncError("Expected the Rust Web, macOS and legacy-upgrade release bundle")

    manifest = json.loads(files[manifest_name].read_text(encoding="utf-8"))
    if (
        manifest.get("schema_version") != 1
        or manifest.get("version") != version
        or manifest.get("commit") != commit
    ):
        raise SyncError("Rust release manifest identity does not match the release tag")
    artifact = manifest.get("artifact")
    if not isinstance(artifact, dict) or Path(urlsplit(str(artifact.get("url") or "")).path).name != upgrade_name:
        raise SyncError("Rust release manifest does not reference the upgrade bridge")
    verify_file(files[upgrade_name], artifact, "Upgrade bridge")
    expected_images = {
        "web": f"ghcr.io/fishcodetech/muteki-web:v{version}",
        "worker": f"ghcr.io/fishcodetech/muteki-worker:v{version}",
    }
    if manifest.get("images") != expected_images:
        raise SyncError("Rust release manifest image tags do not match the release version")

    with tarfile.open(files[upgrade_name], "r:gz") as archive:
        members = [
            member for member in archive.getmembers()
            if member.isfile()
            and len(Path(member.name).parts) == 2
            and Path(member.name).name == "native-assets.json"
        ]
        if len(members) != 1:
            raise SyncError("Upgrade bridge must contain one native-assets.json")
        stream = archive.extractfile(members[0])
        if stream is None:
            raise SyncError("Could not read native-assets.json from the upgrade bridge")
        native = json.load(stream)
    platforms = native.get("platforms") if isinstance(native, dict) else None
    if (
        not isinstance(native, dict)
        or native.get("schema_version") != 1
        or native.get("version") != version
        or not isinstance(platforms, dict)
    ):
        raise SyncError("Upgrade bridge native asset metadata is invalid")
    if set(platforms) != set(targets):
        raise SyncError("Upgrade bridge does not cover every supported native platform")
    for target, name in native_names.items():
        metadata = platforms[target]
        if not isinstance(metadata, dict) or Path(urlsplit(str(metadata.get("url") or "")).path).name != name:
            raise SyncError(f"Upgrade bridge has an invalid {target} asset URL")
        verify_file(files[name], metadata, f"Native {target} package")
        verify_checksum(files[name + ".sha256"], files[name])
    verify_checksum(files[desktop_name + ".sha256"], files[desktop_name])
    return expected


def sync(api: AtomGit, source: dict, files: list[Path], commit: str, dry_run: bool):
    release = api.get()
    existing = assets_by_name(release or {})
    unexpected = sorted(set(existing) - {path.name for path in files})
    if unexpected:
        raise SyncError(
            "Unexpected AtomGit attachments require manual audit: "
            + ", ".join(unexpected)
        )
    payload = {
        "name": source.get("name") or source["tag_name"],
        "body": source.get("body") or "",
        "release_status": "pre" if source.get("prerelease") else "latest",
    }
    with tempfile.TemporaryDirectory(prefix="muteki-atomgit-") as directory:
        downloaded = Path(directory) / "attachment"
        pending = []
        # Check every collision before changing either metadata or attachments.
        for path in files:
            digest = sha256(path)
            if path.name in existing:
                api.download(path.name, downloaded)
                if sha256(downloaded) != digest:
                    raise SyncError(f"Immutable attachment conflict: {path.name}; SHA-256 differs. Audit the release manually before retrying")
                print(f"Skip {path.name}: SHA-256 matches")
            else:
                print(f"{'Would upload' if dry_run else 'Upload planned'} {path.name}")
                pending.append((path, digest))
        metadata_changed = not release or any(release.get(key) != value for key, value in payload.items())
        if dry_run:
            print(f"Dry run: {'create' if not release else 'update' if metadata_changed else 'keep'} release; {len(pending)} attachment uploads")
            return
        if not release:
            api.request("POST", api.root, payload={**payload, "tag_name": source["tag_name"], "target_commitish": commit})
        elif metadata_changed:
            api.request("PATCH", api.release, payload=payload)
        for path, digest in pending:
            api.upload(path.name, path)
            api.download(path.name, downloaded)
            if sha256(downloaded) != digest:
                raise SyncError(f"Uploaded attachment checksum mismatch: {path.name}")
            print(f"Verified {path.name}: SHA-256 matches")
    print(f"AtomGit release {source['tag_name']} synchronized ({len(files)} attachments)")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-json", required=True, type=Path, help="GitHub REST release response")
    parser.add_argument("--assets-dir", required=True, type=Path)
    parser.add_argument("--target-commitish", required=True, help="Commit SHA of the mirrored Git tag")
    parser.add_argument("--repository", default="FishCodeTech/muteki")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    token = os.environ.get("ATOMGIT_TOKEN", "").strip()
    if not token and not args.dry_run:
        raise SyncError("ATOMGIT_TOKEN is required")
    if not re.fullmatch(r"[^/\s]+/[^/\s]+", args.repository):
        raise SyncError("Repository must be owner/repo")
    source = json.loads(args.release_json.read_text())
    tag = source.get("tag_name", "")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", tag) or source.get("draft"):
        raise SyncError("Expected a published versioned GitHub release")
    version = tag[1:]
    source_assets = {asset["name"]: asset for asset in source.get("assets", [])}
    files = sorted(args.assets_dir.iterdir())
    files_by_name = {path.name: path for path in files}
    if len(source_assets) != len(source.get("assets", [])) or any(not path.is_file() for path in files):
        raise SyncError("Release attachments must have unique names and contain only files")
    legacy_expected = {f"muteki-{version}.tar.gz", "release-manifest.json", "image-identity.json"} | {
        f"Muteki-{version}-{arch}.{extension}" for arch in ("arm64", "x64") for extension in ("dmg", "zip")
    }
    if f"muteki-{version}-upgrade.tar.gz" in source_assets:
        expected = verify_rust_bundle(files_by_name, version, args.target_commitish)
    else:
        expected = legacy_expected
    if set(source_assets) != expected or set(files_by_name) != expected:
        raise SyncError("GitHub release attachments do not match the supported release bundle")
    for path in files:
        asset = source_assets[path.name]
        if path.stat().st_size != asset["size"]:
            raise SyncError(f"GitHub attachment size mismatch: {path.name}")
        if asset.get("digest") and asset["digest"] != "sha256:" + sha256(path):
            raise SyncError(f"GitHub attachment checksum mismatch: {path.name}")
    sync(AtomGit(args.repository, tag, token), source, files, args.target_commitish, args.dry_run)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (SyncError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print(f"Release sync failed: {error}", file=sys.stderr)
        raise SystemExit(1)
