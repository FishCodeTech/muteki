#!/usr/bin/env python3
"""Mirror a published GitHub release to AtomGit using only the standard library.

Supports:
- Downloading published GitHub release JSON and assets (--fetch-github-repo / --download-only)
- Verifying Rust / legacy release manifests and SHA-256 digests before any write
- High-throughput chunked uploads (1 MiB buffers), concurrent workers, and retries
- Triggering and polling the AtomGit-side pull workflow (--dispatch-workflow)

Attachments are immutable: a same-name SHA-256 conflict fails before any write.
API reference: https://docs.atomgit.com/docs/apis/post-api-v-5-repos-owner-repo-releases/
"""
from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import ssl
import sys
import tarfile
import tempfile
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

CHUNK_BYTES = 1024 * 1024


class SyncError(Exception):
    pass


def log(message: str) -> None:
    print(message, flush=True)


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class ApiRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        if urlsplit(new_url).scheme != "https":
            raise SyncError("Redirected to a non-HTTPS URL")
        redirected = super().redirect_request(request, response, code, message, headers, new_url)
        if redirected and urlsplit(request.full_url).netloc != urlsplit(new_url).netloc:
            redirected.remove_header("Private-token")
            redirected.remove_header("Authorization")
        return redirected


def build_https_opener():
    return build_opener(ApiRedirectHandler())


def put_file_chunked(url: str, path: Path, headers: dict[str, str], timeout: int = 180) -> None:
    parts = urlsplit(url)
    if parts.scheme != "https" or not parts.hostname:
        raise SyncError("Upload URL must be HTTPS")
    size = path.stat().st_size
    target = parts.path or "/"
    if parts.query:
        target = f"{target}?{parts.query}"
    req_headers = dict(headers)
    req_headers["Content-Length"] = str(size)
    context = ssl.create_default_context()
    conn = http.client.HTTPSConnection(
        parts.hostname,
        parts.port or 443,
        timeout=timeout,
        context=context,
        blocksize=CHUNK_BYTES,
    )
    try:
        conn.putrequest("PUT", target)
        for key, value in req_headers.items():
            conn.putheader(key, value)
        conn.endheaders()
        with path.open("rb") as stream:
            while True:
                chunk = stream.read(CHUNK_BYTES)
                if not chunk:
                    break
                conn.send(chunk)
        response = conn.getresponse()
        response.read()
        if response.status < 200 or response.status >= 300:
            raise SyncError(f"AtomGit PUT failed (HTTP {response.status})")
    except SyncError:
        raise
    except (OSError, http.client.HTTPException):
        raise SyncError("AtomGit PUT failed (network or response error)") from None
    finally:
        conn.close()


def download_url(
    url: str,
    destination: Path,
    *,
    headers: dict[str, str] | None = None,
    timeout: int = 180,
    attempts: int = 4,
    label: str = "file",
) -> None:
    opener = build_https_opener()
    last_error: Exception | None = None
    tmp = destination.with_name(destination.name + ".part")
    for attempt in range(1, attempts + 1):
        try:
            req_headers = dict(headers or {})
            mode = "wb"
            existing_bytes = tmp.stat().st_size if tmp.exists() else 0
            if existing_bytes > 0:
                req_headers["Range"] = f"bytes={existing_bytes}-"
            started = time.monotonic()
            with opener.open(Request(url, headers=req_headers), timeout=timeout) as response:
                status = getattr(response, "status", 200)
                if existing_bytes > 0 and status == 206:
                    mode = "ab"
                else:
                    existing_bytes = 0
                written = existing_bytes
                with tmp.open(mode) as output:
                    while True:
                        chunk = response.read(CHUNK_BYTES)
                        if not chunk:
                            break
                        output.write(chunk)
                        written += len(chunk)
            tmp.replace(destination)
            elapsed = max(time.monotonic() - started, 0.001)
            mb = written / (1024 * 1024)
            if written >= 1024 * 1024:
                log(f"Downloaded {label} ({mb:.1f} MiB in {elapsed:.1f}s, {mb / elapsed:.2f} MiB/s)")
            return
        except HTTPError as error:
            if error.code == 416 and tmp.exists():
                tmp.unlink(missing_ok=True)
            last_error = SyncError(f"Download failed for {label} (HTTP {error.code})")
        except (OSError, URLError, http.client.HTTPException):
            last_error = SyncError(f"Download failed for {label} (network error)")
        if attempt < attempts:
            wait = min(2 ** attempt, 15)
            log(f"Retrying download {label} ({attempt}/{attempts}) in {wait}s...")
            time.sleep(wait)
    tmp.unlink(missing_ok=True)
    raise last_error or SyncError(f"Could not download {label}")


def fetch_github_release(
    repository: str,
    tag: str,
    release_json: Path,
    assets_dir: Path,
    *,
    token: str = "",
    workers: int = 4,
) -> dict:
    if not re.fullmatch(r"[^/\s]+/[^/\s]+", repository):
        raise SyncError("GitHub repository must be owner/repo")
    api_url = f"https://api.github.com/repos/{repository}/releases/tags/{quote(tag, safe='')}"
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "muteki-atomgit-sync",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    opener = build_https_opener()
    try:
        with opener.open(Request(api_url, headers=headers), timeout=60) as response:
            source = json.load(response)
    except HTTPError as error:
        raise SyncError(f"GitHub release fetch failed (HTTP {error.code})") from None
    except (OSError, URLError, ValueError):
        raise SyncError("GitHub release fetch failed (network or JSON error)") from None
    if source.get("draft"):
        raise SyncError(f"GitHub release {tag} is still a draft")
    release_json.parent.mkdir(parents=True, exist_ok=True)
    release_json.write_text(json.dumps(source, indent=2) + "\n", encoding="utf-8")
    assets_dir.mkdir(parents=True, exist_ok=True)
    assets = sorted(source.get("assets") or [], key=lambda item: item.get("size", 0))
    if not assets:
        raise SyncError(f"GitHub release {tag} has no assets")
    download_headers = {"User-Agent": "muteki-atomgit-sync"}
    if token:
        download_headers["Authorization"] = f"Bearer {token}"

    def fetch_one(asset: dict) -> None:
        name = asset["name"]
        url = asset.get("browser_download_url") or ""
        if urlsplit(url).scheme != "https":
            raise SyncError(f"Invalid GitHub asset URL for {name}")
        dest = assets_dir / name
        if dest.is_file() and dest.stat().st_size == asset.get("size"):
            expected_digest = asset.get("digest") or ""
            if not expected_digest or expected_digest == "sha256:" + sha256(dest):
                log(f"Reused local GitHub asset {name}")
                return
        download_url(url, dest, headers=download_headers, label=name)
        if dest.stat().st_size != asset.get("size"):
            raise SyncError(f"Downloaded GitHub asset size mismatch: {name}")
        expected_digest = asset.get("digest") or ""
        if expected_digest and expected_digest != "sha256:" + sha256(dest):
            raise SyncError(f"Downloaded GitHub asset SHA-256 mismatch: {name}")

    max_workers = max(1, min(workers, len(assets)))
    with ThreadPoolExecutor(max_workers=max_workers) as pool:
        futures = [pool.submit(fetch_one, asset) for asset in assets]
        for future in as_completed(futures):
            future.result()
    log(f"Fetched {len(assets)} GitHub release assets for {tag}")
    return source


class AtomGit:
    def __init__(self, repository: str, tag: str, token: str):
        owner_repo = "/".join(quote(part, safe="") for part in repository.split("/"))
        self.root = f"https://api.atomgit.com/api/v5/repos/{owner_repo}/releases"
        self.actions_root = f"https://api.atomgit.com/api/v8/repos/{owner_repo}/actions"
        self.tag = quote(tag, safe="")
        self.release = self.root + "/" + self.tag
        self.token = token
        self.opener = build_https_opener()

    def request(
        self,
        method: str,
        url: str,
        *,
        payload=None,
        headers=None,
        authenticated=True,
        missing_ok=False,
        timeout: int = 120,
    ):
        request_headers = dict(headers or {})
        if authenticated and self.token:
            request_headers["PRIVATE-TOKEN"] = self.token
        if payload is not None:
            request_headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode() if payload is not None else None
        try:
            with self.opener.open(Request(url, data=data, headers=request_headers, method=method), timeout=timeout) as response:
                if method == "PUT" or response.status == 204:
                    return None
                raw = response.read()
                return json.loads(raw.decode("utf-8")) if raw else None
        except HTTPError as error:
            if missing_ok and error.code == 404:
                return None
            # URLs, response bodies and signed upload headers can contain secrets.
            raise SyncError(f"AtomGit {method} failed (HTTP {error.code})") from None
        except (OSError, URLError, ValueError, http.client.HTTPException):
            raise SyncError(f"AtomGit {method} failed (network or response error)") from None

    def get(self):
        return self.request("GET", self.root + "/tags/" + self.tag, missing_ok=True)

    def download(self, name: str, destination: Path):
        url = self.release + "/attach_files/" + quote(name, safe="") + "/download"
        headers = {"PRIVATE-TOKEN": self.token} if self.token else {}
        download_url(url, destination, headers=headers, attempts=3, label=f"AtomGit:{name}")

    def upload(self, name: str, path: Path, attempts: int = 3):
        last_error: Exception | None = None
        size = path.stat().st_size
        for attempt in range(1, attempts + 1):
            try:
                signed = self.request("GET", self.release + "/upload_url?" + urlencode({"file_name": name}))
                if not isinstance(signed, dict) or not isinstance(signed.get("headers"), dict) or urlsplit(signed.get("url", "")).scheme != "https":
                    raise SyncError("AtomGit returned an invalid upload address")
                started = time.monotonic()
                put_file_chunked(signed["url"], path, signed["headers"], timeout=180)
                elapsed = max(time.monotonic() - started, 0.001)
                mb = size / (1024 * 1024)
                if size >= 1024 * 1024:
                    log(f"Uploaded {name} ({mb:.1f} MiB in {elapsed:.1f}s, {mb / elapsed:.2f} MiB/s)")
                else:
                    log(f"Uploaded {name} ({size} bytes in {elapsed:.1f}s)")
                return
            except SyncError as error:
                last_error = error
                if attempt < attempts:
                    wait = min(3 * attempt, 15)
                    log(f"Retrying upload {name} ({attempt}/{attempts}) in {wait}s: {error}")
                    time.sleep(wait)
        raise last_error or SyncError(f"AtomGit upload failed for {name}")

    def dispatch_workflow(self, workflow_id: str, ref: str, inputs: dict[str, str], attempts: int = 4) -> dict:
        if not self.token:
            raise SyncError("ATOMGIT_TOKEN is required to dispatch AtomGit workflow")
        url = (
            f"{self.actions_root}/workflows/{quote(workflow_id, safe='')}/dispatches?"
            + urlencode({"access_token": self.token})
        )
        last_error: Exception | None = None
        for attempt in range(1, attempts + 1):
            try:
                result = self.request("POST", url, payload={"ref": ref, "inputs": inputs}, authenticated=False)
                if not isinstance(result, dict) or not result.get("workflow_run_id"):
                    raise SyncError("AtomGit workflow dispatch returned an unexpected response")
                return result
            except SyncError as error:
                last_error = error
                if "HTTP 404" in str(error) and attempt < attempts:
                    log(f"AtomGit workflow {workflow_id} not indexed yet ({attempt}/{attempts}); retrying in 5s...")
                    time.sleep(5)
                    continue
                raise
        raise last_error or SyncError(f"Could not dispatch AtomGit workflow {workflow_id}")

    def get_workflow_run(self, run_id: str) -> dict:
        if not self.token:
            raise SyncError("ATOMGIT_TOKEN is required to inspect AtomGit workflow run")
        url = (
            f"{self.actions_root}/runs/{quote(str(run_id), safe='')}?"
            + urlencode({"access_token": self.token})
        )
        result = self.request("GET", url, authenticated=False)
        if not isinstance(result, dict):
            raise SyncError("AtomGit workflow run status returned an unexpected response")
        return result


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


def sync(api: AtomGit, source: dict, files: list[Path], commit: str, dry_run: bool, workers: int = 4):
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
    ordered_files = sorted(files, key=lambda p: (p.stat().st_size, p.name))
    with tempfile.TemporaryDirectory(prefix="muteki-atomgit-") as directory:
        temp_root = Path(directory)
        pending: list[tuple[Path, str]] = []
        # Check every collision before changing either metadata or attachments.
        for path in ordered_files:
            digest = sha256(path)
            if path.name in existing:
                downloaded = temp_root / f"existing-{path.name}"
                api.download(path.name, downloaded)
                if sha256(downloaded) != digest:
                    raise SyncError(f"Immutable attachment conflict: {path.name}; SHA-256 differs. Audit the release manually before retrying")
                downloaded.unlink(missing_ok=True)
                log(f"Skip {path.name}: SHA-256 matches")
            else:
                log(f"{'Would upload' if dry_run else 'Upload planned'} {path.name} ({path.stat().st_size} bytes)")
                pending.append((path, digest))
        metadata_changed = not release or any(release.get(key) != value for key, value in payload.items())
        if dry_run:
            log(f"Dry run: {'create' if not release else 'update' if metadata_changed else 'keep'} release; {len(pending)} attachment uploads")
            return
        if not release:
            api.request("POST", api.root, payload={**payload, "tag_name": source["tag_name"], "target_commitish": commit})
            log(f"Created AtomGit release {source['tag_name']}")
        elif metadata_changed:
            api.request("PATCH", api.release, payload=payload)
            log(f"Updated AtomGit release metadata for {source['tag_name']}")

        def upload_and_verify(item: tuple[Path, str]) -> None:
            path, digest = item
            api.upload(path.name, path)
            downloaded = temp_root / f"verify-{path.name}"
            try:
                api.download(path.name, downloaded)
                if sha256(downloaded) != digest:
                    raise SyncError(f"Uploaded attachment checksum mismatch: {path.name}")
                log(f"Verified {path.name}: SHA-256 matches")
            finally:
                downloaded.unlink(missing_ok=True)

        if pending:
            # Upload the smallest file first to fail fast on token/permission errors.
            upload_and_verify(pending[0])
            rest = pending[1:]
            if rest:
                max_workers = max(1, min(workers, len(rest)))
                with ThreadPoolExecutor(max_workers=max_workers) as pool:
                    futures = [pool.submit(upload_and_verify, item) for item in rest]
                    for future in as_completed(futures):
                        future.result()
    log(f"AtomGit release {source['tag_name']} synchronized ({len(files)} attachments)")


def dispatch_and_wait(
    repository: str,
    tag: str,
    workflow: str,
    ref: str,
    dry_run: bool,
    token: str,
    timeout_seconds: int = 1800,
) -> None:
    api = AtomGit(repository, tag, token)
    dispatched = api.dispatch_workflow(
        workflow,
        ref,
        {"tag": tag, "dry_run": "true" if dry_run else "false"},
    )
    run_id = dispatched["workflow_run_id"]
    log(f"Dispatched AtomGit workflow {workflow} (run_id={run_id}) for {tag} on ref {ref}")
    deadline = time.monotonic() + timeout_seconds
    last_status = None
    while time.monotonic() < deadline:
        run = api.get_workflow_run(run_id)
        status = str(run.get("status") or "").upper()
        if status != last_status:
            log(f"AtomGit workflow run {run_id} status: {status}")
            last_status = status
        if status == "COMPLETED":
            log(f"AtomGit workflow run {run_id} completed successfully")
            return
        if status in {"FAILED", "CANCELED", "IGNORED"}:
            raise SyncError(f"AtomGit workflow run {run_id} ended with status {status}")
        time.sleep(10)
    raise SyncError(f"Timed out waiting for AtomGit workflow run {run_id} (last status: {last_status})")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-json", type=Path, help="GitHub REST release response path")
    parser.add_argument("--assets-dir", type=Path, help="Directory of downloaded GitHub release assets")
    parser.add_argument("--target-commitish", help="Commit SHA of the mirrored Git tag")
    parser.add_argument("--repository", default="FishCodeTech/muteki", help="Target AtomGit owner/repo")
    parser.add_argument("--fetch-github-repo", help="Download published release JSON & assets from this GitHub owner/repo")
    parser.add_argument("--tag", help="Release tag (required with --fetch-github-repo or --dispatch-workflow)")
    parser.add_argument("--download-only", action="store_true", help="Only download and verify GitHub release assets")
    parser.add_argument("--dispatch-workflow", help="Trigger this AtomGit workflow filename via v8 Actions API and wait")
    parser.add_argument("--dispatch-ref", default="main", help="Git ref on AtomGit for --dispatch-workflow")
    parser.add_argument("--workers", type=int, default=4, help="Concurrent download/upload workers")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    token = os.environ.get("ATOMGIT_TOKEN", "").strip()
    if not re.fullmatch(r"[^/\s]+/[^/\s]+", args.repository):
        raise SyncError("Repository must be owner/repo")

    if args.dispatch_workflow:
        if not args.tag or not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", args.tag):
            raise SyncError("--tag must be a version tag (for example v0.5.1)")
        dispatch_and_wait(args.repository, args.tag, args.dispatch_workflow, args.dispatch_ref, args.dry_run, token)
        return 0

    if not args.release_json or not args.assets_dir:
        raise SyncError("--release-json and --assets-dir are required")

    if args.fetch_github_repo:
        if not args.tag or not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?", args.tag):
            raise SyncError("--tag must be a version tag when using --fetch-github-repo")
        gh_token = os.environ.get("GH_API_TOKEN", "").strip() or os.environ.get("GH_TOKEN", "").strip()
        fetch_github_release(
            args.fetch_github_repo,
            args.tag,
            args.release_json,
            args.assets_dir,
            token=gh_token,
            workers=args.workers,
        )
        if args.download_only:
            return 0

    if not args.target_commitish:
        raise SyncError("--target-commitish is required unless --download-only is set")
    if not token and not args.dry_run:
        raise SyncError("ATOMGIT_TOKEN is required")

    source = json.loads(args.release_json.read_text(encoding="utf-8"))
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
    sync(AtomGit(args.repository, tag, token), source, files, args.target_commitish, args.dry_run, workers=args.workers)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (SyncError, OSError, ValueError, KeyError, tarfile.TarError) as error:
        print(f"Release sync failed: {error}", file=sys.stderr, flush=True)
        raise SystemExit(1)
