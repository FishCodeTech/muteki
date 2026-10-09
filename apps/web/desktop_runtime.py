"""Private desktop child entrypoint; readiness and session travel over fd 3."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import socket
import sys
import threading


def main() -> None:
    # Python -I prevents ambient PYTHONPATH / user site packages. Only this
    # release's source tree is added, before importing any application code.
    code_root = Path(__file__).resolve().parents[2]
    sys.path.insert(0, str(code_root))
    bootstrap = json.loads(sys.stdin.readline())
    root = Path(os.environ["MUTEKI_ENVIRONMENT_ROOT"]).resolve()
    manifest = json.loads((root / "environment.json").read_text())
    if manifest["id"] != os.environ["MUTEKI_ENVIRONMENT_ID"]:
        raise RuntimeError("Desktop environment identity mismatch")
    if os.getcwd() != str(root):
        raise RuntimeError("Desktop backend must start in its environment directory")
    os.umask(0o077)
    lock = (root / "backend.lock").open("a+b")
    if os.name == "nt":
        import msvcrt
        lock.write(b"0")
        lock.flush()
        lock.seek(0)
        msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
    else:
        import fcntl
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)

    # No reusable bootstrap credential is placed in argv, environment, disk or
    # the renderer. A fresh service-bound native session goes to the parent pipe.
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(128)
    port = listener.getsockname()[1]
    os.environ["MUTEKI_WEB_PORT"] = str(port)
    os.environ["MUTEKI_RELEASE_ROOT"] = str(code_root)
    os.environ["MUTEKI_BACKEND"] = f"http://127.0.0.1:{port}"
    os.environ["MUTEKI_CAPABILITY_GATEWAY_ENDPOINT"] = f"http://127.0.0.1:{port}/api/capability"
    from apps.web.server import app
    from apps.web.auth import issue_token, token_payload
    import uvicorn
    runtime = {"environment_id": manifest["id"], "channel": manifest["channel"],
               "generation": bootstrap["generation"], "release": bootstrap["release"],
               "pid": os.getpid(), "api_port": port, "maintenance": os.environ.get("MUTEKI_DESKTOP_MAINTENANCE") == "1"}
    app.state.desktop_runtime = runtime

    class DesktopServer(uvicorn.Server):
        async def startup(self, sockets=None):
            app.state.desktop_loop = asyncio.get_running_loop()
            await super().startup(sockets=sockets)
            if not self.started:
                return
            token = issue_token(app.state.auth)
            payload = token_payload(app.state.auth, token)
            ready = {**runtime, "service_id": app.state.auth.service_id,
                     "cookie_name": app.state.auth.cookie_name,
                     "token": token, "expires_at": payload["exp"],
                     "control_port": app.state.platform_stack.control_receiver_status.get("port")}
            with os.fdopen(3, "w", closefd=False) as pipe:
                pipe.write(json.dumps(ready) + "\n")
                pipe.flush()

    server = DesktopServer(uvicorn.Config(app, host="127.0.0.1", port=port, access_log=False, timeout_graceful_shutdown=20))

    def watch_parent():
        # Closing the ownership pipe also stops the child after an Electron crash.
        try:
            for line in sys.stdin:
                request = json.loads(line)
                if request.get("command") == "activate":
                    async def activate():
                        app.state.desktop_activate.set()
                        await app.state.desktop_activated.wait()
                        if getattr(app.state, "desktop_activation_error", None):
                            raise RuntimeError(app.state.desktop_activation_error)
                        app.state.desktop_draining = False
                        runtime["maintenance"] = False
                    asyncio.run_coroutine_threadsafe(activate(), app.state.desktop_loop).result(timeout=120)
                elif request.get("command") != "session":
                    raise RuntimeError("Invalid private desktop command")
                with app.state.auth._lock:
                    token = issue_token(app.state.auth)
                    payload = token_payload(app.state.auth, token)
                with os.fdopen(3, "w", closefd=False) as pipe:
                    pipe.write(json.dumps({"request_id": request["request_id"], "token": token, "expires_at": payload["exp"]}) + "\n")
                    pipe.flush()
        finally:
            server.should_exit = True

    threading.Thread(target=watch_parent, name="desktop-owner", daemon=True).start()
    try:
        asyncio.run(server.serve(sockets=[listener]))
    finally:
        listener.close()
        lock.close()


if __name__ == "__main__":
    main()
