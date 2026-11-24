#!/usr/bin/env python3
"""Exercise the real standalone Runner binary over its private socket."""
import argparse
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import uuid


class Connection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=90)
        self.path = str(path)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(self.path)


def read(path, route, payload=None):
    connection = Connection(path)
    try:
        connection.request("GET" if payload is None else "POST", route,
            body=None if payload is None else json.dumps(payload),
            headers={} if payload is None else {"Content-Type":"application/json"})
        response = connection.getresponse()
        data = response.read()
        return response.status, json.loads(data) if data else None
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=".cache/target/debug/muteki-server")
    parser.add_argument("--execution", action="store_true")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    with tempfile.TemporaryDirectory(prefix="runner-ipc-", dir="/tmp") as directory:
        command = [binary, "runner", "--state-dir", directory]
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(command, stdout=log, stderr=log)
            path = Path(directory) / "runner/control.sock"
            try:
                deadline = time.monotonic() + 15
                while not path.exists():
                    if process.poll() is not None or time.monotonic() > deadline:
                        log.seek(0)
                        raise RuntimeError(log.read().decode())
                    time.sleep(0.05)
                status, health = read(path, "/v1/health")
                assert status == 200 and health["protocol_version"] == 1
                assert health["pid"] == process.pid != os.getpid()
                assert path.stat().st_mode & 0o777 == 0o600
                second = subprocess.run(command, capture_output=True, timeout=10)
                assert second.returncode != 0, "duplicate Runner acquired same directory"
                status, resources = read(path, "/v1/resources")
                assert status == 200 and resources["docker"]["available"]
                status, image = read(path, "/v1/worker-image")
                assert status == 200 and image["daemon"]["ok"] and image["pulled"]["ok"]
                assert read(path, "/v1/arbitrary-docker-command")[0] == 404
                assert read(path,"/v1/src/runtime-inspect",{"container_name":"unmanaged-runtime"})[0]==409
                assert read(path,"/v1/src/runtime-inspect",{"container_name":"muteki-src-browser-ffffffffffffffffffffffff"})==(200,{"container":None})
                assert read(path,"/v1/src/environment",{"task_id":"missing-task","operation":"arbitrary","body":{}})[0]==400
                assert read(path,"/v1/catalog/draft",{"engine":"not-an-engine","credential_id":"draft","provider":""})[0]==422
                status,catalog=read(path,"/v1/catalog/draft",{"engine":"pi","credential_id":"draft","provider":""})
                assert status==200 and catalog["source"]=="pi_cli",catalog
                if catalog["ok"]:
                    assert len(catalog["models"])>0,catalog
                else:
                    assert catalog["error_code"]=="model_catalog_unsupported" and catalog["models"]==[],catalog
                execution = []
                if args.execution:
                    prefix = "verify379-" + uuid.uuid4().hex[:10]
                    handles = []
                    released = set()
                    try:
                        for suffix, scope in [("isolated","run"),("sharedA","shared"),("sharedB","shared")]:
                            run = prefix + "-" + suffix
                            status, handle = read(path, "/v1/containers/ensure", {"run_id":run,"owner_run_id":run,"settings":{
                                "worker_container_scope":scope,"worker_network":"bridge","worker_privilege":"default"}})
                            assert status == 200, handle
                            handles.append(handle)
                            workspace=Path(handle["host_workspace"])
                            (workspace/".muteki-browser").mkdir(exist_ok=True)
                            profile=workspace/".muteki-browser-profile";profile.mkdir(exist_ok=True)
                            (profile/"SingletonLock").symlink_to("ffffffffffff-999999")
                            status,recovery=read(path,f"/v1/containers/{run}/browser-profile/recover",{})
                            assert status==200,recovery
                            assert not (profile/"SingletonLock").is_symlink()
                            command = {"command_id":uuid.uuid4().hex,"run_id":run,"spec":{
                                "argv":["/bin/sh","-c","printf runner-started; sleep 1; printf runner-execution-ok"],
                                "cwd":handle["container_workspace"],"env":{},"timeout_sec":20,
                                "output_limit_bytes":1048576,"disk_limit_bytes":10485760}}
                            status, started = read(path,"/v1/workers/start",command)
                            assert status == 200, started
                            assert read(path,"/v1/workers/start",command)==(200,started)
                            status,receipt=read(path,f"/v1/commands/{command['command_id']}")
                            assert status==200 and receipt["result"]==started
                            assert "spec" not in receipt and "env" not in receipt
                            changed = json.loads(json.dumps(command))
                            changed["spec"]["argv"]=["/bin/true"]
                            assert read(path,"/v1/workers/start",changed)[0]==409
                            if suffix == "sharedB":
                                first=handles[1]
                                assert first["name"]==handle["name"]
                                status,result=read(path,f"/v1/containers/{first['run_id']}/teardown",{})
                                assert status==200,result
                                released.add(first["run_id"])
                            cursor=0; frames=[]; deadline=time.monotonic()+30
                            while not any(f["t"]=="exit" for f in frames):
                                assert time.monotonic()<deadline, frames
                                status,page=read(path,f"/v1/workers/{started['worker_id']}/events?after={cursor}")
                                assert status==200,page
                                cursor=page["after"];frames.extend(page["frames"])
                                time.sleep(0.05)
                            assert any(f["t"]=="out" and "runner-execution-ok" in f["line"] for f in frames),frames
                            assert next(f for f in frames if f["t"]=="exit")["rc"]==0
                            execution.append({"scope":scope,"exit_code":0,"idempotent_start":True,"changed_input_rejected":True})
                            if suffix == "isolated":
                                assert read(path,f"/v1/containers/{run}/teardown",{"generation":1})[0]==200
                                assert read(path,"/v1/workers/start",command)[0]==409
                                ensure_body={"run_id":run,"owner_run_id":run,"settings":{"worker_container_scope":"run","worker_network":"bridge"}}
                                assert read(path,"/v1/containers/ensure",ensure_body)[0]==409
                                ensure_body["generation"]=2
                                status,new_handle=read(path,"/v1/containers/ensure",ensure_body)
                                assert status==200,new_handle
                                handles[-1]=new_handle
                                assert read(path,f"/v1/containers/{run}/control",{"op":"Health","generation":1})[0]==409
                                assert read(path,f"/v1/containers/{run}/control",{"op":"Health","generation":2})[0]==200
                                assert read(path,"/v1/workers/start",command)[0]==409
                        assert handles[1]["name"]==handles[2]["name"]
                    finally:
                        for handle in reversed(handles):
                            if handle["run_id"] in released: continue
                            status,result=read(path,f"/v1/containers/{handle['run_id']}/teardown",{"generation":handle["generation"]})
                            assert status==200,result
                        for name in {h["name"] for h in handles}:
                            subprocess.run(["docker","rm","-f",name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
                process.send_signal(signal.SIGINT)
                assert process.wait(timeout=10) == 0
                assert not path.exists()
                print(json.dumps({"standalone_process": True, "protocol": 1,
                    "private_socket": True, "duplicate_owner_rejected": True,
                    "docker": resources["docker"], "image_id": image["image_id"],
                    "clean_shutdown": True,"execution":execution,"draft_pi_models":len(catalog["models"]),"draft_pi_error_code":catalog["error_code"],"managed_inspect_boundary":True,"stale_browser_lock_recovery":bool(execution),"old_generation_rejected":bool(execution)}, indent=2))
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()


if __name__ == "__main__":
    main()
