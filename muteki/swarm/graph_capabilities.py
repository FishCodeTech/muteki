"""Capabilities, access paths and Run-owned resource materializations."""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any
from urllib.parse import urlsplit

from muteki.swarm.graph_defs import (
    EV_ACCESS_PATH_PUBLISHED,
    EV_ACCESS_PATH_STATE_CHANGED,
    EV_CAPABILITY_GAP_REPORTED,
    EV_CAPABILITY_PUBLISHED,
    EV_CAPABILITY_RETIRED,
    EV_INTENT_STATE_CHANGED,
    EV_RUNTIME_RESOURCE_REGISTERED,
    EV_RUNTIME_RESOURCE_STATE_CHANGED,
)


_KEY_RE = re.compile(r"^[a-z0-9][a-z0-9:._/@-]{0,191}$")
_SECRET_KEY_RE = re.compile(r"(?:token|password|secret|cookie|api[_-]?key|auth)", re.I)


def _json_list(value: Any, *, limit: int = 64) -> list[str]:
    if not isinstance(value, (list, tuple, set)):
        return []
    return list(dict.fromkeys(str(item).strip() for item in value if str(item).strip()))[:limit]


class _CapabilitiesMixin:
    def publish_capability(
        self, *, actor: str, capability_key: str, target_epoch: str,
        kind: str, quality: str = "", sharing: str = "run-shared",
        source_intent: str = "", evidence_fact_seqs: list[int] | None = None,
        metadata: dict[str, Any] | None = None, access_path_id: str = "",
    ) -> int:
        key = str(capability_key or "").strip().casefold()
        epoch = str(target_epoch or "").strip()
        if not _KEY_RE.fullmatch(key) or not epoch:
            return -1
        facts = sorted({int(x) for x in (evidence_fact_seqs or []) if int(x) > 0})
        if not facts:
            return -1
        active = self._active_fact_seq_set()
        if any(seq not in active for seq in facts):
            return -1
        payload = {
            "capability_key": key, "target_epoch": epoch, "kind": str(kind or "generic")[:80],
            "quality": str(quality or "")[:80], "sharing": str(sharing or "run-shared")[:40],
            "source_intent": str(source_intent or ""), "access_path_id": str(access_path_id or ""),
            "evidence_fact_seqs": facts, "metadata": dict(metadata or {}),
        }
        with self._lock:
            current = self._conn.execute(
                "SELECT state, quality, sharing FROM capabilities WHERE challenge_id=? "
                "AND target_epoch=? AND capability_key=?",
                (self.challenge.id, epoch, key),
            ).fetchone()
            # Capability availability is the semantic state consumed by
            # scheduling.  Re-reporting the same active run-shared capability
            # with a different free-text quality label must remain idempotent;
            # otherwise ordinary follow-up Facts wake Decide again and create
            # duplicate consumers.  A sharing change still represents a real
            # state transition.
            if current and (
                str(current[0] or "") == "active"
                and str(current[2] or "") == payload["sharing"]
            ):
                return -1
            seq = self._append_locked(
                EV_CAPABILITY_PUBLISHED, actor, payload,
                dedupe_key=(
                    f"capability::{self.challenge.id}::{epoch}::{key}::"
                    f"{payload['quality']}::{payload['sharing']}"
                ),
            )
            if seq < 0:
                self._conn.rollback()
                return -1
            self._conn.execute(
                "INSERT INTO capabilities (capability_key,challenge_id,target_epoch,kind,quality,"
                "sharing,state,source_intent,access_path_id,evidence_facts_json,metadata_json,"
                "created_seq,updated_seq) VALUES (?,?,?,?,?,?,'active',?,?,?,?,?,?) "
                "ON CONFLICT(challenge_id,target_epoch,capability_key) DO UPDATE SET "
                "kind=excluded.kind,quality=excluded.quality,sharing=excluded.sharing,state='active',"
                "source_intent=excluded.source_intent,access_path_id=excluded.access_path_id,"
                "evidence_facts_json=excluded.evidence_facts_json,metadata_json=excluded.metadata_json,"
                "updated_seq=excluded.updated_seq",
                (key, self.challenge.id, epoch, payload["kind"], payload["quality"],
                 payload["sharing"], payload["source_intent"], payload["access_path_id"],
                 json.dumps(facts), json.dumps(payload["metadata"], default=str), seq, seq),
            )
            # A repaired/published capability closes every gap whose complete
            # requirement set is now active.  The capability event itself is
            # the semantic trigger; the gap row is only the current-state view.
            active_keys = {
                str(row[0])
                for row in self._conn.execute(
                    "SELECT capability_key FROM capabilities WHERE challenge_id=? "
                    "AND target_epoch=? AND state='active'",
                    (self.challenge.id, epoch),
                ).fetchall()
            }
            for gap_id, raw_required in self._conn.execute(
                "SELECT gap_id,required_capabilities_json FROM capability_gaps "
                "WHERE challenge_id=? AND target_epoch=? AND state='open'",
                (self.challenge.id, epoch),
            ).fetchall():
                try:
                    required = {
                        str(item).strip().casefold()
                        for item in json.loads(raw_required or "[]")
                        if str(item).strip()
                    }
                except (TypeError, ValueError, json.JSONDecodeError):
                    required = set()
                if required and required.issubset(active_keys):
                    self._conn.execute(
                        "UPDATE capability_gaps SET state='resolved' WHERE gap_id=?",
                        (gap_id,),
                    )
            covered_open: list[str] = []
            covered_claimed: list[str] = []
            for intent_id, status, raw_claim in self._conn.execute(
                "SELECT intent_id,status,value_claim_json FROM intents "
                "WHERE challenge_id=? AND dispatch_state='active' "
                "AND status IN ('open','claimed') AND intent_id<>?",
                (self.challenge.id, payload["source_intent"]),
            ).fetchall():
                try:
                    claim = json.loads(raw_claim or "{}")
                except (TypeError, ValueError, json.JSONDecodeError):
                    claim = {}
                declared_after = {
                    str(item).strip().casefold()
                    for item in claim.get("capability_after", [])
                    if str(item).strip()
                } if isinstance(claim, dict) else set()
                if key not in declared_after:
                    continue
                if str(status) == "open":
                    covered_open.append(str(intent_id))
                else:
                    covered_claimed.append(str(intent_id))
            transition_seq = -1
            if covered_open:
                transition_seq = self._append_locked(
                    EV_INTENT_STATE_CHANGED, actor,
                    {"intent_ids": covered_open, "dispatch_state": "closed",
                     "reason": "capability_already_active",
                     "capability_key": key},
                )
            if covered_open:
                marks = ",".join("?" for _ in covered_open)
                self._conn.execute(
                    f"UPDATE intents SET status='done',dispatch_state='closed',"
                    f"close_reason='capability_already_active',result_seq=? "
                    f"WHERE challenge_id=? AND intent_id IN ({marks})",
                    (transition_seq if transition_seq > 0 else None,
                     self.challenge.id, *covered_open),
                )
            if covered_claimed:
                self._append_locked(
                    EV_INTENT_STATE_CHANGED, actor,
                    {"intent_ids": covered_claimed, "priority": -10,
                     "priority_reason": "capability_already_active",
                     "capability_key": key},
                )
                marks = ",".join("?" for _ in covered_claimed)
                self._conn.execute(
                    f"UPDATE intents SET priority=-10,"
                    f"priority_reason='capability_already_active' "
                    f"WHERE challenge_id=? AND intent_id IN ({marks})",
                    (self.challenge.id, *covered_claimed),
                )
            self._conn.commit()
            return seq

    def retire_capability(self, *, actor: str, capability_key: str,
                          target_epoch: str, reason: str = "") -> int:
        key = str(capability_key or "").strip().casefold()
        with self._lock:
            row = self._conn.execute(
                "SELECT 1 FROM capabilities WHERE challenge_id=? AND target_epoch=? "
                "AND capability_key=? AND state='active'",
                (self.challenge.id, str(target_epoch), key),
            ).fetchone()
            if not row:
                return -1
            payload = {"capability_key": key, "target_epoch": str(target_epoch),
                       "state": "retired", "reason": str(reason or "")[:1000]}
            seq = self._append_locked(EV_CAPABILITY_RETIRED, actor, payload)
            self._conn.execute(
                "UPDATE capabilities SET state='retired', updated_seq=? WHERE challenge_id=? "
                "AND target_epoch=? AND capability_key=?",
                (seq, self.challenge.id, str(target_epoch), key),
            )
            self._conn.commit()
            return seq

    def active_capabilities(self, target_epoch: str = "") -> list[dict[str, Any]]:
        where = "challenge_id=? AND state='active'"
        args: list[Any] = [self.challenge.id]
        if target_epoch:
            where += " AND target_epoch=?"
            args.append(str(target_epoch))
        active_facts = self._active_fact_seq_set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT capability_key,target_epoch,kind,quality,sharing,source_intent,"
                "access_path_id,evidence_facts_json,metadata_json,created_seq,updated_seq "
                f"FROM capabilities WHERE {where} ORDER BY updated_seq", tuple(args)
            ).fetchall()
        out: list[dict[str, Any]] = []
        for r in rows:
            try:
                evidence = set(json.loads(r[7] or "[]"))
                metadata = json.loads(r[8] or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                continue
            if not evidence or evidence - active_facts:
                continue
            out.append({
                "capability_key": r[0], "target_epoch": r[1], "kind": r[2],
                "quality": r[3] or "", "sharing": r[4] or "",
                "source_intent": r[5] or "", "access_path_id": r[6] or "",
                "evidence_fact_seqs": sorted(evidence),
                "metadata": metadata, "created_seq": int(r[9]),
                "updated_seq": int(r[10]),
            })
        return out

    def active_capability_keys(self, target_epoch: str = "") -> set[str]:
        return {str(row["capability_key"]) for row in self.active_capabilities(target_epoch)}

    def report_capability_gap(
        self, *, actor: str, intent_id: str, target_epoch: str, description: str,
        required_capabilities: list[str] | None = None,
        consumers: list[str] | None = None,
    ) -> dict[str, Any] | None:
        clean = " ".join(str(description or "").split())[:2000]
        epoch = str(target_epoch or "").strip()
        if not clean or not epoch:
            return None
        required = _json_list(required_capabilities)
        consumer_ids = _json_list(consumers)
        digest = hashlib.sha256(
            f"{epoch}\x1f{clean.casefold()}\x1f{','.join(required)}".encode()
        ).hexdigest()[:16]
        gap_id = f"CG-{digest}"
        payload = {"gap_id": gap_id, "intent_id": str(intent_id or ""),
                   "target_epoch": epoch, "description": clean,
                   "required_capabilities": required, "consumers": consumer_ids}
        with self._lock:
            seq = self._append_locked(
                EV_CAPABILITY_GAP_REPORTED, actor, payload,
                dedupe_key=f"capability-gap::{self.challenge.id}::{gap_id}",
            )
            if seq < 0:
                self._conn.rollback()
                return None
            self._conn.execute(
                "INSERT OR IGNORE INTO capability_gaps (gap_id,challenge_id,target_epoch,worker,"
                "intent_id,description,required_capabilities_json,consumers_json,state,created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,'open',?)",
                (gap_id, self.challenge.id, epoch, actor, str(intent_id or ""), clean,
                 json.dumps(required), json.dumps(consumer_ids), seq),
            )
            self._conn.commit()
        return {**payload, "created_seq": seq}

    def open_capability_gaps(self, target_epoch: str = "") -> list[dict[str, Any]]:
        query = ("SELECT gap_id,target_epoch,worker,intent_id,description,"
                 "required_capabilities_json,consumers_json,created_seq FROM capability_gaps "
                 "WHERE challenge_id=? AND state='open'")
        args: list[Any] = [self.challenge.id]
        if target_epoch:
            query += " AND target_epoch=?"
            args.append(str(target_epoch))
        query += " ORDER BY created_seq"
        with self._lock:
            rows = self._conn.execute(query, tuple(args)).fetchall()
        return [{"gap_id": r[0], "target_epoch": r[1], "worker": r[2],
                 "intent_id": r[3] or "", "description": r[4],
                 "required_capabilities": json.loads(r[5] or "[]"),
                 "consumers": json.loads(r[6] or "[]"), "created_seq": int(r[7])}
                for r in rows]

    def register_runtime_resource(
        self, *, actor: str, resource_id: str, target_epoch: str,
        owner_intent: str, backend: str, pid: int | None, container_name: str,
        log_path: str, cwd: str = "", env: dict[str, str] | None = None,
        cleanup_command: str = "", health: dict[str, Any] | None = None,
    ) -> int:
        payload = {"resource_id": resource_id, "target_epoch": str(target_epoch),
                   "owner_intent": owner_intent, "backend": backend, "pid": pid,
                   "container_name": container_name, "log_path": log_path,
                   "health": dict(health or {})}
        with self._lock:
            seq = self._append_locked(
                EV_RUNTIME_RESOURCE_REGISTERED, actor, payload,
                dedupe_key=f"runtime-resource::{self.challenge.id}::{resource_id}",
            )
            if seq < 0:
                self._conn.rollback()
                return -1
            self._conn.execute(
                "INSERT INTO runtime_resources (resource_id,challenge_id,target_epoch,owner_worker,"
                "owner_intent,backend,pid,container_name,log_path,cwd,env_json,cleanup_command,"
                "health_json,state,created_seq,updated_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,'running',?,?)",
                (resource_id, self.challenge.id, str(target_epoch), actor, owner_intent, backend,
                 pid, container_name, log_path, str(cwd or ""),
                 json.dumps(dict(env or {})), str(cleanup_command or "")[:4096],
                 json.dumps(health or {}), seq, seq),
            )
            self._conn.commit()
            return seq

    def runtime_resource(self, resource_id: str) -> dict[str, Any] | None:
        with self._lock:
            row = self._conn.execute(
                "SELECT resource_id,target_epoch,owner_worker,owner_intent,backend,pid,"
                "container_name,log_path,cwd,env_json,cleanup_command,health_json,state,"
                "created_seq,updated_seq "
                "FROM runtime_resources WHERE challenge_id=? AND resource_id=?",
                (self.challenge.id, str(resource_id)),
            ).fetchone()
        if not row:
            return None
        return {"resource_id": row[0], "target_epoch": row[1], "owner_worker": row[2],
                "owner_intent": row[3] or "", "backend": row[4], "pid": row[5],
                "container_name": row[6] or "", "log_path": row[7] or "",
                "cwd": row[8] or "", "env": json.loads(row[9] or "{}"),
                "cleanup_command": row[10] or "",
                "health": json.loads(row[11] or "{}"), "state": row[12],
                "created_seq": int(row[13]), "updated_seq": int(row[14])}

    def publish_access_path(
        self, *, actor: str, access_path_id: str, target_epoch: str,
        runtime_resource_id: str, source_intent: str, reach: list[str],
        operations: list[str], quality: str, endpoint: str,
        use_spec: dict[str, Any], health: dict[str, Any] | None,
        dependency_fact_seqs: list[int] | None,
        capability_keys: list[str] | None,
    ) -> int:
        resource = self.runtime_resource(runtime_resource_id)
        if not resource or resource["state"] != "running" or resource["owner_worker"] != actor:
            return -1
        epoch = str(target_epoch or "")
        if resource["target_epoch"] != epoch:
            return -1
        facts = sorted({int(x) for x in (dependency_fact_seqs or []) if int(x) > 0})
        if not facts or any(seq not in self._active_fact_seq_set() for seq in facts):
            return -1
        spec = dict(use_spec or {})
        env = spec.get("env") if isinstance(spec.get("env"), dict) else {}
        if any(_SECRET_KEY_RE.search(str(key)) for key in env):
            return -1
        try:
            parsed = urlsplit(str(endpoint or ""))
            if parsed.username or parsed.password:
                return -1
        except ValueError:
            return -1
        keys = [str(k).strip().casefold() for k in _json_list(capability_keys)
                if _KEY_RE.fullmatch(str(k).strip().casefold())]
        if (not keys or not str(endpoint or "").strip()
                or not _json_list(reach) or not _json_list(operations)
                or dict(health or {}).get("kind") != "tcp"):
            return -1
        payload = {"access_path_id": access_path_id, "target_epoch": epoch,
                   "runtime_resource_id": runtime_resource_id,
                   "source_intent": source_intent, "reach": _json_list(reach),
                   "operations": _json_list(operations), "quality": str(quality or "multiplexed")[:40],
                   "endpoint": str(endpoint or "")[:1000], "use_spec": spec,
                   "health": dict(health or {}), "dependency_fact_seqs": facts,
                   "capability_keys": keys, "state": "ready"}
        with self._lock:
            seq = self._append_locked(
                EV_ACCESS_PATH_PUBLISHED, actor, payload,
                dedupe_key=f"access-path::{self.challenge.id}::{access_path_id}",
            )
            if seq < 0:
                self._conn.rollback()
                return -1
            self._conn.execute(
                "INSERT INTO access_paths (access_path_id,challenge_id,target_epoch,"
                "runtime_resource_id,owner_worker,source_intent,reach_json,operations_json,quality,"
                "endpoint,use_spec_json,health_json,dependency_facts_json,capability_keys_json,state,"
                "consecutive_failures,created_seq,updated_seq) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,"
                "'ready',0,?,?)",
                (access_path_id, self.challenge.id, epoch, runtime_resource_id, actor, source_intent,
                 json.dumps(payload["reach"]), json.dumps(payload["operations"]), payload["quality"],
                 payload["endpoint"], json.dumps(spec), json.dumps(health or {}), json.dumps(facts),
                 json.dumps(keys), seq, seq),
            )
            self._conn.commit()
        for key in keys:
            self.publish_capability(
                actor=actor, capability_key=key, target_epoch=epoch, kind="access",
                quality=payload["quality"], sharing="run-shared", source_intent=source_intent,
                evidence_fact_seqs=facts, metadata={"reach": payload["reach"],
                "operations": payload["operations"], "endpoint": payload["endpoint"]},
                access_path_id=access_path_id,
            )
        return seq

    def active_access_paths(
        self, *, target_epoch: str = "", required_capabilities: list[str] | None = None,
    ) -> list[dict[str, Any]]:
        query = ("SELECT access_path_id,target_epoch,runtime_resource_id,owner_worker,source_intent,"
                 "reach_json,operations_json,quality,endpoint,use_spec_json,health_json,"
                 "dependency_facts_json,capability_keys_json,created_seq,updated_seq "
                 "FROM access_paths WHERE challenge_id=? AND state='ready'")
        args: list[Any] = [self.challenge.id]
        if target_epoch:
            query += " AND target_epoch=?"
            args.append(str(target_epoch))
        query += " ORDER BY updated_seq"
        required = set(_json_list(required_capabilities))
        active_facts = self._active_fact_seq_set()
        with self._lock:
            rows = self._conn.execute(query, tuple(args)).fetchall()
        out = []
        for r in rows:
            try:
                dependencies = set(json.loads(r[11] or "[]"))
                keys = set(json.loads(r[12] or "[]"))
                reach = json.loads(r[5] or "[]")
                operations = json.loads(r[6] or "[]")
                use_spec = json.loads(r[9] or "{}")
                health = json.loads(r[10] or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                continue
            if dependencies - active_facts or (required and not (required & keys)):
                continue
            out.append({"access_path_id": r[0], "target_epoch": r[1],
                        "runtime_resource_id": r[2], "owner_worker": r[3],
                        "source_intent": r[4] or "", "reach": reach,
                        "operations": operations, "quality": r[7],
                        "endpoint": r[8] or "", "use_spec": use_spec,
                        "health": health,
                        "dependency_fact_seqs": sorted(dependencies),
                        "capability_keys": sorted(keys), "state": "ready",
                        "created_seq": int(r[13]), "updated_seq": int(r[14])})
        return out

    def set_access_path_state(self, *, actor: str, access_path_id: str,
                              state: str, reason: str = "") -> int:
        if state not in {"ready", "degraded", "closed"}:
            return -1
        with self._lock:
            row = self._conn.execute(
                "SELECT capability_keys_json,target_epoch,state FROM access_paths "
                "WHERE challenge_id=? AND access_path_id=?",
                (self.challenge.id, access_path_id),
            ).fetchone()
            if not row or str(row[2]) == state:
                return -1
            payload = {"access_path_id": access_path_id, "target_epoch": row[1],
                       "state": state, "reason": str(reason or "")[:1000]}
            seq = self._append_locked(EV_ACCESS_PATH_STATE_CHANGED, actor, payload)
            failures = "consecutive_failures+1" if state == "degraded" else "0"
            self._conn.execute(
                f"UPDATE access_paths SET state=?,consecutive_failures={failures},updated_seq=? "
                "WHERE challenge_id=? AND access_path_id=?",
                (state, seq, self.challenge.id, access_path_id),
            )
            self._conn.commit()
        if state != "ready":
            for key in json.loads(row[0] or "[]"):
                self.retire_capability(actor=actor, capability_key=key,
                                       target_epoch=str(row[1]), reason=reason or state)
        else:
            path = next((item for item in self.active_access_paths(
                target_epoch=str(row[1])) if item["access_path_id"] == access_path_id), None)
            if path:
                for key in path["capability_keys"]:
                    self.publish_capability(
                        actor=actor, capability_key=key, target_epoch=str(row[1]),
                        kind="access", quality=path["quality"], sharing="run-shared",
                        source_intent=path["source_intent"],
                        evidence_fact_seqs=path["dependency_fact_seqs"],
                        metadata={"reach": path["reach"], "operations": path["operations"],
                                  "endpoint": path["endpoint"]},
                        access_path_id=access_path_id,
                    )
        return seq

    def refresh_access_path_health(
        self, *, actor: str = "coordinator", target_epoch: str = "",
        failure_threshold: int = 3,
    ) -> list[dict[str, Any]]:
        """Probe registered TCP health and emit only threshold state changes."""
        from muteki.swarm.runtime_resources import tcp_health

        query = ("SELECT ap.access_path_id,ap.target_epoch,ap.state,"
                 "ap.consecutive_failures,ap.health_json,ap.capability_keys_json,"
                 "rr.backend,rr.container_name,ap.runtime_resource_id,"
                 "ap.dependency_facts_json FROM access_paths ap "
                 "JOIN runtime_resources rr ON rr.resource_id=ap.runtime_resource_id "
                 "WHERE ap.challenge_id=? AND ap.state IN ('ready','degraded')")
        args: list[Any] = [self.challenge.id]
        if target_epoch:
            query += " AND ap.target_epoch=?"
            args.append(str(target_epoch))
        with self._lock:
            rows = self._conn.execute(query, tuple(args)).fetchall()
        changes: list[dict[str, Any]] = []
        active_facts = self._active_fact_seq_set()
        for (path_id, epoch, state, failures, raw_health, raw_keys, backend,
             container, resource_id, raw_dependencies) in rows:
            try:
                dependencies = set(json.loads(raw_dependencies or "[]"))
            except (TypeError, ValueError, json.JSONDecodeError):
                dependencies = set()
            if not dependencies or dependencies - active_facts:
                stopped = self.stop_runtime_resource_id(
                    actor=actor, resource_id=str(resource_id),
                    reason="dependency Fact is no longer active",
                )
                if stopped:
                    changes.append({"access_path_id": path_id, "state": "closed"})
                continue
            try:
                health = json.loads(raw_health or "{}")
            except (TypeError, ValueError, json.JSONDecodeError):
                health = {}
            if health.get("kind") != "tcp" or int(health.get("port") or 0) <= 0:
                continue
            healthy = tcp_health(
                str(health.get("host") or "127.0.0.1"), int(health["port"]),
                backend=str(backend or "local"), container_name=str(container or ""),
            )
            if healthy:
                if state == "degraded":
                    seq = self.set_access_path_state(
                        actor=actor, access_path_id=str(path_id), state="ready",
                        reason="health check recovered")
                    if seq > 0:
                        changes.append({"access_path_id": path_id, "state": "ready", "seq": seq})
                elif int(failures or 0):
                    with self._lock:
                        self._conn.execute(
                            "UPDATE access_paths SET consecutive_failures=0 WHERE access_path_id=?",
                            (path_id,),
                        )
                        self._conn.commit()
                continue
            next_failures = int(failures or 0) + 1
            if state == "ready" and next_failures < max(1, int(failure_threshold)):
                with self._lock:
                    self._conn.execute(
                        "UPDATE access_paths SET consecutive_failures=? WHERE access_path_id=?",
                        (next_failures, path_id),
                    )
                    self._conn.commit()
                continue
            if state == "ready":
                seq = self.set_access_path_state(
                    actor=actor, access_path_id=str(path_id), state="degraded",
                    reason=f"TCP health failed {next_failures} consecutive checks")
                if seq > 0:
                    keys = json.loads(raw_keys or "[]")
                    self.report_capability_gap(
                        actor=actor, intent_id="", target_epoch=str(epoch),
                        description=f"repair degraded shared access path {path_id}",
                        required_capabilities=keys, consumers=keys,
                    )
                    changes.append({"access_path_id": path_id, "state": "degraded", "seq": seq})
        return changes

    def active_novelty_keys(self) -> set[str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT novelty_key FROM intents WHERE challenge_id=? AND novelty_key IS NOT NULL "
                "AND novelty_key!='' AND dispatch_state='active' AND status IN ('open','claimed')",
                (self.challenge.id,),
            ).fetchall()
        return {str(row[0]) for row in rows}

    def access_context_block(self, *, target_epoch: str = "",
                             required_capabilities: list[str] | None = None) -> str:
        capabilities = self.active_capabilities(target_epoch)
        paths = self.active_access_paths(
            target_epoch=target_epoch, required_capabilities=required_capabilities)
        gaps = self.open_capability_gaps(target_epoch)
        lines = ["## Active capabilities"]
        lines.extend(
            f"- {c['capability_key']} (quality={c['quality'] or 'unknown'}, sharing={c['sharing']})"
            for c in capabilities
        )
        if not capabilities:
            lines.append("- none")
        lines.append("## Ready shared access paths")
        for path in paths:
            lines.append(
                f"- {path['access_path_id']}: reach={','.join(path['reach']) or 'unspecified'}; "
                f"operations={','.join(path['operations'])}; quality={path['quality']}; "
                f"endpoint={path['endpoint']}; use={json.dumps(path['use_spec'], ensure_ascii=False)}"
            )
        if not paths:
            lines.append("- none")
        if gaps:
            lines.append("## Open capability gaps")
            lines.extend(
                f"- {gap['gap_id']}: {gap['description']} (consumers={len(gap['consumers'])})"
                for gap in gaps
            )
        return "\n".join(lines)

    def stop_runtime_resource_id(
        self, *, actor: str, resource_id: str, reason: str,
    ) -> bool:
        from muteki.swarm.runtime_resources import stop_runtime_resource

        resource = self.runtime_resource(resource_id)
        if not resource or resource["state"] != "running":
            return False
        if not stop_runtime_resource(resource):
            return False
        with self._lock:
            path_ids = [
                str(row[0]) for row in self._conn.execute(
                    "SELECT access_path_id FROM access_paths WHERE challenge_id=? "
                    "AND runtime_resource_id=? AND state!='closed'",
                    (self.challenge.id, resource_id),
                ).fetchall()
            ]
        for path_id in path_ids:
            self.set_access_path_state(
                actor=actor, access_path_id=path_id, state="closed", reason=reason)
        with self._lock:
            payload = {"resource_id": resource_id, "state": "closed", "reason": reason}
            seq = self._append_locked(EV_RUNTIME_RESOURCE_STATE_CHANGED, actor, payload)
            self._conn.execute(
                "UPDATE runtime_resources SET state='closed',updated_seq=? WHERE resource_id=?",
                (seq, resource_id),
            )
            self._conn.commit()
        return True

    def stop_runtime_resources(
        self, *, actor: str = "coordinator", target_epoch: str = "",
    ) -> int:
        query = (
            "SELECT resource_id,backend,pid,container_name FROM runtime_resources "
            "WHERE challenge_id=? AND state='running'"
        )
        args: list[Any] = [self.challenge.id]
        if target_epoch:
            query += " AND target_epoch=?"
            args.append(str(target_epoch))
        with self._lock:
            rows = self._conn.execute(query, tuple(args)).fetchall()
        stopped = 0
        for resource_id, _backend, _pid, _container_name in rows:
            if self.stop_runtime_resource_id(
                actor=actor, resource_id=str(resource_id),
                reason="target epoch retired" if target_epoch else "run ended",
            ):
                stopped += 1
        return stopped
