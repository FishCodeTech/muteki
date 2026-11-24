"""Actual remote credential projection/refresh/revoke checks; only synthetic values."""
import base64
import hashlib
import json
import sqlite3
from pathlib import Path
import time
import uuid


def exercise(web, client_root, node, prefix, command, envelope, dispatch, handles, request, raw_request, held_uploads, bulk_source):
    stop_times = []
    for mode in ("run", "shared"):
        material_id = "credential-" + uuid.uuid4().hex[:12]
        secret = ("synthetic-" + uuid.uuid4().hex).encode()
        material = {"kind": "model", "files": {"auth/token": base64.b64encode(secret).decode()},
                    "env_files": {"OPENAI_API_KEY_FILE": "auth/token"}, "env_dirs": {}}
        code, _, body = raw_request(web + "/api/runner/credential-materials/" + material_id, "PUT",
            json.dumps({"expected_revision": None, "material": material}).encode(), {"Content-Type": "application/json"})
        assert code == 200, body
        metadata = json.loads(body)
        assert secret.decode() not in body.decode() and material["files"]["auth/token"] not in body.decode()
        local = []
        for suffix in ("a", "b"):
            run = prefix + "-credential-" + mode + "-" + suffix
            handle = command(run, {"operation": "ensure", "settings": {"worker_container_scope": mode,
                "worker_network": "bridge", "credentials": [material_id]}})
            handles.append(handle)
            code, grant = request(web + "/api/runner/credential-grants", {"material_id": material_id,
                "scope": {"node_id": node, "project_id": "acceptance-project", "run_id": run, "generation": 1}, "ttl_seconds": 120})
            assert code == 200, grant
            projection = command(run, {"operation": "credential_install", "grant_id": grant["id"], "revision": grant["revision"]})
            assert projection["state"] == "active"
            assert secret.decode() not in json.dumps(projection)
            assert material["files"]["auth/token"] not in json.dumps(projection)
            home = client_root / projection["home_relative"]
            assert (home / "auth/token").read_bytes() == secret
            assert (home / "auth/token").stat().st_mode & 0o777 == 0o600
            loop = command(run, {"operation": "start", "spec": {"argv": ["/bin/sh", "-c",
                "test \"$(sha256sum \"$OPENAI_API_KEY_FILE\" | cut -d' ' -f1)\" = " + hashlib.sha256(secret).hexdigest() +
                " || exit 9; while true; do echo tick >> credential-progress; sleep 1; done"],
                "cwd": handle["container_workspace"], "timeout_sec": 120, "env": projection["env"]}})
            local.append((run, handle, grant, projection, home, loop))
        assert (local[0][1]["container_id"] == local[1][1]["container_id"]) == (mode == "shared")
        a, b = local
        if mode == "run":
            slow = held_uploads(web, bulk_source)
            try:
                time.sleep(17)
                for run, _, grant, _, _, _ in local:
                    assert command(run, {"operation": "credential_status", "grant_id": grant["id"]})["state"] == "active"
                    assert command(run, {"operation": "status"})["run"]["state"] == "active"
            finally:
                for connection in slow:
                    connection.close()
        assert dispatch(envelope(b[0], {"operation": "credential_install", "grant_id": a[2]["id"], "revision": a[2]["revision"]}))[0] == 409
        updated = "synthetic-refreshed-" + uuid.uuid4().hex
        refresh_worker = command(a[0], {"operation": "start", "spec": {"argv": ["/bin/sh", "-c",
            "printf '%s' " + updated + " > \"$OPENAI_API_KEY_FILE.new\"; mv \"$OPENAI_API_KEY_FILE.new\" \"$OPENAI_API_KEY_FILE\""],
            "cwd": a[1]["container_workspace"], "timeout_sec": 30, "env": a[3]["env"]}})
        deadline = time.monotonic() + 15
        while True:
            events = command(a[0], {"operation": "events", "worker_id": refresh_worker["worker_id"], "after": 0})
            exits = [frame for frame in events["frames"] if frame["t"] == "exit"]
            if exits:
                assert exits[0]["rc"] == 0, exits
                break
            assert time.monotonic() < deadline
            time.sleep(0.05)
        refresh_request = envelope(a[0], {"operation": "credential_refresh", "grant_id": a[2]["id"]})
        code, refreshed = dispatch(refresh_request)
        assert code == 200, refreshed
        assert refreshed["grant"]["revision"] != metadata["revision"]
        later = b"synthetic-unsubmitted-later-value"
        (a[4] / "auth/token").write_bytes(later)
        with sqlite3.connect(client_root.parent / "muteki.sqlite", timeout=5) as database:
            changed = database.execute("UPDATE documents SET value=json_remove(json_set(value,'$.state','pending'),'$.result') WHERE collection='runner_remote_dispatches' AND id=?",
                (node + ":" + refresh_request["id"],))
            assert changed.rowcount == 1
        assert dispatch(refresh_request) == (200, refreshed)
        with sqlite3.connect(client_root / "muteki.sqlite", timeout=5) as database:
            changed = database.execute("UPDATE documents SET value=json_remove(json_set(value,'$.state','dispatching'),'$.result') WHERE collection='runner_credential_commands' AND id=?", (refresh_request["id"],))
            assert changed.rowcount == 1
        with sqlite3.connect(client_root.parent / "muteki.sqlite", timeout=5) as database:
            database.execute("UPDATE documents SET value=json_remove(json_set(value,'$.state','pending'),'$.result') WHERE collection='runner_remote_dispatches' AND id=?", (node + ":" + refresh_request["id"],))
        assert dispatch(refresh_request)[0] == 409
        (a[4] / "auth/token").write_bytes(updated.encode())
        current = request(web + "/api/runner/credential-materials/" + material_id)[1]
        assert current["revision"] == refreshed["grant"]["revision"]
        assert dispatch(envelope(b[0], {"operation": "credential_refresh", "grant_id": b[2]["id"]}))[0] == 409
        assert request(web + "/api/runner/credential-materials/" + material_id)[1]["revision"] == current["revision"]
        for root in (client_root, client_root.parent):
            for path in root.glob("muteki.sqlite*"):
                data = path.read_bytes()
                for marker in (secret, base64.b64encode(secret), updated.encode(), base64.b64encode(updated.encode())):
                    assert marker not in data, "Credential leaked into ordinary persisted records"
        start = time.monotonic()
        code, revoked = request(web + "/api/runner/credential-grants/" + a[2]["id"] + "/revoke", {})
        assert code == 200 and revoked["execution_stop_confirmed"] is False
        deadline = start + 25
        while True:
            status = command(a[0], {"operation": "credential_status", "grant_id": a[2]["id"]})
            if status["state"] == "removed":
                break
            assert time.monotonic() < deadline, status
            time.sleep(0.2)
        stop_times.append(round(time.monotonic() - start, 3))
        assert not a[4].exists()
        assert command(a[0], {"operation": "status"})["run"]["state"] == "stopped"
        stopped_progress = Path(a[1]["host_workspace"]) / "credential-progress"
        stopped_before = stopped_progress.read_bytes()
        progress = Path(b[1]["host_workspace"]) / "credential-progress"
        before = progress.read_bytes()
        time.sleep(1.2)
        assert stopped_progress.read_bytes() == stopped_before, "Revoked Run's registered Worker kept writing"
        assert len(progress.read_bytes()) > len(before), "Neighbor was stopped by another grant's revocation"
        code, material_revoked = request(web + "/api/runner/credential-materials/" + material_id + "/revoke", {})
        assert code == 200 and material_revoked["material"]["private_data_removed"] is True
        assert not (client_root.parent / "runner/credential-vault" / material_id).exists()
        deadline = time.monotonic() + 25
        while b[4].exists():
            assert time.monotonic() < deadline, "Stopped Run retained projected credentials"
            time.sleep(0.1)
    return {"credential_modes": ["run", "shared"], "credential_worker_file_read": True,
            "credential_refresh_revision_cas": True, "credential_node_receipt_recovers_controller_loss": True, "credential_unknown_refresh_rejects_changed_files": True, "credential_cross_run_denied": True,
            "credential_not_in_sqlite_receipts": True, "credential_revoke_stops_and_cleans": True, "credential_registered_worker_stop_verified": True,
            "credential_neighbor_survives_revoke": True, "credential_revalidation_survives_bulk_saturation": True, "credential_material_revoke_removes_private_versions": True, "credential_revoke_stop_seconds": stop_times}
