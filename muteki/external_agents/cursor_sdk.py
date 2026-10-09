"""Cursor chat adapter backed by a local ``@cursor/sdk`` stdio bridge.

The bridge is installed into an adapter-owned directory under the Muteki
state root. ``CURSOR_API_KEY`` is passed only through the child environment.
A missing key fails with ``cursor_sdk.auth_required``; this adapter never
falls back to ``cursor.acp``. A failed resume is not retried as a new agent.
"""

from __future__ import annotations

import asyncio
import errno
import fcntl
import io
import signal
import tarfile
import tempfile
import time
import threading
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path, PurePosixPath
from typing import Any, AsyncIterator, Optional

from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    PlanPayload,
    PlanTaskPayload,
    ReasoningPayload,
    RuntimeExitedPayload,
    RuntimeWarningPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    dump_payload,
    redact_secrets,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES,
    AccessMode,
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    MessageInput,
    ProbeRequest,
    SessionStart,
    SteerInput,
)
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .acp import materialize_mcp_servers
from .base import BaseExternalAgentAdapter, TurnLimits, TurnRunner
from .capabilities import (
    SOURCE_PROBE,
    SOURCE_REPORTED,
    SOURCE_STATIC,
    CapabilityProbeReport,
    conservative_capabilities,
    require_access_mode,
)
from .events import build_event
from .probe_environment import subprocess_environment
from .rpc import PeerClosedError, StdioJsonlPeer
from .sessions import EXIT_FAILED, EXIT_RESUMABLE

SDK_PACKAGE = "@cursor/sdk"
SDK_VERSION = "1.0.36"
MIN_NODE = (22, 13)
PROTOCOL_VERSION = 1
_BRIDGE_DIR = Path(__file__).resolve().parent / "cursor_sdk_bridge"
_ACCESS_NOTES = {
    AccessMode.SUPERVISED.value: (
        "SDK 没有交互审批回调。supervised 映射为 autoReview=true 且开启 sandbox，"
        "比 ACP 的逐次审批弱。"
    ),
    AccessMode.AUTO_ACCEPT_EDITS.value: (
        "SDK 不能按编辑类型单独放行。auto-accept-edits 与 supervised 一样映射为 "
        "autoReview=true 加 sandbox，比交互审批弱。"
    ),
}
_EFFORT_PARAMETERS = ("reasoning_effort", "effort", "reasoning")
_FAST_PARAMETER = "fast"
_PLAN_STATUSES = {"pending", "in_progress", "completed", "blocked", "cancelled"}
_INSTALL_LOCK = asyncio.Lock()
_DEFAULT_TURN_TIMEOUT_S = 600
_PUBLIC_PLUGIN_REPOSITORIES = ("cursor/plugins", "upstash/context7")


def _plugin_git_environment(env: dict[str, str], root: Path) -> dict[str, str]:
    """Keep known public plugin sources on HTTPS in this bridge only.

    Cursor marketplace metadata supplies HTTPS for these repositories, but
    the SDK first attempts an SSH clone, which stalls without SSH access.
    SDK plugin fetches replace GIT_CONFIG_COUNT when adding safe.directory,
    so use a private global-config overlay, including the caller's configs.
    Other repositories keep their configured transport and credentials.
    """
    home = Path(env.get("HOME") or str(Path.home())).expanduser()
    explicit = env.get("GIT_CONFIG_GLOBAL")
    global_configs = ([Path(explicit).expanduser()] if explicit else [
        Path(env.get("XDG_CONFIG_HOME") or str(home / ".config")) / "git" / "config",
        home / ".gitconfig",
    ])

    def quoted(value: str) -> str:
        return '"' + value.replace('\\', '\\\\').replace('"', '\\"').replace('\n', '\\n').replace('\t', '\\t') + '"'

    text = "".join(
        f"[include]\n\tpath = {quoted(str(path.resolve()))}\n"
        for path in global_configs if explicit or path.is_file()
    )
    text += "".join(
        f'[url "https://github.com/{repository}.git"]\n'
        f'\tinsteadOf = git@github.com:{repository}.git\n'
        for repository in _PUBLIC_PLUGIN_REPOSITORIES
    )
    root.mkdir(parents=True, exist_ok=True)
    config = root / "plugin-git.config"
    config.write_text(text, encoding="utf-8")
    config.chmod(0o600)
    return {**env, "GIT_CONFIG_GLOBAL": str(config.resolve())}
_INSTALL_TIMEOUT_S = 600


_PUBLIC_PLUGIN_PATHS = {
    "cursor-team-kit": "cursor-team-kit", "docs-canvas": "docs-canvas",
    "pr-review-canvas": "pr-review-canvas", "cursor-sdk": "cursor-sdk",
    "pstack": "pstack", "gmail": "third_party/gmail",
}
_PUBLIC_PLUGIN_REPOSITORY = "https://github.com/cursor/plugins.git"


class _PublicPluginGitCache:
    """Derive public packages from verified Git objects, never installed caches."""

    def __init__(self, runtime: Path, env: dict[str, str], key: str, deadline: float) -> None:
        private = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"]).expanduser().resolve()
        home = Path(env["HOME"]).expanduser().resolve()
        if home != private / "home" or home == Path.home().resolve() or not (private / ".imports.json").is_file():
            raise CursorSdkFailure("cursor_sdk.plugin_cache_scope", "Public cache requires a managed private HOME", category="validation")
        self.cancelled = threading.Event()
        self.process_lock = threading.Lock()
        self.active_process: subprocess.Popen | None = None
        self.deadline = deadline
        self.home = home
        self.local = home / ".cursor/plugins/cache/cursor-public"
        self.scope = runtime / "public-plugin-git" / hashlib.sha256(key.encode()).hexdigest()
        self.repo = self.scope / "cursor-plugins.git"
        self.temp_root = Path(env.get("TMPDIR") or tempfile.gettempdir()).resolve()
        # Public Git helpers never receive Cursor keys, native auth, or history.
        self.git_env = {name: os.environ[name] for name in (
            "PATH", "LANG", "LC_ALL", "SYSTEMROOT", "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY", "no_proxy",
        ) if name in os.environ}
        self.git_env.update(HOME=str(home), GIT_CONFIG_GLOBAL=os.devnull,
                            GIT_CONFIG_NOSYSTEM="1", GIT_NO_LAZY_FETCH="1",
                            GIT_NO_REPLACE_OBJECTS="1", GIT_TERMINAL_PROMPT="0",
                            GIT_OPTIONAL_LOCKS="0")

    def cancel(self) -> None:
        self.cancelled.set()
        with self.process_lock:
            if self.active_process is not None:
                try:
                    os.killpg(self.active_process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    def _check_cancelled(self) -> None:
        if self.cancelled.is_set():
            raise CursorSdkFailure("cursor_sdk.plugin_cache_cancelled", "Public plugin cache preparation cancelled", category="cancelled")

    @staticmethod
    def _sha(value: Any) -> bool:
        return isinstance(value, str) and len(value) == 40 and all(c in "0123456789abcdef" for c in value)

    @staticmethod
    def _directory(path: Path) -> None:
        if any(p.is_symlink() for p in (path, *path.parents)):
            raise CursorSdkFailure("cursor_sdk.plugin_cache_scope", "Public cache cannot traverse symlinks", category="validation")
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        path.chmod(0o700)

    def _git(self, args: list[str], *, repo: Path | None = None, network: bool = False,
             input: bytes | None = None, check: bool = True, timeout: float = 30) -> subprocess.CompletedProcess:
        prefix = ["git", "--no-replace-objects", "-c", "core.hooksPath=" + os.devnull,
                  "-c", "core.fsmonitor=false", "-c", "protocol.allow=never",
                  "-c", "remote.origin.promisor=false", "-c", "maintenance.auto=false"]
        if network:
            prefix += ["-c", "protocol.https.allow=always"]
        prefix += ["-C", str(repo)] if repo else ["--git-dir", str(self.repo)]
        self._check_cancelled()
        remaining = min(timeout, self.deadline - time.monotonic())
        if remaining <= 0:
            raise CursorSdkFailure("cursor_sdk.plugin_cache_timeout", "Public plugin startup budget exhausted", category="timeout")
        with self.process_lock:
            self._check_cancelled()
            process = subprocess.Popen(prefix + args, env=self.git_env, stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            self.active_process = process
        try:
            stdout, stderr = process.communicate(input, timeout=remaining)
        except subprocess.TimeoutExpired as exc:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = process.communicate()
            raise CursorSdkFailure("cursor_sdk.plugin_cache_timeout", "Public Git operation timed out: " + args[0],
                                   detail=stderr.decode("utf-8", errors="replace"), category="timeout") from exc
        finally:
            with self.process_lock:
                self.active_process = None
        self._check_cancelled()
        result = subprocess.CompletedProcess(prefix + args, process.returncode, stdout, stderr)
        if check and result.returncode:
            raise CursorSdkFailure("cursor_sdk.plugin_cache_git", "Public Git cache operation failed: " + args[0],
                                   detail=result.stderr.decode("utf-8", errors="replace"), category="validation")
        return result

    def _present(self, commit: str, repo: Path | None = None) -> bool:
        return self._git(["cat-file", "-e", commit + "^{commit}"], repo=repo, check=False).returncode == 0

    def _check(self, commit: str, repo: Path | None = None) -> None:
        self._git(["fsck", "--strict", "--no-reflogs", "--no-dangling", commit], repo=repo)

    def _import_objects(self, source: Path, commit: str) -> None:
        self._check(commit, source)
        raw_commit = self._git(["cat-file", "commit", commit], repo=source).stdout
        tree_line = next((x for x in raw_commit.splitlines() if x.startswith(b"tree ")), None)
        if not tree_line:
            raise CursorSdkFailure("cursor_sdk.plugin_cache_git", "Official commit has no tree", category="validation")
        root_tree = tree_line[5:].decode("ascii")
        objects = {commit, root_tree}
        # Only the requested commit snapshot. No other refs, history, config or dangling objects.
        for record in self._git(["ls-tree", "-r", "-t", "-z", commit], repo=source).stdout.split(b"\0"):
            if record:
                objects.add(record.split(b"\t", 1)[0].split()[2].decode("ascii"))
        # pack-objects without --revs includes only the explicit object IDs.
        # Git validates canonical object hashes while unpacking; fsck validates the graph.
        packed = self._git(["pack-objects", "--stdout"], repo=source,
                           input="".join(x + "\n" for x in sorted(objects)).encode("ascii")).stdout
        self._git(["unpack-objects", "-r"], input=packed)
        shallow = self.repo / "shallow"
        commits = set(shallow.read_text().splitlines()) if shallow.is_file() else set()
        commits.add(commit)
        shallow.write_text("".join(x + "\n" for x in sorted(commits)), encoding="ascii")
        self._git(["update-ref", "refs/muteki/public/" + commit, commit])
        self._check(commit)

    def _ensure_commit(self, commit: str) -> str:
        if self._present(commit):
            self._check(commit)
            return "existing"
        # SDK's already downloaded raw checkouts may survive a cancelled startup.
        for source in sorted(self.temp_root.glob("backend-plugin-*")):
            if source.is_symlink() or not (source / ".git").is_dir():
                continue
            if self._present(commit, source):
                self._import_objects(source, commit)
                return "local_git_objects"
        # Exactly one official repo fetch per missing, metadata-selected commit.
        self._git(["fetch", "--depth=1", "--no-tags", _PUBLIC_PLUGIN_REPOSITORY,
                   commit + ":refs/muteki/public/" + commit], network=True, timeout=600)
        self._check(commit)
        return "official_fetch"

    def prepare(self, metadata: dict[str, Any]) -> dict[str, Any]:
        self._directory(self.scope)
        lock_path = self.scope / "cache.lock"
        if lock_path.is_symlink():
            raise CursorSdkFailure("cursor_sdk.plugin_cache_scope", "Public cache lock is a symlink", category="validation")
        with lock_path.open("a") as lock:
            while True:
                self._check_cancelled()
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= self.deadline:
                        raise CursorSdkFailure("cursor_sdk.plugin_cache_timeout", "Public cache lock budget exhausted", category="timeout")
                    time.sleep(0.05)
            return self._prepare(metadata)

    def _prepare(self, metadata: dict[str, Any]) -> dict[str, Any]:
        approved = []
        for item in metadata.get("plugins", []):
            if (isinstance(item, dict) and item.get("name") in _PUBLIC_PLUGIN_PATHS
                    and item.get("repository") == _PUBLIC_PLUGIN_REPOSITORY
                    and self._sha(item.get("commit"))
                    and item.get("gitPath") == _PUBLIC_PLUGIN_PATHS[item["name"]]):
                approved.append(item)
        self._directory(self.scope)
        if any(p.is_symlink() for p in (self.local, *self.local.parents)):
            return {"skipped": "private SDK cache traverses symlinks", "hydrated": 0}
        self._directory(self.local)
        if not self.repo.exists():
            self._git(["init", "--bare"])
        elif self.repo.is_symlink():
            raise CursorSdkFailure("cursor_sdk.plugin_cache_scope", "Public Git store is a symlink", category="validation")
        self._git(["fsck", "--strict", "--no-reflogs", "--no-dangling"])
        result: dict[str, Any] = {"commits": {}, "hydrated": 0, "existing": 0}
        for commit in sorted({x["commit"] for x in approved}):
            result["commits"][commit] = self._ensure_commit(commit)
        for item in approved:
            destination = self.local / item["name"] / item["commit"]
            if any(p.is_symlink() for p in (destination, *destination.parents)):
                result["existing"] += 1
                continue
            self._directory(destination.parent)
            if destination.exists() or destination.is_symlink():
                result["existing"] += 1
                continue  # Existing mutable/private installations are never replaced or shared.
            archive = self._git(["archive", "--format=tar", item["commit"] + ":" + item["gitPath"]]).stdout
            with tempfile.TemporaryDirectory(prefix=".hydrate-", dir=destination.parent) as temporary:
                staged = Path(temporary) / item["commit"]; staged.mkdir(mode=0o700)
                with tarfile.open(fileobj=io.BytesIO(archive), mode="r:") as stream:
                    for member in stream.getmembers():
                        path = PurePosixPath(member.name)
                        if path.is_absolute() or ".." in path.parts or not (member.isfile() or member.isdir()):
                            raise CursorSdkFailure("cursor_sdk.plugin_cache_archive", "Unsupported public package archive entry", category="validation")
                    stream.extractall(staged, filter="data")
                (staged / ".cache-complete").write_text("", encoding="ascii")
                try:
                    staged.rename(destination); result["hydrated"] += 1
                except OSError as exc:
                    if exc.errno not in {errno.EEXIST, errno.ENOTEMPTY}:
                        raise
                    result["existing"] += 1
        return result


class CursorSdkFailure(RuntimeError):
    """Bridge or launch failure. Branch on ``code``, never on the message."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        detail: str = "",
        category: str = "provider",
        retryable: bool = False,
        native_code: Optional[str] = None,
    ) -> None:
        super().__init__(redact_secrets(message))
        self.code = code
        self.detail = redact_secrets(detail)
        self.category = category
        self.retryable = retryable
        self.native_code = native_code

    @classmethod
    def from_bridge_error(cls, error: dict[str, Any], *, default_code: str, default_message: str) -> "CursorSdkFailure":
        """Rebuild a failure from the bridge's error object, keeping the SDK's own identifiers."""
        facts = [
            f"{key}={error[key]}"
            for key in ("sdkClass", "nativeCode", "status", "requestId", "operation", "endpoint")
            if error.get(key) not in (None, "")
        ]
        detail = str(error.get("detail") or "")
        if error.get("causes"):
            detail = "\n".join(filter(None, [detail, json.dumps(error["causes"], ensure_ascii=False)]))
        if facts:
            detail = "\n".join([", ".join(facts), detail]) if detail else ", ".join(facts)
        native = error.get("nativeCode")
        return cls(
            str(error.get("code") or default_code),
            str(error.get("message") or default_message),
            detail=detail,
            category=str(error.get("category") or "provider"),
            retryable=bool(error.get("retryable")),
            native_code=str(native) if native not in (None, "") else None,
        )


def _node_version(binary: str = "node") -> tuple[int, ...]:
    try:
        completed = subprocess.run(
            [binary, "--version"],
            capture_output=True, text=True, timeout=15, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise CursorSdkFailure(
            "cursor_sdk.node_unsupported",
            f"node is not runnable ({type(exc).__name__})",
            category="validation",
        ) from exc
    text = (completed.stdout or completed.stderr or "").strip()
    if completed.returncode != 0 or not text.startswith("v"):
        raise CursorSdkFailure(
            "cursor_sdk.node_unsupported",
            f"node --version failed: {text}",
            detail=text,
            category="validation",
        )
    parts: list[int] = []
    for item in text[1:].split("."):
        if not item.isdigit():
            break
        parts.append(int(item))
    if tuple(parts[:2]) < MIN_NODE:
        raise CursorSdkFailure(
            "cursor_sdk.node_unsupported",
            f"node {text} is older than {MIN_NODE[0]}.{MIN_NODE[1]}",
            detail=text,
            category="validation",
        )
    return tuple(parts)


def _sdk_mcp_servers(entries: list[dict[str, Any]]) -> dict[str, Any]:
    """ACP mcpServers list -> SDK ``Record<string, McpServerConfig>``."""
    servers: dict[str, Any] = {}
    for entry in entries:
        name = str(entry.get("name") or "").strip()
        if not name:
            continue
        url = entry.get("url") or entry.get("httpUrl")
        if url:
            headers: dict[str, str] = {}
            raw_headers = entry.get("headers") or {}
            if isinstance(raw_headers, dict):
                headers = {str(key): str(value) for key, value in raw_headers.items()}
            else:
                for header in raw_headers:
                    if isinstance(header, dict) and header.get("name"):
                        headers[str(header["name"])] = str(header.get("value") or "")
            kind = "sse" if str(entry.get("type") or "") == "sse" else "http"
            servers[name] = {"type": kind, "url": str(url), "headers": headers}
            continue
        command = entry.get("command")
        if not command:
            continue
        item: dict[str, Any] = {"command": str(command)}
        if entry.get("args"):
            item["args"] = [str(arg) for arg in entry["args"]]
        if isinstance(entry.get("env"), dict):
            item["env"] = {str(key): str(value) for key, value in entry["env"].items()}
        servers[name] = item
    return servers


def _access_policy(mode: Optional[str]) -> dict[str, Any]:
    """T3 mapping: only full-access disables auto-review and the sandbox."""
    if mode == AccessMode.FULL_ACCESS.value:
        return {"autoReview": False, "sandbox": False}
    return {"autoReview": True, "sandbox": True}


def _model_selection(model: Optional[str]) -> dict[str, Any]:
    model_id = str(model or "").strip()
    return {"id": "default" if model_id in {"", "auto"} else model_id}


def _effort(value: Optional[str]) -> Optional[str]:
    effort = str(value or "").strip()
    return None if effort in {"", "default"} else effort


def _parameter_values(param: Optional[dict[str, Any]]) -> list[str]:
    values: list[str] = []
    for value in (param or {}).get("values") or []:
        if isinstance(value, dict) and value.get("value"):
            token = str(value["value"])
            if token not in values:
                values.append(token)
    return values


def _failure_category(name: str) -> FailureCategory:
    try:
        return FailureCategory(name)
    except ValueError:
        return FailureCategory.PROVIDER


def _catalog(models: list[Any]) -> dict[str, Any]:
    rows: list[dict[str, Any]] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        model_id = str(item.get("id") or "").strip()
        if not model_id:
            continue
        params = [param for param in item.get("parameters") or [] if isinstance(param, dict)]
        # context / fast / thinking carry values that are not efforts. The
        # bridge applies the first effort-like parameter, so read the same one.
        effort_param = next((param for param in params if param.get("id") in _EFFORT_PARAMETERS), None)
        fast_param = next((param for param in params if param.get("id") == _FAST_PARAMETER), None)
        levels = _parameter_values(effort_param)
        default_variant = next((
            variant for variant in item.get("variants") or []
            if isinstance(variant, dict) and variant.get("isDefault") is True
        ), {})
        defaults = {
            str(param.get("id")): str(param.get("value"))
            for param in default_variant.get("params") or []
            if isinstance(param, dict) and param.get("id")
        }
        default_effort = defaults.get(str(effort_param.get("id")), "") if effort_param else ""
        fast_values = _parameter_values(fast_param)
        service_tiers = [{
            "id": _FAST_PARAMETER,
            "name": _FAST_PARAMETER,
            "description": "Cursor Fast 模式，响应更快，用量更高",
        }] if "true" in fast_values else []
        rows.append({
            "id": model_id,
            "label": str(item.get("displayName") or model_id),
            "reasoning": {
                "supported": bool(levels),
                "levels": levels,
                "default": default_effort if default_effort in levels else "",
                "kind": "effort",
                "source": "cursor.sdk.models",
            },
            "service_tiers": service_tiers,
            "default_service_tier": (
                _FAST_PARAMETER if service_tiers and defaults.get(_FAST_PARAMETER) == "true" else ""
            ),
        })
    default = next((row["id"] for row in rows if row["id"] == "default"), "")
    return {
        "ok": True,
        "models": rows,
        "default_model": default or (rows[0]["id"] if rows else ""),
        "source": "cursor.sdk.models",
    }


class CursorSdkAdapter(BaseExternalAgentAdapter):
    """Local Cursor SDK runtime. ``adapter_id`` is ``cursor.sdk``."""

    adapter_id = "cursor.sdk"

    def __init__(
        self,
        *,
        node_binary: Optional[str] = None,
        runtime_root: Optional[str | Path] = None,
        env_extra: Optional[dict[str, str]] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._node = node_binary or os.environ.get("MUTEKI_CURSOR_SDK_NODE") or "node"
        self._runtime_root = Path(
            runtime_root or (Path(os.environ.get("TMPDIR") or "/tmp") / "muteki-cursor-sdk")
        ).expanduser().resolve()
        self._env_extra = dict(env_extra or {})
        self._handles: dict[str, dict[str, Any]] = {}

    def probe_binary(self) -> str:
        return shutil.which(self._node) or self._node

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        catalog: dict[str, Any] | None = None
        caps = conservative_capabilities(transport_kind="sdk", capability_source=SOURCE_STATIC)
        version = ""
        try:
            node = await asyncio.to_thread(_node_version, self._node)
            version = ".".join(str(part) for part in node)
            await self._ensure_installed()
            hello = await self._with_bridge(None, request_models=False)
            sdk_version = str((hello or {}).get("sdk") or "")
            caps.runtime_version = sdk_version or version
            caps.protocol_version = str((hello or {}).get("protocol") or "")
            if int((hello or {}).get("protocol") or 0) != PROTOCOL_VERSION:
                raise CursorSdkFailure(
                    "cursor_sdk.protocol",
                    f"bridge protocol {(hello or {}).get('protocol')!r} != {PROTOCOL_VERSION}",
                    category="validation",
                )
            key = self._api_key(None)
            if request.include_models and key:
                listed = await self._with_bridge(key, request_models=True)
                raw_models = list((listed or {}).get("models") or [])
                catalog = _catalog(raw_models)
                caps.supported_models = [row["id"] for row in catalog["models"]]
            elif request.include_models:
                catalog = {
                    "ok": False, "models": [], "source": "cursor.sdk.models",
                    "error_code": "cursor_sdk.auth_required",
                    "detail": "CURSOR_API_KEY is not set",
                }
                degradations.append("cursor_sdk.auth_required")
            self._declare(caps, field_sources, SOURCE_PROBE if sdk_version else SOURCE_REPORTED)
            detail = ""
        except CursorSdkFailure as exc:
            degradations.append(f"{exc.code}: {exc}")
            detail = exc.detail or str(exc)
            if exc.code == "cursor_sdk.auth_required":
                self._declare(caps, field_sources, SOURCE_REPORTED)
            catalog = catalog or {
                "ok": False, "models": [], "source": "cursor.sdk.models",
                "error_code": exc.code, "detail": detail,
            }
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps,
            binary_path=self.probe_binary(),
            field_sources=field_sources,
            degradations=degradations,
            detail=detail,
            model_catalog=catalog,
        )
        return caps

    @staticmethod
    def _declare(caps: AgentCapabilities, sources: dict[str, str], source: str) -> None:
        caps.capability_source = source
        caps.streaming = True
        caps.resume = True
        caps.resume_continues_turn = False
        caps.steer = True
        caps.interrupt = True
        caps.approval = False
        caps.user_input = False
        caps.subagents = True
        caps.tool_events = True
        caps.usage_events = True
        caps.session_persistence = True
        caps.mcp = True
        caps.plan = True
        caps.plan_mode = True
        caps.image_input = True
        caps.access_modes = list(ACCESS_MODE_VALUES)
        for name in (
            "streaming", "resume", "steer", "interrupt", "approval", "user_input",
            "subagents", "tool_events", "usage_events", "session_persistence",
            "mcp", "plan", "plan_mode", "image_input",
        ):
            sources[name] = source

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        require_access_mode(
            self.identity.adapter_id, request.access_mode, ACCESS_MODE_VALUES,
            reasons=_ACCESS_NOTES,
        )
        if request.interaction_mode == "plan":
            caps = await self._capabilities()
            if not caps.plan_mode:
                raise CursorSdkFailure(
                    "cursor_sdk.plan_mode_unsupported",
                    "cursor.sdk cannot start a plan-mode session",
                    category="unsupported",
                )
        key = self._api_key(request)
        if not key:
            raise CursorSdkFailure(
                "cursor_sdk.auth_required",
                "CURSOR_API_KEY is not set for the Cursor SDK bridge",
                category="auth",
            )
        await asyncio.to_thread(_node_version, self._node)
        installed = await self._ensure_installed()
        cwd = request.options.cwd or os.getcwd()
        store = self._store_for(request.resume_handle)
        store.mkdir(parents=True, exist_ok=True)
        log_root = self._runtime_root / "sessions" / request.agent_session_id
        log_root.mkdir(parents=True, exist_ok=True)
        env = dict(subprocess_environment({**self._env_extra, **dict(request.options.env)}) or {})
        env["CURSOR_API_KEY"] = key
        env = _plugin_git_environment(env, log_root)
        queue: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
        peer = StdioJsonlPeer(
            [self.probe_binary(), str(installed / "bridge.mjs")],
            cwd=str(installed),
            env=env,
            label="cursor-sdk",
            is_response=lambda msg: isinstance(msg, dict) and msg.get("type") == "response",
            request_envelope=_bridge_envelope,
            on_message=lambda msg: _enqueue(queue, msg),
            owner_adapter_id=self.identity.adapter_id,
            owner_session_id=request.agent_session_id,
            stderr_log_root=self._runtime_root / "logs",
        )
        await peer.start()
        watcher = asyncio.ensure_future(_watch_exit(peer, queue))
        handle: dict[str, Any] = {
            "peer": peer, "queue": queue, "watcher": watcher, "cwd": cwd,
            "store": str(store), "log_path": log_root / "protocol.jsonl",
            "text": [], "thinking": [], "thinking_index": 0, "nested": {},
        }
        self._handles[request.agent_session_id] = handle
        startup_budget = self.conversation_turn_timeout(request.thread_id, _DEFAULT_TURN_TIMEOUT_S) or _DEFAULT_TURN_TIMEOUT_S
        startup_deadline = time.monotonic() + startup_budget
        def startup_remaining() -> float:
            remaining = startup_deadline - time.monotonic()
            if remaining <= 0:
                raise CursorSdkFailure("cursor_sdk.startup_timeout", "Cursor startup budget exhausted", category="timeout")
            return remaining
        try:
            hello = await self._rpc(peer, "hello", timeout=min(60, startup_remaining()))
            if int(hello.get("protocol") or 0) != PROTOCOL_VERSION:
                raise CursorSdkFailure(
                    "cursor_sdk.protocol",
                    f"bridge protocol {hello.get('protocol')!r} != {PROTOCOL_VERSION}",
                    category="validation",
                )
            if env.get("MUTEKI_CHAT_PRIVATE_ROOT"):
                metadata = await self._rpc(peer, "publicPluginMetadata", timeout=min(60, startup_remaining()))
                public_cache = _PublicPluginGitCache(self._runtime_root, env, key, startup_deadline)
                try:
                    cached = await asyncio.to_thread(public_cache.prepare, metadata)
                except BaseException:
                    public_cache.cancel()
                    raise
                self._append_log(handle, {"kind": "public_plugin_git_cache", "cache": cached})
            policy = _access_policy(request.access_mode)
            opened = await self._rpc(peer, "open", {
                "cwd": cwd,
                "storeDir": str(store),
                "model": _model_selection(request.model),
                "effort": _effort(request.effort),
                "serviceTier": str(request.service_tier or "").strip() or None,
                "mode": "plan" if request.interaction_mode == "plan" else "agent",
                "autoReview": policy["autoReview"],
                "sandbox": policy["sandbox"],
                "settingSources": ["project", "user", "team", "mdm", "plugins"],
                "mcpServers": _sdk_mcp_servers(materialize_mcp_servers(plan, bearer_token)),
                **({"agentId": request.resume_handle} if request.resume_handle else {}),
            # Plugin prewarm may fetch the public repository more than once.
            # Share the existing execution budget, while keeping startup
            # bounded even for conversations whose turns have no time limit.
            }, timeout=startup_remaining())
            if request.resume_handle and opened.get("resumed") is not True:
                raise CursorSdkFailure(
                    "cursor_sdk.resume_failed",
                    "cursor.sdk resume failed and will not start a new agent",
                    category="provider",
                )
        except BaseException:
            await self._stop_peer(request.agent_session_id)
            raise
        agent_id = str(opened.get("agentId") or "")
        if not agent_id:
            await self._stop_peer(request.agent_session_id)
            raise CursorSdkFailure(
                "cursor_sdk.protocol", "bridge open did not return agentId",
                category="validation",
            )
        handle["agent_id"] = agent_id
        try:
            self._index_store(agent_id, store)
        except BaseException:
            await self._stop_peer(request.agent_session_id)
            raise
        return {"external_session_id": agent_id, "resume_handle": agent_id}

    def _store_for(self, agent_id: Optional[str]) -> Path:
        """Native agent identity owns storage, independently of Muteki sessions.

        T3 resumes by native agent id against stable SDK storage. Keep that
        behavior while using Muteki-owned stores, and locate pre-upgrade
        session stores without copying their encrypted SDK contents.
        """
        if not agent_id:
            return self._runtime_root / "agents" / new_id("store")
        index = self._runtime_root / "native_index" / (hashlib.sha256(agent_id.encode()).hexdigest() + ".json")
        try:
            if index.exists():
                row = json.loads(index.read_text(encoding="utf-8"))
                store = (self._runtime_root / row["store"]).resolve()
                if row["agent_id"] != agent_id or not store.is_relative_to(self._runtime_root) or not store.is_dir():
                    raise ValueError("invalid native store reference")
                return store
            matches: set[Path] = set()
            for folder in ("sessions", "agents"):
                for path in (self._runtime_root / folder).glob("*/agents.ndjson"):
                    with path.open(encoding="utf-8") as stream:
                        for line in stream:
                            if line.strip() and json.loads(line).get("agentId") == agent_id:
                                matches.add(path.parent)
                                break
            if len(matches) > 1:
                raise CursorSdkFailure("cursor_sdk.store_ambiguous", "native agent exists in multiple SDK stores", category="validation")
            if matches:
                store = matches.pop()
                self._index_store(agent_id, store)
                return store
        except (OSError, ValueError, KeyError, TypeError, AttributeError) as exc:
            raise CursorSdkFailure("cursor_sdk.store_invalid", "native SDK store could not be read", category="validation") from exc
        raise CursorSdkFailure("cursor_sdk.agent_not_found", "native agent has no persisted SDK store", category="provider")

    def _index_store(self, agent_id: str, store: Path) -> None:
        root = self._runtime_root / "native_index"
        root.mkdir(parents=True, exist_ok=True, mode=0o700)
        index = root / (hashlib.sha256(agent_id.encode()).hexdigest() + ".json")
        temporary = root / new_id("store-ref")
        try:
            temporary.write_text(json.dumps({"agent_id": agent_id, "store": str(store.relative_to(self._runtime_root))}), encoding="utf-8")
            temporary.replace(index)
        except OSError as exc:
            raise CursorSdkFailure("cursor_sdk.store_invalid", "native SDK store reference could not be persisted", category="validation") from exc
        finally:
            temporary.unlink(missing_ok=True)

    async def read_thread_snapshot(self, session: AgentSessionRef) -> dict[str, Any]:
        """Read complete native messages through the same SDK API as T3."""
        handle = self._handles.get(session.agent_session_id)
        if handle is None:
            raise CursorSdkFailure("cursor_sdk.agent_not_found", "cursor.sdk session is not open")
        history = await self._rpc(handle["peer"], "history", timeout=60)
        messages = history.get("messages")
        if not isinstance(messages, list):
            raise CursorSdkFailure("cursor_sdk.protocol", "SDK history did not return messages", category="validation")
        return {"agent_id": handle["agent_id"], "messages": messages}

    def send(self, session: AgentSessionRef, input: AgentInput) -> AsyncIterator[AgentEvent]:
        return self._send(session, input)

    async def _send(self, session: AgentSessionRef, input: AgentInput) -> AsyncIterator[AgentEvent]:
        if not isinstance(input, MessageInput):
            async for event in self.unsupported_input_stream(session, input):
                yield event
            return
        handle = self._handles.get(session.agent_session_id)
        if handle is None:
            yield self._failed(session, CursorSdkFailure(
                "cursor_sdk.agent_not_found", "cursor.sdk session is not open",
                category="provider",
            ))
            return
        if input.payload.interaction_mode == "plan":
            caps = await self._capabilities()
            if not caps.plan_mode:
                yield self._failed(session, CursorSdkFailure(
                    "cursor_sdk.plan_mode_unsupported",
                    "cursor.sdk does not support plan mode on this turn",
                    category="unsupported",
                ), turn_id=None)
                return
        turn_id = new_id("turn")
        text, images = _message_body(input)
        record = self.session_record(session.agent_session_id)
        runner = TurnRunner(
            self, session, turn_id=turn_id,
            limits=TurnLimits(
                idle_s=None if (record and record.thread_id) else 180,
                overall_s=self.conversation_turn_timeout(
                    record.thread_id if record else None, _DEFAULT_TURN_TIMEOUT_S),
            ),
            on_abort=lambda failure: self._abort_turn(session, failure),
            diagnostics=lambda: redact_secrets(handle["peer"].stderr_text()),
            auto_ack=False,
            run_id=record.run_id if record else None,
            execution_generation=record.execution_generation if record else None,
        )
        runner.mark_sent()
        try:
            await self._rpc(handle["peer"], "send", {
                "turn": turn_id,
                "text": text,
                "mode": "plan" if input.payload.interaction_mode == "plan" else "agent",
                **({"images": images} if images else {}),
            }, timeout=120)
        except CursorSdkFailure as exc:
            if exc.code == "cursor_sdk.bridge_exited":
                yield self._bridge_exited(session, turn_id, None, exc.detail)
            else:
                yield self._failed(session, exc, turn_id=turn_id)
            return
        runner.ack()
        handle["text"] = []
        handle["thinking"] = []
        handle["thinking_index"] = 0

        async def source() -> AsyncIterator[AgentEvent]:
            yield self._event(session, turn_id, AgentEventType.TURN_STARTED, TurnStartedPayload())
            while True:
                msg = await handle["queue"].get()
                self._append_log(handle, msg)
                if msg.get("kind") == "bridge_exited":
                    yield self._bridge_exited(
                        session, turn_id, msg.get("exitCode"),
                        handle["peer"].stderr_text(),
                    )
                    return
                if msg.get("turn") != turn_id:
                    continue
                for event in self._map_frame(session, turn_id, handle, msg):
                    yield event
                if msg.get("kind") == "turn_end":
                    return

        async for event in runner.stream(source()):
            yield event

    async def steer(self, session: AgentSessionRef, input: AgentInput) -> CommandReceipt:
        handle = self._handles.get(session.agent_session_id)
        if handle is None or not isinstance(input, SteerInput):
            return self.unsupported_receipt("steer", "steer", session=session)
        try:
            data = await self._rpc(handle["peer"], "steer", {
                "text": input.text,
                **({"turn": input.payload.expected_turn_id} if input.payload.expected_turn_id else {}),
            }, timeout=60)
        except CursorSdkFailure as exc:
            return self._receipt_error(session, exc)
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session", id=session.agent_session_id),
            output={"outcome": data.get("outcome")},
        )

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        handle = self._handles.get(session.agent_session_id)
        if handle is None:
            return self.unsupported_receipt("interrupt", "interrupt", session=session)
        try:
            data = await self._rpc(handle["peer"], "cancel", timeout=30)
        except CursorSdkFailure as exc:
            if exc.code == "cursor_sdk.no_active_turn":
                return CommandReceipt(
                    command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
                    aggregate=AggregateRef(type="agent_session", id=session.agent_session_id),
                    output={"cancelled": False},
                )
            return self._receipt_error(session, exc)
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session", id=session.agent_session_id),
            output={"cancelled": bool(data.get("cancelled"))},
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._handles.get(session.agent_session_id)
        agent_id = str((handle or {}).get("agent_id") or "")
        await self._stop_peer(session.agent_session_id)
        return EXIT_RESUMABLE if agent_id else EXIT_FAILED

    async def _abort_turn(self, session: AgentSessionRef, _failure: Any) -> None:
        handle = self._handles.get(session.agent_session_id)
        if handle is None:
            return
        try:
            await self._rpc(handle["peer"], "cancel", timeout=15)
        except (CursorSdkFailure, PeerClosedError):
            return

    async def _stop_peer(self, agent_session_id: str) -> None:
        handle = self._handles.pop(agent_session_id, None)
        if handle is None:
            return
        peer: StdioJsonlPeer = handle["peer"]
        try:
            if peer.running:
                await self._rpc(peer, "close", timeout=10)
        except (CursorSdkFailure, PeerClosedError):
            pass
        await peer.close()
        watcher = handle.get("watcher")
        if watcher is not None and not watcher.done():
            watcher.cancel()

    async def _ensure_installed(self) -> Path:
        async with _INSTALL_LOCK:
            return await asyncio.to_thread(self._install_sync)

    def _install_sync(self) -> Path:
        if not (_BRIDGE_DIR / "package.json").is_file() or not (_BRIDGE_DIR / "bridge.mjs").is_file():
            raise CursorSdkFailure(
                "cursor_sdk.not_installed",
                "cursor SDK bridge sources are missing from the installation",
                category="validation",
            )
        root = self._runtime_root / SDK_VERSION
        root.mkdir(parents=True, exist_ok=True)
        for name in ("package.json", "package-lock.json", "bridge.mjs"):
            source = _BRIDGE_DIR / name
            target = root / name
            if source.is_file() and (
                not target.is_file() or source.read_bytes() != target.read_bytes()
            ):
                shutil.copy2(source, target)
        marker = root / "node_modules" / "@cursor" / "sdk" / "package.json"
        installed = ""
        if marker.is_file():
            try:
                installed = str(json.loads(marker.read_text(encoding="utf-8")).get("version") or "")
            except json.JSONDecodeError:
                installed = ""
        if installed != SDK_VERSION:
            npm = shutil.which("npm")
            if not npm:
                raise CursorSdkFailure(
                    "cursor_sdk.not_installed", "npm is not installed",
                    category="validation",
                )
            cache = self._runtime_root / "npm-cache"
            cache.mkdir(parents=True, exist_ok=True)
            env = {
                key: os.environ[key]
                for key in ("PATH", "HOME", "USER", "LANG", "TMPDIR",
                            "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY",
                            "NO_PROXY", "no_proxy")
                if key in os.environ
            }
            env["npm_config_cache"] = str(cache)
            env["npm_config_update_notifier"] = "false"
            try:
                completed = subprocess.run(
                    [npm, "ci", "--no-audit", "--no-fund"],
                    cwd=root, env=env, capture_output=True, text=True, check=False,
                    timeout=_INSTALL_TIMEOUT_S,
                )
            except (OSError, subprocess.TimeoutExpired) as exc:
                def output_text(value: Any) -> str:
                    if isinstance(value, bytes):
                        return value.decode("utf-8", errors="replace")
                    return str(value or "")

                detail = "\n".join(part for part in (
                    str(exc), output_text(getattr(exc, "stdout", None)),
                    output_text(getattr(exc, "stderr", None)),
                ) if part)
                raise CursorSdkFailure(
                    "cursor_sdk.not_installed",
                    f"npm ci of {SDK_PACKAGE}@{SDK_VERSION} did not run to completion ({type(exc).__name__})",
                    detail=detail,
                    category="validation",
                ) from exc
            if completed.returncode != 0 or not marker.is_file():
                raise CursorSdkFailure(
                    "cursor_sdk.not_installed",
                    f"npm ci of {SDK_PACKAGE}@{SDK_VERSION} failed",
                    detail=redact_secrets((completed.stdout or "") + (completed.stderr or "")),
                    category="validation",
                )
        return root

    async def _with_bridge(
        self, key: Optional[str], *, request_models: bool,
    ) -> dict[str, Any]:
        installed = await self._ensure_installed()
        env = dict(subprocess_environment(self._env_extra) or {})
        if key:
            env["CURSOR_API_KEY"] = key
        else:
            env.pop("CURSOR_API_KEY", None)
        peer = StdioJsonlPeer(
            [self.probe_binary(), str(installed / "bridge.mjs")],
            cwd=str(installed),
            env=env,
            label="cursor-sdk-probe",
            is_response=lambda msg: isinstance(msg, dict) and msg.get("type") == "response",
            request_envelope=_bridge_envelope,
            owner_adapter_id=self.identity.adapter_id,
            stderr_log_root=self._runtime_root / "logs",
        )
        await peer.start()
        try:
            hello = await self._rpc(peer, "hello", timeout=60)
            if not request_models:
                return hello
            listed = await self._rpc(peer, "models", timeout=60)
            listed["hello"] = hello
            return listed
        finally:
            try:
                if peer.running:
                    await self._rpc(peer, "close", timeout=10)
            except (CursorSdkFailure, PeerClosedError):
                pass
            await peer.close()

    def _api_key(self, request: Optional[SessionStart]) -> str:
        env: dict[str, str] = {}
        if request is not None:
            env.update(request.options.env)
        env.update(self._env_extra)
        merged = subprocess_environment(env) or {}
        return str(merged.get("CURSOR_API_KEY") or "").strip()

    async def _rpc(
        self, peer: StdioJsonlPeer, op: str, params: Optional[dict[str, Any]] = None,
        *, timeout: float,
    ) -> dict[str, Any]:
        try:
            response = await peer.request(op, params or {}, timeout=timeout)
        except asyncio.TimeoutError as exc:
            raise CursorSdkFailure(
                "cursor_sdk.rpc_timeout", f"Cursor SDK {op} timed out after {timeout:g}s",
                detail="\n".join(filter(None, [str(exc), peer.stderr_text()])),
                category="transport", retryable=True,
            ) from exc
        except PeerClosedError as exc:
            raise CursorSdkFailure(
                "cursor_sdk.bridge_exited",
                "Cursor SDK bridge exited during " + op,
                detail=exc.stderr or "",
                category="runtime_exited",
            ) from exc
        if not isinstance(response, dict) or response.get("type") != "response":
            raise CursorSdkFailure(
                "cursor_sdk.protocol", f"{op} response is not a bridge frame",
                detail=redact_secrets(json.dumps(response, ensure_ascii=False, default=str)),
                category="validation",
            )
        if response.get("ok"):
            data = response.get("data")
            return data if isinstance(data, dict) else {"value": data}
        error = response.get("error") if isinstance(response.get("error"), dict) else {}
        raise CursorSdkFailure.from_bridge_error(
            error, default_code="cursor_sdk.sdk_error", default_message=op)

    def _map_frame(
        self, session: AgentSessionRef, turn_id: str,
        handle: dict[str, Any], msg: dict[str, Any],
    ) -> list[AgentEvent]:
        kind = str(msg.get("kind") or "")
        if kind == "nested":
            inner = msg.get("update") if isinstance(msg.get("update"), dict) else {}
            parent = str(msg.get("parentCallId") or "")
            return self._map_frame(session, turn_id, handle, {**inner, "turn": turn_id, "_parent": parent})
        parent = str(msg.get("_parent") or "")
        if not parent and kind in {"text", "tool_start", "tool_end", "tool_output", "step_end", "thinking_end", "turn_end"}:
            completed = self._complete_thinking(session, turn_id, handle, msg.get("durationMs") if kind == "thinking_end" else None)
            # Complete before the next item, as CursorAdapterV2 does. The same
            # item id replaces the accumulated deltas; no empty text is made up.
            if kind in {"thinking_end", "step_end"}:
                return completed
            if completed:
                return completed + self._map_frame(session, turn_id, handle, msg)
        if kind == "text" and msg.get("text"):
            text = str(msg["text"])
            if parent:
                return [self._agent_patch(session, turn_id, parent, text, status="running")]
            handle["text"].append(text)
            return [self._event(
                session, turn_id, AgentEventType.MESSAGE_DELTA,
                MessageDeltaPayload(text=text),
            )]
        if kind == "thinking" and msg.get("text"):
            text = str(msg["text"])
            if parent:
                return [self._event(
                    session, turn_id, AgentEventType.AGENT_UPDATED,
                    AgentUpdatedPayload(agents=[AgentNodePayload(
                        agent_id=parent, call_id=parent, status="running", activity=text,
                    )]),
                )]
            handle["thinking"].append(text)
            return [self._event(
                session, turn_id, AgentEventType.REASONING_SUMMARY,
                ReasoningPayload(text=text, channel="thinking", partial=True, item_id=f"{turn_id}:thinking:{handle.get('thinking_index', 0)}"),
            )]
        if kind == "tool_start":
            return self._tool_start(session, turn_id, handle, msg)
        if kind == "tool_end":
            return self._tool_end(session, turn_id, handle, msg)
        if kind == "tool_output":
            call_id = str(msg.get("callId") or "")
            if not call_id:
                return []
            return [self._event(
                session, turn_id, AgentEventType.TOOL_PROGRESS,
                ToolPayload(
                    tool_call_id=call_id, status="running",
                    chunk=json.dumps(msg.get("event"), ensure_ascii=False, default=str),
                    agent_id=parent or None,
                ),
            )]
        if kind == "turn_end":
            return self._turn_end(session, turn_id, handle, msg)
        if kind == "bridge_warning":
            detail = str(msg.get("detail") or "")
            message = str(msg.get("message") or "Cursor SDK bridge warning")
            return [self._event(
                session, turn_id, AgentEventType.RUNTIME_WARNING,
                RuntimeWarningPayload(kind="protocol",
                                      code=str(msg.get("code") or "cursor_sdk.bridge_warning"),
                                      message=message + ("\n" + detail if detail else "")),
            )]
        return []

    def _complete_thinking(
        self, session: AgentSessionRef, turn_id: str, handle: dict[str, Any], duration: Any,
    ) -> list[AgentEvent]:
        text = "".join(handle.get("thinking") or [])
        if not text:
            return []
        index = handle.get("thinking_index", 0)
        handle["thinking"] = []
        handle["thinking_index"] = index + 1
        return [self._event(session, turn_id, AgentEventType.REASONING_SUMMARY,
                           ReasoningPayload(text=text, channel="thinking", partial=False,
                                            item_id=f"{turn_id}:thinking:{index}",
                                            duration_ms=duration if isinstance(duration, int) and not isinstance(duration, bool) else None))]

    def _tool_start(
        self, session: AgentSessionRef, turn_id: str,
        handle: dict[str, Any], msg: dict[str, Any],
    ) -> list[AgentEvent]:
        call_id = str(msg.get("callId") or new_id("tool"))
        parent = str(msg.get("_parent") or "")
        events = [self._event(
            session, turn_id, AgentEventType.TOOL_STARTED,
            ToolPayload(
                tool_call_id=call_id,
                name=str(msg.get("name") or "tool"),
                kind=str(msg.get("toolKind") or "other"),
                input=msg.get("input"),
                status="running",
                parent_tool_call_id=parent or None,
                agent_id=parent or None,
            ),
        )]
        subagent = msg.get("subagent") if isinstance(msg.get("subagent"), dict) else None
        if subagent is not None:
            events.append(self._event(
                session, turn_id, AgentEventType.AGENT_UPDATED,
                AgentUpdatedPayload(agents=[AgentNodePayload(
                    agent_id=call_id,
                    call_id=call_id,
                    title=str(subagent.get("description") or "") or None,
                    role=str(subagent.get("role") or "") or None,
                    model=str(subagent.get("model") or "") or None,
                    request=str(subagent.get("prompt") or "") or None,
                    status="running",
                )]),
            ))
        return events

    def _tool_end(
        self, session: AgentSessionRef, turn_id: str,
        _handle: dict[str, Any], msg: dict[str, Any],
    ) -> list[AgentEvent]:
        call_id = str(msg.get("callId") or new_id("tool"))
        parent = str(msg.get("_parent") or "")
        status = "failed" if msg.get("status") == "failed" else "completed"
        events = [self._event(
            session, turn_id, AgentEventType.TOOL_COMPLETED,
            ToolPayload(
                tool_call_id=call_id,
                name=str(msg.get("tool") or "") or None,
                output=msg.get("output"),
                status=status,
                error=redact_secrets(str(msg.get("error") or "")) or None,
                exit_code=msg.get("exitCode") if isinstance(msg.get("exitCode"), int) else None,
                duration_ms=msg.get("durationMs") if isinstance(msg.get("durationMs"), int) else None,
                parent_tool_call_id=parent or None,
                agent_id=parent or None,
            ),
        )]
        subagent = msg.get("subagent") if isinstance(msg.get("subagent"), dict) else None
        if subagent is not None:
            background = subagent.get("isBackground") is True
            events.append(self._event(
                session, turn_id, AgentEventType.AGENT_UPDATED,
                AgentUpdatedPayload(agents=[AgentNodePayload(
                    agent_id=call_id,
                    call_id=call_id,
                    status="running" if background else ("failed" if status == "failed" else "completed"),
                    result=str(subagent.get("text") or "") or None,
                    duration_ms=subagent.get("durationMs") if isinstance(subagent.get("durationMs"), int) else None,
                    session_ref=str(subagent.get("agentId") or "") or None,
                    activity="background" if background else None,
                )]),
            ))
        plan = msg.get("plan")
        todos = msg.get("todos")
        if isinstance(plan, str) and plan.strip():
            events.append(self._event(
                session, turn_id, AgentEventType.PLAN_UPDATED,
                PlanPayload(explanation=plan, tasks=[PlanTaskPayload(task_id=call_id, title="plan")]),
            ))
        if isinstance(todos, list) and todos:
            tasks = []
            for index, todo in enumerate(todos):
                if not isinstance(todo, dict):
                    continue
                title = str(todo.get("content") or todo.get("title") or todo.get("id") or "").strip()
                if not title:
                    continue
                raw_status = str(todo.get("status") or "pending")
                tasks.append(PlanTaskPayload(
                    task_id=str(todo.get("id") or f"{call_id}:{index}"),
                    title=title,
                    status=raw_status if raw_status in _PLAN_STATUSES else "pending",
                ))
            if tasks:
                events.append(self._event(
                    session, turn_id, AgentEventType.PLAN_UPDATED,
                    PlanPayload(tasks=tasks, patch=True),
                ))
        return events

    def _turn_end(
        self, session: AgentSessionRef, turn_id: str,
        handle: dict[str, Any], msg: dict[str, Any],
    ) -> list[AgentEvent]:
        events: list[AgentEvent] = []
        text = "".join(handle.get("text") or [])
        # T3 preserves RunResult.result when no text deltas were delivered.
        # This is the SDK's actual answer, including its complete body.
        result = msg.get("result")
        if not text and isinstance(result, str) and result:
            text = result
            handle.setdefault("text", []).append(text)
            events.append(self._event(
                session, turn_id, AgentEventType.MESSAGE_DELTA,
                MessageDeltaPayload(text=text),
            ))
        if text:
            events.append(self._event(
                session, turn_id, AgentEventType.MESSAGE_COMPLETED,
                MessageCompletedPayload(text=text),
            ))
        usage = msg.get("usage") if isinstance(msg.get("usage"), dict) else None
        if usage:
            events.append(self._event(
                session, turn_id, AgentEventType.USAGE_UPDATED,
                UsagePayload(
                    scope="turn",
                    usage_id=turn_id,
                    input_tokens=_token(usage.get("inputTokens")),
                    output_tokens=_token(usage.get("outputTokens")),
                    cached_input_tokens=_token(usage.get("cacheReadTokens") or usage.get("cacheRead")),
                    cache_write_tokens=_token(usage.get("cacheWriteTokens") or usage.get("cacheWrite")),
                ),
            ))
        status = str(msg.get("status") or "")
        if status == "cancelled":
            events.append(self._failed(session, CursorSdkFailure(
                "cursor_sdk.aborted", "turn cancelled", category="cancelled",
            ), turn_id=turn_id))
            return events
        if status == "error":
            error = msg.get("error") if isinstance(msg.get("error"), dict) else {}
            events.append(self._failed(session, CursorSdkFailure.from_bridge_error(
                error, default_code="cursor_sdk.run_failed", default_message="run failed",
            ), turn_id=turn_id))
            return events
        if status != "finished":
            events.append(self._failed(session, CursorSdkFailure(
                "cursor_sdk.protocol", "SDK returned an unknown terminal status",
                detail=json.dumps(msg, ensure_ascii=False, default=str), category="validation",
            ), turn_id=turn_id))
            return events
        events.append(self._event(
            session, turn_id, AgentEventType.TURN_COMPLETED,
            TurnCompletedPayload(
                stop_reason=status or "finished",
                duration_ms=msg.get("durationMs") if isinstance(msg.get("durationMs"), int) else None,
            ),
        ))
        return events

    def _agent_patch(
        self, session: AgentSessionRef, turn_id: str, agent_id: str,
        text: str, *, status: str,
    ) -> AgentEvent:
        handle = self._handles.get(session.agent_session_id) or {}
        bucket: dict[str, str] = handle.setdefault("nested", {})
        bucket[agent_id] = bucket.get(agent_id, "") + text
        return self._event(
            session, turn_id, AgentEventType.AGENT_UPDATED,
            AgentUpdatedPayload(agents=[AgentNodePayload(
                agent_id=agent_id, call_id=agent_id, status=status,  # type: ignore[arg-type]
                result=bucket[agent_id], activity=text,
            )]),
        )

    def _bridge_exited(
        self, session: AgentSessionRef, turn_id: Optional[str],
        exit_code: Any, stderr: str,
    ) -> AgentEvent:
        failure = self.failure(
            FailureCategory.RUNTIME_EXITED, "bridge_exited",
            message="Cursor SDK bridge exited before the turn finished",
            detail=redact_secrets(stderr or ""),
            native_code="cursor_sdk.bridge_exited",
        )
        event = self._event(
            session, turn_id, AgentEventType.TURN_FAILED,
            TurnFailedPayload(error=failure), native="cursor_sdk.bridge_exited",
        )
        self.emit(build_event(
            AgentEventType.RUNTIME_EXITED,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            turn_id=turn_id,
            native_type="cursor_sdk.bridge_exited",
            payload=RuntimeExitedPayload(
                classification=EXIT_FAILED,
                exit_code=exit_code if isinstance(exit_code, int) else None,
                error=failure,
            ),
        ))
        return event

    def _failed(
        self, session: AgentSessionRef, exc: CursorSdkFailure, *, turn_id: Optional[str] = None,
    ) -> AgentEvent:
        return self._event(
            session, turn_id, AgentEventType.TURN_FAILED,
            TurnFailedPayload(error=self.failure(
                _failure_category(exc.category),
                exc.code.removeprefix("cursor_sdk."),
                message=str(exc),
                detail=exc.detail,
                retryable=exc.retryable,
                native_code=exc.native_code or exc.code,
            )),
            native=exc.code,
        )

    def _event(
        self, session: AgentSessionRef, turn_id: Optional[str],
        event_type: AgentEventType, payload: Any, *, native: str = "cursor.sdk",
    ) -> AgentEvent:
        record = self.session_record(session.agent_session_id)
        return self.emit(build_event(
            event_type,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            run_id=record.run_id if record else None,
            execution_generation=record.execution_generation if record else None,
            turn_id=turn_id,
            native_type=native,
            payload=dump_payload(payload) if hasattr(payload, "model_dump") else payload,
        ))

    def _receipt_error(self, session: AgentSessionRef, exc: CursorSdkFailure) -> CommandReceipt:
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.FAILED,
            aggregate=AggregateRef(type="agent_session", id=session.agent_session_id),
            error=ErrorEnvelope(
                code=exc.code,
                message=str(exc),
                category=ErrorCategory.RUNTIME,
                retryable=exc.retryable,
                detail={"category": exc.category},
            ),
        )

    @staticmethod
    def _append_log(handle: dict[str, Any], msg: dict[str, Any]) -> None:
        path: Path = handle["log_path"]
        line = redact_secrets(json.dumps(msg, ensure_ascii=False, default=str))
        with path.open("a", encoding="utf-8") as stream:
            stream.write(line + "\n")


def _bridge_envelope(msg_id: str, method: str, params: Optional[dict[str, Any]]) -> dict[str, Any]:
    body = {"id": msg_id, "op": method}
    if params:
        body.update(params)
    return body


async def _enqueue(queue: asyncio.Queue[dict[str, Any]], msg: dict[str, Any]) -> None:
    if msg.get("type") == "event":
        await queue.put(msg)


async def _watch_exit(peer: StdioJsonlPeer, queue: asyncio.Queue[dict[str, Any]]) -> None:
    try:
        code = await peer.wait_exit()
    except PeerClosedError:
        code = None
    await queue.put({"type": "event", "kind": "bridge_exited", "exitCode": code})


def _message_body(input: MessageInput) -> tuple[str, list[dict[str, str]]]:
    parts = []
    if input.payload.capability_context:
        parts.append(input.payload.capability_context)
    if input.text:
        parts.append(input.text)
    images: list[dict[str, str]] = []
    notes: list[str] = []
    for attachment in input.payload.attachments:
        if attachment.delivery == "native_image" and attachment.content_base64:
            images.append({
                "data": attachment.content_base64,
                "mimeType": attachment.media_type or "application/octet-stream",
            })
        elif attachment.path or attachment.workspace_path:
            notes.append(attachment.workspace_path or attachment.path)
    if notes:
        parts.append("附件路径：\n" + "\n".join(notes))
    return "\n\n".join(parts), images


def _token(value: Any) -> Optional[int]:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, float) and value >= 0:
        return int(value)
    return None


__all__ = ["CursorSdkAdapter", "CursorSdkFailure", "SDK_VERSION"]
