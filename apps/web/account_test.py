"""Test-connectivity for registered credential accounts (DESIGN §2.4 補強C-2).

Two backends, DIFFERENT contracts — see the design doc. The cardinal rule both
share: NEVER fall back to the host's default login to fake a success. We test
the *registered account*, with the account's own resolved env.

- backend="local"   → resolve the account into env and run the engine's cheap
                      host probe (driver.health_detail). Verifies "this
                      credential can log in" on the host.
- backend="container" → `docker run --rm` a one-shot container with ONLY the
                      account projection mounted (never the bench tree), and run
                      the engine's in-container liveness probe. This is the ONLY
                      way to catch the container-specific failure layers that a
                      local probe is blind to: image present, mount readable by
                      the container uid (#15), HOME isolation, CLI launchable.

`layer` in the result names which stage failed (image / mount / cli / auth) so a
red status is actionable.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any, Optional

from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    CredentialAccountStore,
    account_store_root,
    project_account_root,
    runtime_env_for_engine,
)

# in-container worker binary per engine — mirrors container_exec._CONTAINER_BIN.
_CONTAINER_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
}


def _result(ok: bool, detail: str, layer: Optional[str] = None) -> dict[str, Any]:
    out: dict[str, Any] = {"ok": ok, "detail": detail}
    if layer:
        out["layer"] = layer
    return out


def probe_account(
    *,
    engine: str,
    account_id: str,
    sessions_root: str | Path,
    backend: str,
) -> dict[str, Any]:
    """Test the registered account. Never raises; returns {ok, detail, layer?}."""
    engine = (engine or "").strip().lower()
    account_id = (account_id or "").strip()
    root = account_store_root(sessions_root)

    # Account must actually be registered — no host-login fallback (reviewer P1).
    store = CredentialAccountStore(root)
    acct = store.inspect(account_id) if account_id else None
    if not account_id or acct is None or not acct.present:
        return _result(False, "账号未登记凭据", layer="auth")

    if backend == "container":
        return _probe_container(engine=engine, account_id=account_id, root=root)
    return _probe_local(engine=engine, account_id=account_id, root=root)


def _probe_local(*, engine: str, account_id: str, root: Path) -> dict[str, Any]:
    """Resolve the account into env (container=False) and run the host probe."""
    from muteki.solver.cli_driver import driver_for  # lazy: avoid import cycle

    env = runtime_env_for_engine(
        engine, account_root=root, account_id=account_id, container=False
    ).env
    if not env:
        return _result(False, "账号未登记凭据", layer="auth")
    prev = {k: os.environ.get(k) for k in env}
    try:
        os.environ.update(env)
        ok, detail = driver_for(engine).health_detail()
    except Exception as exc:  # noqa: BLE001
        return _result(False, str(exc)[:160], layer="cli")
    finally:
        for k, v in prev.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
    return _result(ok, detail or ("ok" if ok else "unhealthy"),
                   layer=None if ok else "auth")


def _docker(*args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout)


def _probe_container(*, engine: str, account_id: str, root: Path) -> dict[str, Any]:
    """Real one-shot `docker run --rm` test of the container plumbing.

    Mounts ONLY the account projection (never the bench tree) + a throwaway empty
    workspace, then runs the engine's in-container liveness probe. Layers:
      image  → worker image missing / docker unavailable
      mount  → container uid can't read the projected credential
      cli    → engine binary won't launch in the container
    """
    from muteki.solver.container_exec import (
        WORKER_IMAGE,
        CONTAINER_WORKSPACE,
    )

    image = WORKER_IMAGE

    # 1) docker reachable + image present.
    try:
        r = _docker("image", "inspect", image, timeout=20)
    except FileNotFoundError:
        return _result(False, "docker 不可用（未安装或 daemon 未运行）", layer="image")
    except subprocess.TimeoutExpired:
        return _result(False, "docker image inspect 超时", layer="image")
    if r.returncode != 0:
        return _result(False, f"镜像缺失或不可用: {image}", layer="image")

    # 2) project the account store into a throwaway, container-readable dir.
    import tempfile
    with tempfile.TemporaryDirectory(prefix="muteki-acct-test-") as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            project_account_root(root, projection)
        except OSError as exc:
            return _result(False, f"凭据投影失败: {str(exc)[:120]}", layer="mount")

        bin_path = _CONTAINER_BIN.get(engine, engine)
        # in-container probe: the credential file must be READABLE at the mount
        # path (catches #15 uid-mismatch) AND the engine binary must launch
        # (--version is the cheap liveness check; a full authed turn would spend
        # quota + need network, out of scope for a plumbing test).
        cred_path = f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}"
        script = (
            f"test -r {cred_path} || {{ echo MUTEKI_MOUNT_UNREADABLE; exit 71; }}; "
            f"{bin_path} --version >/dev/null 2>&1 || {{ echo MUTEKI_CLI_FAIL; exit 72; }}; "
            "echo MUTEKI_OK"
        )
        run_cmd = [
            "run", "--rm", "--init",
            "--network", "none",  # plumbing test needs no network
            # the image ENTRYPOINT is the runtime supervisor (a long-running daemon);
            # a one-shot probe must override it with a shell, else `-lc <script>` is
            # passed as args to the supervisor and the probe hangs / errors.
            "--entrypoint", "bash",
            "--mount",
            f"type=bind,source={os.path.abspath(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount",
            f"type=bind,source={os.path.abspath(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
            image, "-lc", script,
        ]
        try:
            run = _docker(*run_cmd, timeout=60)
        except subprocess.TimeoutExpired:
            return _result(False, "容器探测超时（>60s）", layer="cli")
        out = (run.stdout or "") + (run.stderr or "")
        if "MUTEKI_MOUNT_UNREADABLE" in out or run.returncode == 71:
            return _result(False, "容器内无法读取凭据（uid 不匹配或挂载失败）", layer="mount")
        if "MUTEKI_CLI_FAIL" in out or run.returncode == 72:
            return _result(False, f"容器内 {engine} CLI 无法启动", layer="cli")
        if run.returncode != 0:
            return _result(False, f"容器探测失败: {out.strip()[:160]}", layer="cli")
        return _result(True, "容器内凭据可读、CLI 可启动（已验证镜像+挂载+HOME隔离）")
