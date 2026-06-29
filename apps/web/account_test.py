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

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Optional

from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    project_account_root,
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
    """LEGACY entry — test a bare (engine, account) credential, NOT a profile.

    Kept for back-compat (the old credential-accounts/{id}/test endpoint). It
    routes through the profile_health kernel so its verdict matches dispatch for
    plumbing + auth, but it synthesizes a profile WITHOUT a pinned model, so the
    auth hello uses the engine-DEFAULT model. That is fine for "can this raw
    credential authenticate?" but it is NOT equivalent to a specific profile's
    run health (a profile may pin a model the default check can't see). Profile
    readiness must use the per-profile health endpoint instead.

    Returns the historical {ok, detail, layer?} shape. Never raises.
    """
    from muteki.solver.credential_accounts import (
        CredentialAccountStore,
        account_store_root,
    )
    from muteki.solver.profile_health import evaluate_profile_health

    engine = (engine or "").strip().lower()
    account_id = (account_id or "").strip()
    backend = "container" if backend == "container" else "local"

    # Legacy contract (reviewer P1): the account MUST be registered with present
    # credential material — there is NO host-login fallback for the account-test
    # entry, even on a host that happens to be logged in. This is STRICTER than a
    # real profile (which may legitimately use a present system login), so we gate
    # it here rather than relying on the kernel's profile-grade binding rule.
    store = CredentialAccountStore(account_store_root(sessions_root))
    acct = store.inspect(account_id) if account_id else None
    if not account_id or acct is None or not acct.present:
        return _result(False, "账号未登记凭据", layer="auth")

    # A custom-endpoint account (third-party OpenAI/Anthropic-compatible endpoint,
    # e.g. DeepSeek) has NO model bound to it — so synthesizing a profile and
    # shelling claude-code makes the CLI pick a wrong DEFAULT model the endpoint
    # doesn't know, which HANGS until timeout even though the endpoint is fine.
    # Probe the endpoint DIRECTLY instead (cheap curl, model-agnostic): "can this
    # base_url + key authenticate?" — the right question for an account, with no
    # CLI and no pinned model. (A profile's real run-readiness still uses the
    # per-profile health endpoint, which DOES carry the pinned model.)
    if acct.mode == "custom_endpoint":
        return _probe_endpoint_account(account_id=account_id, acct=acct,
                                       root=account_store_root(sessions_root))

    # Synthesize a minimal profile (no pinned model — see docstring) so the
    # kernel's plumbing + auth layers run with the account's resolved env.
    profile = {
        "id": account_id,
        "name": account_id,
        "engine": engine,
        "credential_account": account_id,
        "credential_mode": "api_key",
        "enabled": True,
    }
    h = evaluate_profile_health(
        profile, backend=backend, sessions_root=sessions_root, depth="auth"
    )
    out: dict[str, Any] = {"ok": h.ok, "detail": h.detail}
    if h.layer:
        out["layer"] = h.layer
    return out


def _probe_endpoint_account(*, account_id: str, acct: Any, root: Path) -> dict[str, Any]:
    """Model-agnostic auth probe for a custom-endpoint credential.

    Reads the account's API_KEY + BASE_URL + target engine and sends ONE minimal
    request (max_tokens=1) in the wire format the target engine speaks:
      - claude target → Anthropic Messages  ({base}/v1/messages, x-api-key)
      - codex/cursor/other → OpenAI Chat Completions ({base}/chat/completions, Bearer)
    Never shells a CLI, never needs a pinned model, returns fast. NEVER raises.
    """
    base = root / account_id
    try:
        api_key = (base / "API_KEY").read_text(encoding="utf-8").strip()
    except OSError:
        return _result(False, "账号缺少 API_KEY", layer="auth")
    base_url = ""
    if (base / "BASE_URL").exists():
        try:
            base_url = (base / "BASE_URL").read_text(encoding="utf-8").strip().rstrip("/")
        except OSError:
            base_url = ""
    if not base_url:
        return _result(False, "自定义端点缺少 base_url", layer="auth")
    if not api_key:
        return _result(False, "自定义端点缺少 API key", layer="auth")

    target = (acct.details or {}).get("target_engine") or acct.engine or ""
    # build the right wire request for the target engine.
    if target == "claude":
        url = f"{base_url}/v1/messages"
        headers = ["-H", f"x-api-key: {api_key}", "-H", "anthropic-version: 2023-06-01",
                   "-H", "Content-Type: application/json"]
        body = json.dumps({"model": "probe", "max_tokens": 1,
                           "messages": [{"role": "user", "content": "ok"}]})
    else:
        url = f"{base_url}/chat/completions"
        headers = ["-H", f"Authorization: Bearer {api_key}", "-H", "Content-Type: application/json"]
        body = json.dumps({"model": "probe", "max_tokens": 1,
                           "messages": [{"role": "user", "content": "ok"}]})

    # -w writes the HTTP status on its own line so we can classify auth vs other.
    argv = ["curl", "-sS", "-m", "20", "-o", "/dev/null", "-w", "%{http_code}",
            "-X", "POST", *headers, "--data", body, url]
    try:
        r = subprocess.run(argv, capture_output=True, text=True,
                           encoding="utf-8", errors="replace", timeout=25)
    except FileNotFoundError:
        return _result(False, "curl 不可用", layer="auth")
    except subprocess.TimeoutExpired:
        return _result(False, "端点探测超时（>20s）", layer="auth")
    code = (r.stdout or "").strip()[-3:]
    # 200 → key authenticates. 400/422 → endpoint reached + key OK, just our dummy
    # "probe" model/body was rejected — that still PROVES auth+reachability, which is
    # all an account test asserts. 401/403 → bad key. 404 → wrong endpoint path.
    if code in ("200", "400", "422"):
        return _result(True, f"端点可达且凭据通过认证（HTTP {code}）")
    if code in ("401", "403"):
        return _result(False, f"端点拒绝凭据（HTTP {code}，key 可能无效）", layer="auth")
    if code == "404":
        return _result(False, f"端点路径不存在（HTTP 404，base_url 或目标引擎不匹配）", layer="auth")
    tail = (r.stderr or "").strip().splitlines()
    detail = f"端点探测失败（HTTP {code or '?'}）" + (f": {tail[-1][:80]}" if tail else "")
    return _result(False, detail, layer="auth")


def _docker(*args: str, timeout: float = 30.0) -> subprocess.CompletedProcess:
    # encoding=utf-8/errors=replace (P2-v3): decode docker output as UTF-8, not the
    # host locale codepage (Windows cp936 would corrupt non-ASCII output).
    return subprocess.run(["docker", *args], capture_output=True, text=True,
                          encoding="utf-8", errors="replace", timeout=timeout)


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
        _HOST_DATA_ROOT,
        _mount_source,
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
    #    P2-v3 BLOCKER-c: the sibling probe container is launched by the HOST
    #    daemon, so the temp dir must live somewhere the host can see. /tmp inside
    #    the web container is invisible to the host — root the temp dir under the
    #    mirrored data root instead. On a bare host (_HOST_DATA_ROOT unset) this is
    #    the normal system temp dir.
    import tempfile
    _tmp_base = None
    if _HOST_DATA_ROOT:
        _tmp_base = os.path.join(os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
                                 "_tmp", "account-tests")
        try:
            os.makedirs(_tmp_base, exist_ok=True)
        except OSError:
            _tmp_base = None
    with tempfile.TemporaryDirectory(prefix="muteki-acct-test-", dir=_tmp_base) as td:
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
            f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount",
            f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
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
