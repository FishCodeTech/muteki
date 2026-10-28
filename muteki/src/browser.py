from __future__ import annotations

import hashlib
import io
import json
import os
import re
import subprocess
import uuid
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from PIL import Image

from muteki.platform.contracts.base import new_id, utcnow

from .artifacts import SrcArtifactStore
from .objects import SrcObjectService
from .store import SrcStore, SrcStoreError

_CONTEXT_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")
SHARED_LABEL = "muteki-shared"


def _token(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()[:24]


class AgentBrowserTaskDriver:
    """Task-owned agent-browser daemon/profile adapter.

    The daemon HOME, profile, session namespace and socket are derived from the
    trusted Task/context, never from a Run's private HOME or model arguments.
    """

    def __init__(self, executable: str = "/usr/local/bin/agent-browser") -> None:
        self.executable = executable

    def _env(self, task_root: Path, context_id: str, profile_path: Path) -> dict[str, str]:
        session = f"s-{_token(str(task_root))[:12]}-{_token(context_id)[:8]}"
        home = task_root / "home"
        socket_dir = Path("/tmp") / f"sb-{_token(session)[:12]}"
        home.mkdir(mode=0o700, parents=True, exist_ok=True)
        socket_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
        env = os.environ.copy()
        env.update({
            "HOME": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_CACHE_HOME": str(home / ".cache"),
            "XDG_DATA_HOME": str(home / ".local" / "share"),
            "AGENT_BROWSER_SESSION": session,
            "AGENT_BROWSER_NAMESPACE": session,
            "AGENT_BROWSER_PROFILE": str(profile_path),
            "AGENT_BROWSER_SOCKET_DIR": str(socket_dir),
            "AGENT_BROWSER_EXECUTABLE_PATH": os.environ.get("MUTEKI_SRC_BROWSER_EXECUTABLE", "/usr/bin/chromium"),
        })
        return env

    def _run(
        self, task_root: Path, context_id: str, profile_path: Path,
        *args: str, timeout: int = 60,
    ) -> subprocess.CompletedProcess[str]:
        try:
            result = subprocess.run(
                [self.executable, *args], capture_output=True, text=True,
                check=False, timeout=timeout,
                env=self._env(task_root, context_id, profile_path),
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise SrcStoreError(
                "capability_unavailable", f"Task browser command failed: {type(exc).__name__}: {exc}",
            ) from exc
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
            raise SrcStoreError("capability_unavailable", f"Task browser {args[0]} failed: {detail}")
        return result

    def ensure_context(self, task_root: Path, context_id: str, profile_path: Path) -> dict[str, Any]:
        self._run(task_root, context_id, profile_path, "tab", "list", "--json")
        return {
            "driver": "agent-browser", "daemon": "task-context-owned",
            "profile_path": str(profile_path), "session": f"s-{_token(str(task_root))[:12]}-{_token(context_id)[:8]}",
        }

    def tabs(self, task_root: Path, context_id: str, profile_path: Path) -> list[dict[str, Any]]:
        result = self._run(task_root, context_id, profile_path, "tab", "list", "--json")
        try:
            payload = json.loads(result.stdout)
        except ValueError as exc:
            raise SrcStoreError("capability_unavailable", "Task browser returned invalid tab JSON") from exc
        if not payload.get("success"):
            raise SrcStoreError("capability_unavailable", f"Task browser tab list failed: {payload.get('error')}")
        return list(payload.get("data", {}).get("tabs", []))

    def open_tab(
        self, task_root: Path, context_id: str, profile_path: Path, *, label: str,
    ) -> dict[str, Any]:
        tabs = self.tabs(task_root, context_id, profile_path)
        existing = next((tab for tab in tabs if tab.get("label") == label), None)
        if existing is None:
            self._run(task_root, context_id, profile_path, "tab", "new", "--label", label)
        else:
            self._run(task_root, context_id, profile_path, "tab", label)
        return next(
            (tab for tab in self.tabs(task_root, context_id, profile_path) if tab.get("label") == label),
            {"label": label},
        )

    def capture_screenshot(
        self, task_root: Path, context_id: str, profile_path: Path, *,
        tab_id: str, label: str,
    ) -> dict[str, Any]:
        self._run(task_root, context_id, profile_path, "tab", label)
        capture_root = task_root / "captures"
        capture_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        path = capture_root / f"{uuid.uuid4().hex}.png"
        self._run(task_root, context_id, profile_path, "screenshot", str(path))
        try:
            data = path.read_bytes()
        except OSError as exc:
            raise SrcStoreError("artifact_missing", "Task browser did not produce a screenshot") from exc
        finally:
            path.unlink(missing_ok=True)
        url_result = self._run(task_root, context_id, profile_path, "get", "url")
        request_ids: list[str] = []
        capture_errors: list[str] = []
        try:
            network = self._run(task_root, context_id, profile_path, "network", "requests", "--json")
            payload = json.loads(network.stdout)
            if payload.get("success"):
                request_ids = list(dict.fromkeys(
                    str(item["requestId"]) for item in payload.get("data", {}).get("requests", [])
                    if isinstance(item, dict) and item.get("requestId")
                ))
            else:
                capture_errors.append("network_requests_unavailable")
        except (SrcStoreError, ValueError, TypeError):
            capture_errors.append("network_requests_unavailable")
        return {
            "data": data, "url": url_result.stdout.strip(),
            "request_ids": request_ids, "capture_errors": capture_errors,
        }

    def run_finished(self, task_root: Path, context_id: str, profile_path: Path, run_id: str) -> None:
        return None

    def close_task(self, task_root: Path, context_id: str, profile_path: Path) -> None:
        self._run(task_root, context_id, profile_path, "quit")


class SrcTaskBrowserService:
    def __init__(
        self, store: SrcStore, artifacts: SrcArtifactStore, *,
        browser_root: Optional[Path] = None, driver: Optional[Any] = None,
        policy_provider: Optional[Callable[[str], Any]] = None,
        egress: Optional[Any] = None,
        reviewer_reader: Optional[Callable[[bytes], Awaitable[Any] | Any]] = None,
    ) -> None:
        self.store = store
        self.artifacts = artifacts
        self.objects = SrcObjectService(store, artifacts)
        self.browser_root = Path(browser_root or artifacts.root.parent / "browser").resolve()
        self.browser_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.driver = driver
        self.policy_provider = policy_provider
        self.egress = egress
        self.reviewer_reader = reviewer_reader

    def set_driver(self, driver: Any) -> None:
        self.driver = driver

    def set_reviewer_reader(
        self, reader: Callable[[bytes], Awaitable[Any] | Any],
    ) -> None:
        self.reviewer_reader = reader

    def _driver(self) -> Any:
        if self.driver is None:
            raise SrcStoreError("capability_unavailable", "no Task browser driver is configured")
        return self.driver

    def environment(self, task_id: str) -> dict[str, Any]:
        if self.policy_provider is None:
            return {"network_mode": "declared", "inspection_mode": "off", "coverage_limitations": [], "policy_digest": ""}
        policy = self.policy_provider(task_id)
        inspection_mode = str(policy.inspection_requirements.get("mode") or "off")
        current = self.egress.current(task_id, policy) if self.egress is not None else {}
        limitations: list[str] = []
        if policy.network_mode == "enforced" and not current.get("network_enforced"):
            raise SrcStoreError("capability_unavailable", "Task browser gateway is not verified")
        if inspection_mode == "enforce" and not current.get("content_enforced"):
            raise SrcStoreError("inspection_unavailable", "Task browser content inspection is not verified")
        if inspection_mode == "record":
            limitations.append("tls_passthrough_uninspected")
            if not current.get("content_enforced"):
                limitations.append("inspection_proxy_unverified")
        network = str(current.get("network") or "")
        gateway_mount = ""
        if policy.network_mode == "enforced" or inspection_mode in {"record", "enforce"}:
            if self.egress is None:
                raise SrcStoreError("capability_unavailable", "Task browser gateway is not configured")
            gateway_mount, _name = self.egress.mount_for_gateway(network)
        ca_path = ""
        if inspection_mode == "enforce":
            ca_path = "/run/src/ca.pem" if policy.network_mode == "enforced" else str(Path(gateway_mount) / "ca.pem")
        return {
            "network_mode": policy.network_mode,
            "network": network,
            "gateway_mount": gateway_mount,
            "http_proxy": str(current.get("http_proxy") or ""),
            "ca_path": ca_path,
            "inspection_mode": inspection_mode,
            "policy_digest": str((current.get("health") or {}).get("digest") or policy.digest),
            "environment_generation": current.get("generation"),
            "coverage_limitations": limitations,
        }

    def capabilities(self, task_id: str) -> dict[str, Any]:
        try:
            environment = self.environment(task_id)
        except SrcStoreError as exc:
            return {
                "pre_dispatch_gate": True,
                "environment_boundary": "unavailable",
                "network_scope": ["declared"],
                "content_inspection": ["off"],
                "observed_resource_fields": ["page_url", "context_id", "browser_generation"],
                "activation_error": exc.message,
                "error_code": exc.code,
            }
        enforced = environment["network_mode"] == "enforced"
        inspection = environment["inspection_mode"]
        limitations = list(environment["coverage_limitations"])
        for context in self.store.all(task_id, "browser_context"):
            limitations.extend((context.payload.get("driver") or {}).get("coverage_limitations") or [])
            limitations.extend((context.payload.get("environment") or {}).get("coverage_limitations") or [])
        limitations = list(dict.fromkeys(limitations))
        return {
            "trusted_input_fields": ["action", "url", "selector", "context_id"],
            "observed_resource_fields": ["page_url", "request_url", "context_id", "browser_generation", "connected_ip", "method", "path", "headers", "body_sha256"],
            "pre_dispatch_gate": True,
            "environment_boundary": "task-browser-container-netns" if enforced else "task-browser-declared",
            "network_scope": ["declared", "enforced"] if enforced else ["declared"],
            "content_inspection": [inspection] if inspection != "enforce" or not limitations else ["record"],
            "cancellation": "best-effort", "receipt_support": "browser-action",
            "coverage_limitations": limitations,
        }

    def _configure_driver(self, task_id: str) -> dict[str, Any]:
        environment = self.environment(task_id)
        configure = getattr(self._driver(), "configure", None)
        if configure is not None:
            configure(self.task_root(task_id), environment)
        return environment

    def task_root(self, task_id: str) -> Path:
        root = self.browser_root / _token(task_id)
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        if root.is_symlink() or not root.resolve().is_relative_to(self.browser_root):
            raise SrcStoreError("task_binding_invalid", "Task browser root escapes its managed directory")
        return root

    def ensure_task(self, task_id: str) -> dict[str, Any]:
        existing = self.store.by_natural_key(task_id, "browser_service", "task")
        if existing:
            return existing.model_dump(mode="json")
        record = self.store.put(
            task_id, "browser_service", {
                "owner": "task", "state": "unverified",
                "driver": type(self.driver).__name__ if self.driver is not None else "unavailable",
            }, record_id=SrcStore.digest(task_id)[:24], natural_key="task", generation=1,
        )
        return record.model_dump(mode="json")

    def service(self, task_id: str) -> Any:
        return self.store.require(
            task_id, "browser_service", SrcStore.digest(task_id)[:24],
        )

    def generation(self, task_id: str) -> int:
        return self.service(task_id).generation

    def ensure_context(self, task_id: str, context_id: str = "default") -> dict[str, Any]:
        if not _CONTEXT_ID.fullmatch(context_id):
            raise SrcStoreError("task_binding_invalid", "invalid Task browser context_id")
        service = self.ensure_task(task_id)
        environment = self._configure_driver(task_id)
        existing = self.store.by_natural_key(task_id, "browser_context", context_id)
        if (
            existing and existing.payload.get("state") == "ready"
            and existing.payload.get("environment", {}).get("policy_digest") == environment["policy_digest"]
        ):
            return existing.model_dump(mode="json")
        profile_path = self.task_root(task_id) / "profiles" / _token(context_id)
        profile_path.mkdir(mode=0o700, parents=True, exist_ok=True)
        context = existing or self.store.put(
            task_id, "browser_context", {
                "context_id": context_id, "profile_path": str(profile_path),
                "browser_generation": service["generation"], "recovery": "new",
                "state": "pending", "last_verified_page": None,
            }, record_id=f"{SrcStore.digest(task_id)[:24]}:{context_id}", natural_key=context_id,
            generation=service["generation"],
        )
        try:
            actual = self._driver().ensure_context(self.task_root(task_id), context_id, profile_path)
        except Exception as exc:
            self.store.put(
                task_id, "browser_context", {
                    **context.payload, "state": "unavailable", "environment": environment,
                    "error": f"{type(exc).__name__}: {exc}",
                }, record_id=context.record_id, expected_revision=context.revision,
                natural_key=context_id, generation=service["generation"],
            )
            raise
        return self.store.put(
            task_id, "browser_context", {
                **context.payload, "state": "ready", "driver": actual,
                "environment": {
                    **environment,
                    "coverage_limitations": list(dict.fromkeys([
                        *environment.get("coverage_limitations", []),
                        *actual.get("coverage_limitations", []),
                    ])),
                },
            }, record_id=context.record_id, expected_revision=context.revision,
            natural_key=context_id, generation=service["generation"],
        ).model_dump(mode="json")

    def context(self, task_id: str, context_id: str) -> Any:
        record = self.store.by_natural_key(task_id, "browser_context", context_id)
        if record is None:
            raise SrcStoreError("artifact_missing", "Task browser context is unavailable")
        return record

    def recover(self, task_id: str, *, profile_readable: bool = True) -> dict[str, Any]:
        current = self.service(task_id)
        generation = current.generation + 1
        service = self.store.put(
            task_id, "browser_service", {
                **current.payload, "state": "ready" if profile_readable else "profile_unavailable",
                "recovered_at": utcnow().isoformat(),
            }, record_id=SrcStore.digest(task_id)[:24], expected_revision=current.revision,
            natural_key="task", generation=generation,
        )
        contexts = []
        for row in self.store.all(task_id, "browser_context"):
            recovery = "session_recreated" if profile_readable else "profile_unavailable"
            updated = self.store.put(
                task_id, "browser_context", {
                    **row.payload, "browser_generation": generation,
                    "recovery": recovery, "session_storage_restored": False,
                    "verification_required": True,
                }, record_id=row.record_id, expected_revision=row.revision,
                natural_key=row.natural_key, generation=generation,
            )
            contexts.append(updated.model_dump(mode="json"))
        return {
            "browser_generation": generation, "recovery": "session_recreated" if profile_readable else "profile_unavailable",
            "session_storage_restored": False, "contexts": contexts,
            "service": service.model_dump(mode="json"),
        }

    @staticmethod
    def _check_generation(actual: int, actor: dict[str, Any]) -> None:
        supplied = actor.get("browser_generation")
        if supplied is not None and int(supplied) != actual:
            raise SrcStoreError("authorization_stale", "browser generation is stale")

    def open_tab(
        self, task_id: str, context_id: str, actor: dict[str, Any], *,
        shared: bool = False, ttl_seconds: int = 600,
    ) -> dict[str, Any]:
        self.ensure_context(task_id, context_id)
        context = self.context(task_id, context_id)
        self._check_generation(context.generation, actor)
        if context.payload["state"] != "ready":
            raise SrcStoreError("capability_unavailable", f"Task browser context is {context.payload['state']}")
        participant = self.objects.participant(task_id, context_id, actor)
        label = SHARED_LABEL if shared else f"muteki-{_token(participant)[:20]}"
        lease = self.objects.acquire_lease(
            task_id, context_id, "src.browser_tab", label,
            owner=participant, ttl_seconds=ttl_seconds,
        ) if shared else None
        try:
            actual = self._driver().open_tab(
                self.task_root(task_id), context_id, Path(context.payload["profile_path"]), label=label,
            )
        except Exception:
            if lease:
                self.objects.release_lease(
                    task_id, context_id, "src.browser_tab", label,
                    owner=participant, fencing_token=lease.fencing_token,
                )
            raise
        natural = f"{context_id}:{label}"
        existing = self.store.by_natural_key(task_id, "browser_tab", natural)
        payload = {
            "tab_id": str(actual.get("tabId") or actual.get("targetId") or label),
            "label": label, "context_id": context_id, "shared": shared,
            "owner": participant, "actual": actual,
            "browser_generation": context.payload["browser_generation"],
            "environment": dict(context.payload.get("environment") or {}),
            "lease": lease.model_dump(mode="json") if lease else None,
        }
        record = self.store.put(
            task_id, "browser_tab", payload,
            record_id=existing.record_id if existing else new_id("sbrowsertab"),
            expected_revision=existing.revision if existing else None,
            parent_id=context_id, natural_key=natural,
            generation=context.payload["browser_generation"],
        )
        return record.model_dump(mode="json")

    def _use_tab(
        self, task_id: str, context_id: str, tab_id: str,
        actor: dict[str, Any], fencing_token: int = 0,
    ) -> dict[str, Any]:
        tabs = [row for row in self.store.all(task_id, "browser_tab") if row.parent_id == context_id]
        tab = next((row for row in tabs if row.payload.get("tab_id") == tab_id or row.record_id == tab_id), None)
        if tab is None:
            raise SrcStoreError("artifact_missing", "Task browser tab is unavailable")
        self._check_generation(tab.generation, actor)
        participant = self.objects.participant(task_id, context_id, actor)
        if tab.payload.get("shared"):
            lease = tab.payload.get("lease") or {}
            token = fencing_token or int(lease.get("fencing_token") or 0)
            self.objects.assert_lease(
                task_id, context_id, "src.browser_tab", tab.payload["label"],
                owner=participant, fencing_token=token,
            )
        elif tab.payload.get("owner") != participant:
            raise SrcStoreError("resource_busy", "private tab belongs to another participant")
        return tab.model_dump(mode="json")

    def release_shared_tab(
        self, task_id: str, context_id: str, tab_id: str,
        actor: dict[str, Any], fencing_token: int,
    ) -> dict[str, Any]:
        tab = self._use_tab(task_id, context_id, tab_id, actor, fencing_token)
        lease = self.objects.release_lease(
            task_id, context_id, "src.browser_tab", tab["payload"]["label"],
            owner=tab["payload"]["owner"], fencing_token=fencing_token,
        )
        current = self.store.require(task_id, "browser_tab", tab["record_id"])
        return self.store.put(
            task_id, "browser_tab", {**current.payload, "lease": lease.model_dump(mode="json")},
            record_id=current.record_id, expected_revision=current.revision,
            parent_id=context_id, natural_key=current.natural_key,
        ).model_dump(mode="json")

    def capture_screenshot(
        self, task_id: str, context_id: str, tab_id: str,
        actor: dict[str, Any], *, fencing_token: int = 0,
        action_ref: str = "", request_refs: Optional[list[str]] = None,
    ) -> dict[str, Any]:
        self.ensure_context(task_id, context_id)
        context = self.context(task_id, context_id)
        tab = self._use_tab(task_id, context_id, tab_id, actor, fencing_token)
        captured = self._driver().capture_screenshot(
            self.task_root(task_id), context_id, Path(context.payload["profile_path"]),
            tab_id=tab["payload"]["tab_id"], label=tab["payload"]["label"],
        )
        data = captured["data"]
        try:
            with Image.open(io.BytesIO(data)) as image:
                width, height = image.size
                media_type = f"image/{image.format.lower()}" if image.format else "application/octet-stream"
                image.verify()
        except Exception as exc:
            raise SrcStoreError("artifact_hash_mismatch", "browser screenshot is not a readable image") from exc
        environment = dict(context.payload.get("environment") or {})
        inspection_mode = str(environment.get("inspection_mode") or "off")
        limitations = [
            *list(environment.get("coverage_limitations") or []),
            *list(captured.get("capture_errors") or []),
        ]
        if inspection_mode == "record":
            limitations.append("record_best_effort_not_complete")
            limitations.append("non_http_or_uncaptured_subrequests_not_inspected")
        inspection = {
            "mode": inspection_mode,
            "coverage": "gateway_netns_proxy" if environment.get("network_mode") == "enforced" else "declared_proxy_only",
            "capture_state": "partial" if inspection_mode == "record" or limitations else "complete",
            "uninspected": inspection_mode == "record" or bool(limitations),
            "reasons": limitations,
            "policy_digest": environment.get("policy_digest", ""),
        }
        artifact = self.artifacts.put(
            task_id, data, media_type=media_type, name="browser-screenshot",
            source_run_id=str(actor.get("run_id") or "") or None,
            complete=inspection["capture_state"],
        )
        record = self.store.put(
            task_id, "browser_screenshot", {
                "artifact": artifact.model_dump(mode="json"),
                "sha256": artifact.sha256, "width": width, "height": height,
                "url": captured.get("url") or "", "captured_at": utcnow().isoformat(),
                "context_id": context_id,
                "identity_revision": int(actor.get("identity_revision") or 0),
                "browser_generation": context.payload["browser_generation"],
                "tab_id": tab["payload"]["tab_id"], "actor": dict(actor),
                "action_ref": action_ref,
                "request_refs": list(request_refs or captured.get("request_ids") or []),
                "capture_errors": list(captured.get("capture_errors") or []),
                "inspection": inspection,
                "review_status": "unreviewed",
            }, record_id=new_id("sscreenshot"), parent_id=context_id,
            generation=context.payload["browser_generation"],
        )
        self.record_page_state(
            task_id, context_id, actor=actor, url=str(captured.get("url") or ""),
            action=action_ref or "screenshot",
            network_events=list(captured.get("request_ids") or []),
            evidence_refs=[record.model_dump(mode="json")],
        )
        return record.model_dump(mode="json")

    def record_page_state(
        self, task_id: str, context_id: str, *, actor: dict[str, Any],
        url: str, action: str, dom: Optional[bytes] = None,
        console_events: Optional[list[dict[str, Any]]] = None,
        network_events: Optional[list[Any]] = None,
        session_state: str = "observed", frame_origin: str = "",
        evidence_refs: Optional[list[dict[str, Any]]] = None,
    ) -> dict[str, Any]:
        context = self.context(task_id, context_id)
        self._check_generation(context.generation, actor)
        artifact = self.artifacts.put(
            task_id, dom, media_type="text/html", name="browser-page.html",
            source_run_id=str(actor.get("run_id") or "") or None,
        ) if dom is not None else None
        event = self.store.put(
            task_id, "browser_page_event", {
                "context_id": context_id, "actor": dict(actor), "url": url,
                "action": action, "frame_origin": frame_origin,
                "dom_artifact": artifact.model_dump(mode="json") if artifact else None,
                "console_events": list(console_events or []),
                "network_events": list(network_events or []),
                "session_state": session_state,
                "browser_generation": context.generation,
                "evidence_refs": list(evidence_refs or []),
                "observed_at": utcnow().isoformat(),
            }, record_id=new_id("sbrowserpage"), parent_id=context_id,
            generation=context.generation,
        )
        last_page = {
            "url": url, "event_id": event.record_id,
            "observed_at": event.payload["observed_at"],
            "session_state": session_state,
        }
        self.store.put(
            task_id, "browser_context", {
                **context.payload, "last_verified_page": last_page,
                "verification_required": session_state not in {"observed", "verified"},
            }, record_id=context.record_id, expected_revision=context.revision,
            natural_key=context.natural_key, generation=context.generation,
        )
        return event.model_dump(mode="json")

    def read_review_image(
        self, task_id: str, screenshot_id: str, *, reviewer: str,
        confirm_delivery: bool = True,
    ) -> dict[str, Any]:
        screenshot = self.store.require(task_id, "browser_screenshot", screenshot_id)
        from .contracts import ArtifactRef
        ref = ArtifactRef.model_validate(screenshot.payload["artifact"])
        data = self.artifacts.read(ref, expected_task_id=task_id)
        if hashlib.sha256(data).hexdigest() != screenshot.payload["sha256"]:
            raise SrcStoreError("artifact_hash_mismatch", "screenshot changed before review")
        status = "delivered" if confirm_delivery else "prepared"
        receipt = self.store.put(
            task_id, "image_review_read", {
                "screenshot_id": screenshot_id, "artifact_sha256": ref.sha256,
                "reviewer": reviewer, "status": status,
                "media_type": ref.media_type, "size": len(data),
                "delivered_at": utcnow().isoformat() if confirm_delivery else None,
            }, record_id=new_id("simageread"), parent_id=screenshot_id,
        )
        return {
            "read_receipt": receipt.model_dump(mode="json"),
            "image_bytes": data, "media_type": ref.media_type,
            "sha256": ref.sha256, "width": screenshot.payload.get("width"),
            "height": screenshot.payload.get("height"),
        }

    def confirm_review_image_delivery(self, task_id: str, read_id: str) -> dict[str, Any]:
        read = self.store.require(task_id, "image_review_read", read_id)
        updated = self.store.put(
            task_id, "image_review_read", {
                **read.payload, "status": "delivered",
                "delivered_at": utcnow().isoformat(),
            }, record_id=read_id, expected_revision=read.revision,
            parent_id=read.parent_id,
        )
        return updated.model_dump(mode="json")

    async def review_image(
        self, task_id: str, screenshot_id: str, *, reviewer: str,
        judgment: Any = None, review_read_id: str = "",
        reviewer_reader: Optional[Callable[[bytes], Awaitable[Any] | Any]] = None,
    ) -> dict[str, Any]:
        screenshot = self.store.require(task_id, "browser_screenshot", screenshot_id)
        from .contracts import ArtifactRef
        ref = ArtifactRef.model_validate(screenshot.payload["artifact"])
        data = self.artifacts.read(ref, expected_task_id=task_id)
        if hashlib.sha256(data).hexdigest() != screenshot.payload["sha256"]:
            raise SrcStoreError("artifact_hash_mismatch", "screenshot changed before review")
        status = "reviewed"
        error = ""
        reader = reviewer_reader or self.reviewer_reader
        if reader is not None:
            try:
                judgment = reader(data)
                if hasattr(judgment, "__await__"):
                    judgment = await judgment
                if judgment in (None, "", [], {}):
                    raise SrcStoreError("capability_unavailable", "Reviewer returned an empty judgment")
            except Exception as exc:
                status = "unavailable"
                error = f"{type(exc).__name__}: {exc}"
        else:
            read = self.store.get(task_id, "image_review_read", review_read_id)
            valid_read = bool(
                read and read.payload.get("reviewer") == reviewer
                and read.payload.get("screenshot_id") == screenshot_id
                and read.payload.get("artifact_sha256") == ref.sha256
                and read.payload.get("status") == "delivered"
            )
            if not valid_read or judgment in (None, "", [], {}):
                status = "unavailable"
                error = "reviewer must read the original image and provide a judgment"
        record = self.store.put(
            task_id, "image_review", {
                "screenshot_id": screenshot_id, "artifact_sha256": screenshot.payload["sha256"],
                "reviewer": reviewer, "status": status, "judgment": judgment,
                "review_read_id": review_read_id or None,
                "error": error, "reviewed_at": utcnow().isoformat(),
            }, record_id=new_id("simagereview"), parent_id=screenshot_id,
        )
        current = self.store.require(task_id, "browser_screenshot", screenshot_id)
        self.store.put(
            task_id, "browser_screenshot", {
                **current.payload,
                "review_status": status if status == "reviewed" else "unreviewed",
                "last_image_review": record.model_dump(mode="json"),
            }, record_id=screenshot_id, expected_revision=current.revision,
            parent_id=current.payload["context_id"],
        )
        return record.model_dump(mode="json")

    def record_execution_signal(
        self, task_id: str, context_id: str, *,
        kind: str, frame_origin: str, trigger: dict[str, Any],
        evidence_refs: list[dict[str, Any]], injected: bool = False,
        normal_user_path: bool = False, csp: str = "", sandbox: str = "",
    ) -> dict[str, Any]:
        if kind not in {"reflection", "console", "injected_evaluate", "page_execution", "oast_callback"}:
            raise SrcStoreError("policy_denied", "unknown browser execution signal kind")
        verified_evidence = bool(evidence_refs) and all(
            isinstance(ref, (dict, str)) and bool(ref) for ref in evidence_refs
        )
        target_executed = (
            kind == "page_execution" and normal_user_path and not injected
            and verified_evidence
        )
        record = self.store.put(
            task_id, "browser_execution", {
                "context_id": context_id, "kind": kind, "frame_origin": frame_origin,
                "trigger": trigger, "evidence_refs": evidence_refs,
                "injected": injected, "normal_user_path": normal_user_path,
                "csp": csp, "sandbox": sandbox,
                "target_executed": target_executed,
                "conclusion": "target_page_executed" if target_executed else "not_proof_of_target_execution",
            }, record_id=new_id("sbrowserexecution"), parent_id=context_id,
        )
        return record.model_dump(mode="json")

    def run_finished(self, task_id: str, context_id: str, run_id: str) -> dict[str, Any]:
        context = self.context(task_id, context_id)
        self._driver().run_finished(self.task_root(task_id), context_id, Path(context.payload["profile_path"]), run_id)
        return {
            "task_browser_closed": False, "run_id": run_id,
            "browser_generation": context.payload["browser_generation"],
        }

    def shutdown(self) -> None:
        stop = getattr(self._driver(), "shutdown", None)
        if stop is None:
            return
        with self.store.platform.lock:
            rows = list(self.store.platform.conn.execute(
                "SELECT DISTINCT task_id FROM src_records WHERE kind='browser_service'",
            ).fetchall())
        for row in rows:
            stop(self.task_root(str(row["task_id"])))

    def close_task(self, task_id: str) -> dict[str, Any]:
        current = self.service(task_id)
        for row in self.store.all(task_id, "browser_context"):
            self._driver().close_task(self.task_root(task_id), row.payload["context_id"], Path(row.payload["profile_path"]))
        return self.store.put(
            task_id, "browser_service", {**current.payload, "state": "closed"},
            record_id=SrcStore.digest(task_id)[:24], expected_revision=current.revision,
            natural_key="task", generation=current.generation,
        ).model_dump(mode="json")


__all__ = ["AgentBrowserTaskDriver", "SrcTaskBrowserService", "SHARED_LABEL"]
