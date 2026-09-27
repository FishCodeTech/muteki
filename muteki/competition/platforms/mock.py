"""本地持久模拟比赛平台。

该 Adapter 用于产品验收和开发环境，执行与真实 PlatformAdapter 相同的
probe/sync/artifact/instance/submit/poll 接口。远端状态保存到独立 JSON
文件，因此后端重启后仍能核对幂等提交和实例状态。
"""

from __future__ import annotations

import hashlib
import json
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, Optional

from muteki.competition.models import PlatformConnection, RevisionArtifact
from muteki.competition.platforms.base import (
    PlatformAuthRequiredError,
    PlatformRateLimitedError,
    PlatformTimeoutError,
    PlatformUnknownResultError,
    RemoteChallengeSnapshot,
)
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.modules import (
    ArtifactObject,
    InstanceLeaseRef,
    InstanceResult,
    PlatformCapabilities,
    PlatformChallengeRef,
    PlatformConnectionRef,
    RemoteArtifactRef,
    SubmissionRequest,
    SubmissionResult,
    SyncRequest,
    SyncResult,
)


MOCK_FLAG = "flag{muteki-local-acceptance}"
MOCK_ARTIFACT = b"Muteki local platform acceptance artifact\n" + MOCK_FLAG.encode() + b"\n"


class MockCompetitionAdapter:
    """绑定单个本地模拟连接的持久 PlatformAdapter。"""

    id = "mock"

    def __init__(
        self,
        connection: PlatformConnection,
        *,
        state_root: str | Path,
    ) -> None:
        if connection.platform_kind != self.id:
            raise ValueError("MockCompetitionAdapter requires platform_kind='mock'")
        self.connection = connection
        self.state_root = Path(state_root)
        self.state_root.mkdir(parents=True, exist_ok=True)
        self.state_path = self.state_root / f"{connection.connection_id}.json"
        if not self.state_path.exists():
            self._save({
                "revision": 1,
                "failure_mode": "",
                "submissions": {},
                "instances": {},
            })

    def _load(self) -> dict[str, Any]:
        try:
            value = json.loads(self.state_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            value = {}
        return {
            "revision": int(value.get("revision") or 1),
            "failure_mode": str(value.get("failure_mode") or ""),
            "submissions": dict(value.get("submissions") or {}),
            "instances": dict(value.get("instances") or {}),
        }

    def _save(self, value: dict[str, Any]) -> None:
        tmp = self.state_path.with_suffix(".json.tmp")
        tmp.write_text(
            json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True),
            encoding="utf-8",
        )
        tmp.replace(self.state_path)

    def set_failure_mode(self, mode: str = "") -> None:
        """开发验收注入 timeout/rate_limited/auth/unknown。"""
        state = self._load()
        state["failure_mode"] = str(mode or "")
        self._save(state)

    def _maybe_fail(self, operation: str) -> None:
        mode = self._load()["failure_mode"]
        if mode == "timeout":
            raise PlatformTimeoutError(f"mock {operation}: timeout")
        if mode == "rate_limited":
            raise PlatformRateLimitedError(
                f"mock {operation}: rate limited", retry_after_seconds=0.2)
        if mode == "auth":
            raise PlatformAuthRequiredError(
                f"mock {operation}: authentication required")
        if mode == "unknown" and operation == "submit":
            raise PlatformUnknownResultError(
                "mock submit: request accepted but response unavailable")

    def _check_connection(self, connection_id: str) -> None:
        if connection_id and connection_id != self.connection.connection_id:
            raise ValueError("mock request targets another connection")

    async def probe(
        self, connection: Optional[PlatformConnectionRef] = None
    ) -> PlatformCapabilities:
        if connection is not None:
            self._check_connection(connection.connection_id)
        self._maybe_fail("probe")
        return PlatformCapabilities(
            platform_kind=self.id,
            sync=True,
            artifacts=True,
            dynamic_instances=True,
            submit=True,
            scoreboard=True,
            detail={
                "transport": "local_persistent_mock",
                "runtime_version": "mock-platform/1",
                "platform_type": "mock",
                "platform_version": "mock-platform/1",
                "identity": self.connection.account_key or "local-operator",
                "clock_skew_seconds": 0,
                "rate_limit": "local / unlimited",
                "schema_hash": "mock-platform-v1",
                "sync_modes": ["cursor"],
                "automation_allowed": True,
            },
        )

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        self._check_connection(request.connection_id)
        self._maybe_fail("sync")
        state = self._load()
        artifact_sha = hashlib.sha256(MOCK_ARTIFACT).hexdigest()
        snapshot = RemoteChallengeSnapshot(
            external_challenge_id="mock-challenge-1",
            name="Local Acceptance Challenge",
            category="misc",
            points=100,
            description=(
                "Inspect the player-visible attachment and submit the flag found "
                "in real command output."
            ),
            flag_format=r"flag\{[^}]+\}",
            artifacts=[RevisionArtifact(
                name="brief.txt",
                sha256=artifact_sha,
                size=len(MOCK_ARTIFACT),
                media_type="text/plain",
            )],
            raw_payload={
                "files": [{
                    "name": "brief.txt",
                    "url": "mock://artifact/brief.txt",
                }],
            },
        )
        cursor = str(state["revision"])
        return SyncResult(
            connection_id=request.connection_id,
            cursor=cursor,
            synced_challenges=1,
            detail={"snapshots": [snapshot.model_dump(mode="json")]},
        )

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        self._check_connection(artifact.connection_id)
        self._maybe_fail("artifact")
        digest = hashlib.sha256(MOCK_ARTIFACT).hexdigest()
        if artifact.sha256 and artifact.sha256.lower() != digest:
            raise ValueError("mock artifact sha256 mismatch")
        return ArtifactObject(
            sha256=digest,
            media_type="text/plain",
            content=MOCK_ARTIFACT,
        )

    async def acquire_instance(
        self, challenge: PlatformChallengeRef
    ) -> InstanceResult:
        self._check_connection(challenge.connection_id)
        self._maybe_fail("acquire_instance")
        state = self._load()
        lease_id = "mock-" + hashlib.sha256(
            f"{challenge.connection_id}:{challenge.challenge_key}".encode()
        ).hexdigest()[:16]
        expires = utcnow() + timedelta(minutes=30)
        state["instances"][lease_id] = {
            "challenge_key": challenge.challenge_key,
            "expires_at": expires.isoformat(),
            "active": True,
        }
        self._save(state)
        return InstanceResult(
            lease=InstanceLeaseRef(
                lease_id=lease_id,
                connection_id=challenge.connection_id,
                challenge_key=challenge.challenge_key,
                fencing_token=1,
                expires_at=expires,
            ),
            endpoints={"http": "http://127.0.0.1:19090/mock-target"},
        )

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        self._check_connection(lease.connection_id)
        self._maybe_fail("renew_instance")
        state = self._load()
        current = state["instances"].get(lease.lease_id)
        if not current or not current.get("active"):
            from muteki.competition.platforms.base import PlatformNotFoundError

            raise PlatformNotFoundError("mock instance not found")
        expires = utcnow() + timedelta(minutes=30)
        current["expires_at"] = expires.isoformat()
        self._save(state)
        return InstanceResult(
            lease=lease.model_copy(update={
                "fencing_token": int(lease.fencing_token) + 1,
                "expires_at": expires,
            }),
            endpoints={"http": "http://127.0.0.1:19090/mock-target"},
        )

    async def probe_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        """读取模拟平台真实状态，不延长 TTL。

        provisioning 恢复记录可能只保存了 challenge_key；此时按题目键查找
        活动实例。正常记录直接按远端实例 ID 查询。
        """
        self._check_connection(lease.connection_id)
        self._maybe_fail("probe_instance")
        state = self._load()
        lease_id = lease.lease_id
        current = state["instances"].get(lease_id)
        if not lease_id or current is None:
            for candidate_id, candidate in state["instances"].items():
                if (
                    candidate.get("active")
                    and candidate.get("challenge_key") == lease.challenge_key
                ):
                    lease_id = candidate_id
                    current = candidate
                    break
        if current is None or not current.get("active"):
            from muteki.competition.platforms.base import PlatformNotFoundError

            raise PlatformNotFoundError("mock instance not found")
        expires_value = current.get("expires_at")
        expires = (
            datetime.fromisoformat(str(expires_value))
            if expires_value
            else utcnow() + timedelta(minutes=30)
        )
        return InstanceResult(
            lease=InstanceLeaseRef(
                lease_id=lease_id,
                connection_id=lease.connection_id,
                challenge_key=str(current.get("challenge_key") or lease.challenge_key),
                fencing_token=lease.fencing_token,
                expires_at=expires,
            ),
            endpoints={"http": "http://127.0.0.1:19090/mock-target"},
        )

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        self._check_connection(lease.connection_id)
        self._maybe_fail("release_instance")
        state = self._load()
        current = state["instances"].get(lease.lease_id)
        if current is not None:
            current["active"] = False
            self._save(state)

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        self._check_connection(request.connection_id)
        self._maybe_fail("submit")
        state = self._load()
        key = str(request.idempotency_key or "") or hashlib.sha256(
            f"{request.challenge_key}:{request.flag}".encode()
        ).hexdigest()
        prior = state["submissions"].get(key)
        if prior is None:
            status = "correct" if request.flag == MOCK_FLAG else "incorrect"
            prior = {
                "submission_id": f"mock-sub-{len(state['submissions']) + 1}",
                "status": status,
                "challenge_key": request.challenge_key,
            }
            state["submissions"][key] = prior
            self._save(state)
        return SubmissionResult(
            submission_id=prior["submission_id"],
            status=prior["status"],
            detail={
                "remote_receipt": (
                    f"{prior['submission_id']}:{prior['status']}"
                ),
                "transport": "local_persistent_mock",
            },
        )

    async def poll_submission(
        self, request: SubmissionRequest
    ) -> SubmissionResult:
        return await self.submit(request)

    async def aclose(self) -> None:
        return None


__all__ = ["MOCK_ARTIFACT", "MOCK_FLAG", "MockCompetitionAdapter"]
