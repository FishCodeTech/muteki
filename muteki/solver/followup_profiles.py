"""Select auxiliary workers from the task's recorded participants."""
from __future__ import annotations
from copy import deepcopy
from typing import Any, Iterable
from muteki.core.events import EventType
from muteki.solver.worker_profiles import base_engine_for_profile, worker_identity_fields

class FollowupSelectionError(ValueError):
    code = "followup.worker_unavailable"

def require_followup_profile(ref: str, profiles: list[dict], *, allow_engine: bool = False) -> dict:
    rows = [profile for profile in profiles if isinstance(profile, dict)]
    matches = [profile for profile in rows if ref in {profile.get("id"), profile.get("name")}]
    if not matches and allow_engine:
        matches = [profile for profile in rows if base_engine_for_profile(profile) == ref]
    if len(matches) != 1 or not matches[0].get("enabled", True):
        raise FollowupSelectionError("出战 Agent 的配置不可用或不唯一，请检查原配置")
    return deepcopy(matches[0])


def participant_rows(events: Iterable[Any]) -> list[dict[str, str]]:
    """Only coordinator-recorded execution proves participation, not the roster."""
    rows: dict[str, dict[str, str]] = {}
    for event in events:
        if event.event_type not in {EventType.WORKER_STATUS, EventType.WORKER_LIFECYCLE}:
            continue
        payload = event.payload or {}
        worker_id = str(event.solver_id or "")
        if (not worker_id or worker_id.endswith("-standby")
                or payload.get("worker_role") == "respond"):
            continue
        if worker_id not in rows:
            if not (payload.get("online") is True or payload.get("phase") == "spawned"):
                continue
            rows[worker_id] = {"worker_id": worker_id}
        row = rows[worker_id]
        for key in ("engine", "profile_id", "profile_label", "model", "account_id",
                    "connection", "endpoint_host", "provider"):
            if payload.get(key):
                row[key] = str(payload[key])
    return [row for row in rows.values() if row.get("engine") and row.get("profile_id")]

async def participating_workers(run) -> list[dict[str, str]]:
    events = [event async for event in run.store.replay(run.run_id)
              if event.event_type in {EventType.WORKER_STATUS, EventType.WORKER_LIFECYCLE}]
    return participant_rows(events)

def select_participant(participants: list[dict], requested: str = "", winner: dict | None = None) -> dict:
    if requested:
        matches = [p for p in participants if p["worker_id"] == requested]
        if not matches:
            matches = [p for p in participants if p.get("profile_id") == requested]
        if len(matches) != 1:
            raise FollowupSelectionError("请选择本次任务实际出战的 Agent；该选择不存在或不唯一")
        return dict(matches[0])
    if not participants:
        raise FollowupSelectionError("本次任务尚无已出战的 Agent")
    winner_id = str((winner or {}).get("worker_id") or "")
    return dict(next((p for p in participants if p["worker_id"] == winner_id), participants[0]))

def participant_profile(participant: dict, profiles: list[dict]) -> dict:
    profile = require_followup_profile(str(participant["profile_id"]), profiles)
    if base_engine_for_profile(profile) != participant["engine"]:
        raise FollowupSelectionError("出战 Agent 的引擎配置已更改，请恢复原配置后重试")
    recorded_account = str(participant.get("account_id") or "")
    current_account = str(profile.get("credential_account") or "")
    if recorded_account != current_account:
        raise FollowupSelectionError("出战 Agent 的账号配置已更改，请恢复原账号后重试")
    current_identity = worker_identity_fields(profile)
    for key in ("connection", "endpoint_host", "provider"):
        if participant.get(key) and participant[key] != current_identity.get(key):
            raise FollowupSelectionError("出战 Agent 的连接配置已更改，请恢复原配置后重试")
    if participant.get("model"):
        profile["model"] = participant["model"]
    return profile

def select_btw_profile(config: dict, winner: dict, requested: str = "", *, participants: list[dict]) -> dict:
    participant = select_participant(participants, requested, winner)
    return participant_profile(participant, config.get("worker_profiles") or [])

def configured_standby_identity(winner: dict, config: dict, action: str, *,
                                participants: list[dict], requested: str = "") -> dict | None:
    if action not in {"ask", "writeup"}:
        return None
    participant = select_participant(participants, requested, winner)
    participant_profile(participant, config.get("worker_profiles") or [])
    return {**winner, **participant, "session": "", "workdir_rel": "", "agent_state_rel": ""}
