"""Unit tests for the container worker-execution backend (pure logic — no Docker).

ONE long-lived container per run. The DEFAULT transport is the rcp Runtime Control
Plane (an in-container supervisor the host drives over a Unix socket); a legacy
host-side `docker exec` transport is kept behind MUTEKI_WORKER_BACKEND=
container_dockerexec as an emergency fallback. The live paths were validated
end-to-end against a real container; these lock in the host→container translation
logic a regression could silently break: cwd mapping, argv binary translation, the
docker-exec command shape (legacy), and the signal mapping the solver's control
surface relies on (rcp Signal op + legacy pkill).
"""

from __future__ import annotations

import os

import signal

import muteki.solver.container_exec as cx
from muteki.solver.container_exec import (
    CONTAINER_WORKSPACE,
    CONTAINER_CONTROL_DIR,
    ContainerHandle,
    _containerize_argv,
    _ContainerProc,
    _DockerExecBackend,
    _mount_source,
    ensure_container,
    run_cli_container,
    runtime_execs_for_run,
)
from muteki.solver.cli_driver import CliResult


def _handle(ws="/run/sessions/abc/workspace", container="muteki-run-nyu_2021q-x",
            mode="dockerexec"):
    return ContainerHandle(run_id="nyu:2021q-x", host_workspace=ws,
                           container=container, image="snowywar/muteki-worker:latest",
                           network="host", mode=mode)


# ── cwd mapping (host path under the bind-mounted workspace → container path) ──

def test_to_container_cwd_maps_subdir_under_workspace():
    h = _handle("/run/ws")
    assert h.to_container_cwd("/run/ws/cli-claude-1") == f"{CONTAINER_WORKSPACE}/cli-claude-1"


def test_to_container_cwd_root_is_workspace():
    h = _handle("/run/ws")
    assert h.to_container_cwd("/run/ws") == CONTAINER_WORKSPACE


def test_to_container_cwd_outside_workspace_falls_back_to_root():
    # a cwd outside the mounted workspace must never escape — clamp to workspace.
    h = _handle("/run/ws")
    assert h.to_container_cwd("/etc/passwd") == CONTAINER_WORKSPACE


def test_to_container_path_maps_account_mount():
    h = ContainerHandle(
        run_id="run-x",
        host_workspace="/run/ws",
        container="muteki-run-x",
        account_root="/run/sessions/_secrets/accounts",
    )
    assert (
        h.to_container_path("/run/sessions/_secrets/accounts/claude-main/CLAUDE_CODE_OAUTH_TOKEN")
        == "/run/muteki/accounts/claude-main/CLAUDE_CODE_OAUTH_TOKEN"
    )


# ── argv binary translation (host CLI path → in-container command) ────────────

def test_containerize_argv_replaces_host_path_with_bare_command():
    argv = ["/Users/x/.local/bin/claude", "-p", "--session-id", "abc"]
    assert _containerize_argv("claude", argv)[0] == "claude"
    assert _containerize_argv("claude", argv)[1:] == argv[1:]


def test_containerize_argv_cursor_maps_to_cursor_agent_abs_path():
    # cursor-agent lives in ~/.local/bin (NOT on the container's default PATH) → the
    # container binary must be the ABSOLUTE path, else exec-not-found.
    assert _containerize_argv("cursor", ["/opt/cursor-agent", "-p"])[0] == "/home/kali/.local/bin/cursor-agent"


def test_containerize_argv_unknown_engine_strips_dir():
    assert _containerize_argv("weird", ["/a/b/weird-bin", "-x"])[0] == "weird-bin"


def test_worker_image_installs_blackboard_skill_for_all_engine_user_scopes():
    """Container workers need both the compat CLI and discoverable user-scope skills.

    Claude/Cursor discover ~/.claude/skills; Codex discovers ~/.agents/skills.
    Keeping this as a static test prevents the image from silently regressing to
    only shipping /usr/local/bin/blackboard.py.
    """
    repo = os.path.dirname(os.path.dirname(__file__))
    dockerfile = open(os.path.join(repo, "docker", "worker", "Dockerfile"), encoding="utf-8").read()
    build_sh = open(os.path.join(repo, "docker", "worker", "build.sh"), encoding="utf-8").read()

    assert "blackboard.SKILL.md" in build_sh
    assert "blackboard.py" in build_sh
    assert "/usr/local/bin/blackboard.py" in dockerfile
    assert "/home/kali/.claude/skills/muteki-blackboard" in dockerfile
    assert "/home/kali/.agents/skills/muteki-blackboard" in dockerfile
    assert "/opt/muteki/muteki-blackboard/SKILL.md" in dockerfile


def test_worker_images_wrap_package_managers_with_auto_sudo():
    """Slim workers are intentionally light, but agents must be able to install
    missing tools even when they forget to prefix apt/dpkg commands with sudo."""
    repo = os.path.dirname(os.path.dirname(__file__))
    for rel in ("docker/worker/Dockerfile", "docker/worker-slim/Dockerfile"):
        dockerfile = open(os.path.join(repo, rel), encoding="utf-8").read()
        assert "muteki-auto-sudo" in dockerfile
        assert "/usr/local/bin/apt-get" in dockerfile
        assert "/usr/local/bin/apt" in dockerfile
        assert "/usr/local/bin/dpkg" in dockerfile
        assert 'exec sudo -n "$real" "$@"' in dockerfile


# ── legacy docker exec command shape (_DockerExecBackend fallback) ────────────

def test_exec_argv_targets_the_run_container_with_cwd_and_sentinel():
    h = _handle("/run/ws", container="muteki-run-nyu_2021q-x")
    cmd = _DockerExecBackend._exec_argv(
        h, ["/host/claude", "-p"], container_cwd=CONTAINER_WORKSPACE,
        env=None, driver_name="claude", tag="deadbeef", timeout=720)
    joined = " ".join(cmd)
    assert cmd[0] == "docker" and cmd[1] == "exec"
    # exec INTO the run's single long-lived container (not a fresh per-worker run)
    assert "muteki-run-nyu_2021q-x" in cmd
    assert "--rm" not in cmd  # exec, not run --rm
    # cwd is the worker's dir inside the bind-mounted workspace
    assert "-w" in cmd and CONTAINER_WORKSPACE in cmd
    assert "nyu_ctf_bench" not in joined
    # argv[0] translated to the bare container command (inside the sh -c string)
    assert "claude -p" in joined and "/host/claude" not in joined
    # per-worker kill sentinel rides in the cmdline ($0) + MUTEKI_WTAG env
    assert "MUTEKI_WTAG=deadbeef" in cmd
    assert "muteki_wtag_deadbeef" in cmd
    # the wall-clock cap is container-side timeout -s KILL, stdin from /dev/null,
    # and NO setsid (worker must stay the exec foreground)
    assert "exec timeout -s KILL 720s" in joined
    assert "< /dev/null" in joined
    assert "setsid" not in joined


def test_exec_argv_passes_only_whitelisted_env():
    h = _handle("/run/ws")
    cmd = _DockerExecBackend._exec_argv(
        h, ["/host/claude"], container_cwd=CONTAINER_WORKSPACE,
        env={
            "MUTEKI_X": "1",
            "ANTHROPIC_Y": "2",
            "HOME": "/leak",
            "PATH": "/leak",
            "CLAUDE_CODE_OAUTH_TOKEN_FILE": "/run/muteki/accounts/claude-main/CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN": "plain-secret",
        },
        driver_name="claude", tag="t1", timeout=600)
    assert "MUTEKI_X=1" in cmd
    assert "ANTHROPIC_Y=2" in cmd
    assert "CLAUDE_CODE_OAUTH_TOKEN_FILE=/run/muteki/accounts/claude-main/CLAUDE_CODE_OAUTH_TOKEN" in cmd
    assert "CLAUDE_CODE_OAUTH_TOKEN=plain-secret" in cmd
    # host HOME/PATH must NOT be forwarded (the container has its own)
    assert "HOME=/leak" not in cmd
    assert "PATH=/leak" not in cmd
    assert 'cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE"' in " ".join(cmd)


def test_exec_argv_expands_api_key_files_inside_container():
    h = _handle("/run/ws")
    cmd = _DockerExecBackend._exec_argv(
        h, ["/host/codex"], container_cwd=CONTAINER_WORKSPACE,
        env={
            "OPENAI_API_KEY_FILE": "/run/muteki/accounts/deepseek-main/API_KEY",
            "OPENAI_BASE_URL": "https://api.deepseek.example/v1",
            "ANTHROPIC_API_KEY_FILE": "/run/muteki/accounts/anthropic-main/API_KEY",
        },
        driver_name="codex", tag="api", timeout=600)
    joined = " ".join(cmd)
    assert "OPENAI_API_KEY_FILE=/run/muteki/accounts/deepseek-main/API_KEY" in cmd
    assert "OPENAI_BASE_URL=https://api.deepseek.example/v1" in cmd
    assert "ANTHROPIC_API_KEY_FILE=/run/muteki/accounts/anthropic-main/API_KEY" in cmd
    assert 'cat "$OPENAI_API_KEY_FILE"' in joined
    assert 'cat "$ANTHROPIC_API_KEY_FILE"' in joined
    assert "deepseek-secret" not in joined


def test_exec_argv_allows_only_isolated_container_home():
    h = _handle("/run/ws")
    cmd = _DockerExecBackend._exec_argv(
        h, ["/host/codex"], container_cwd=CONTAINER_WORKSPACE,
        env={"HOME": f"{CONTAINER_WORKSPACE}/workers/_homes/cli-codex"},
        driver_name="codex", tag="t2", timeout=600)
    assert f"HOME={CONTAINER_WORKSPACE}/workers/_homes/cli-codex" in cmd


# ── ensure_container mounts (rcp default: workspace + control + accounts) ──────

def _fake_docker_factory(calls):
    def fake_docker(*args, **kwargs):
        calls.append(args)
        if args[:2] == ("image", "inspect"):
            return type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})()
        if args[:2] == ("inspect", "-f"):
            return type("R", (), {"returncode": 1, "stdout": "", "stderr": "no"})()
        if args and args[0] == "run":
            return type("R", (), {"returncode": 0, "stdout": "cid\n", "stderr": ""})()
        return type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})()
    return fake_docker


def test_ensure_container_rcp_mounts_workspace_control_and_accounts(monkeypatch, tmp_path):
    calls = []
    import muteki.solver.container_exec as ce
    monkeypatch.setattr(ce, "_docker", _fake_docker_factory(calls))
    monkeypatch.setattr(ce, "_USE_DOCKEREXEC", False)
    # rcp mode waits for the supervisor — stub that out (no real container).
    monkeypatch.setattr(ce, "_await_supervisor", lambda handle: None)
    # stub the receiver so the test doesn't bind the real control port; capture the
    # token it registers.
    import muteki.solver.control_receiver as cr
    expected = {}
    class _FakeRcv:
        def expect(self, run_id, token): expected[run_id] = token
    monkeypatch.setattr(cr.ControlReceiver, "instance", classmethod(lambda cls: _FakeRcv()))
    ws = tmp_path / "run" / "workspace"
    accounts = tmp_path / "_secrets" / "accounts"
    acct = accounts / "codex-main"
    (acct / "codex-home").mkdir(parents=True)
    (acct / "API_KEY").write_text("sk-secret\n")
    (acct / "codex-home" / "auth.json").write_text('{"tok":"x"}\n')
    os.chmod(acct / "API_KEY", 0o600)
    os.chmod(acct / "codex-home" / "auth.json", 0o600)

    handle = ensure_container(
        "run-x", str(ws), account_root=str(accounts), image="img",
        network="bridge", memory="12g", cpus="4", pids_limit=2048)

    run_call = next(a for a in calls if a and a[0] == "run")
    joined = " ".join(run_call)
    # rcp mode: ENTRYPOINT supervisor → NO `sleep infinity`, NO published port.
    assert "sleep" not in run_call
    assert "-p" not in run_call
    assert handle.mode == "rcp"
    # reverse-connect: control dir mounted (carries token) + token written +
    # supervisor told to --connect host.docker.internal --run-id, + --add-host.
    control_dir = ws / ".muteki_control"
    assert f"source={control_dir},target={CONTAINER_CONTROL_DIR}" in joined
    assert handle.control_dir == str(control_dir)
    assert handle.token and (control_dir / "token").read_text() == handle.token
    assert (control_dir / "token").stat().st_mode & 0o777 == 0o600
    assert "--connect" in run_call and "--run-id" in run_call and "run-x" in run_call
    assert "host.docker.internal:host-gateway" in joined  # Linux dial-back
    # workspace + account projection mounts (unchanged from before).
    projection = ws / ".muteki_accounts"
    assert handle.account_root == str(projection)
    assert f"source={projection},target=/run/muteki/accounts" in joined
    assert f"source={ws},target={CONTAINER_WORKSPACE}" in joined
    assert "--tmpfs" in run_call and "/tmp:rw,exec,size=2g" in run_call
    assert "--network bridge" in joined
    assert "--memory 12g" in joined
    # the host store is untouched (still 0600), the projection is container-readable
    assert (acct / "API_KEY").stat().st_mode & 0o777 == 0o600
    proj_auth = projection / "codex-main" / "codex-home" / "auth.json"
    assert proj_auth.stat().st_mode & 0o002, "#14: codex auth.json writable in projection"


# ── shared workspace is made kali-writable (blackboard readonly regression) ───
# Bug: graph/shared_graph.db (the team board) is created root-owned host-side and
# bind-mounted into the kali worker, so worker writes hit sqlite "readonly db".
# ensure_container must chown the workspace tree to the worker uid first.

def test_chown_tree_to_worker_noop_when_not_root(monkeypatch, tmp_path):
    import muteki.solver.container_exec as ce
    f = tmp_path / "graph" / "shared_graph.db"
    f.parent.mkdir(parents=True)
    f.write_text("db")
    monkeypatch.setattr(os, "geteuid", lambda: 1000)  # non-root
    chowns = []
    monkeypatch.setattr(os, "chown", lambda *a, **k: chowns.append(a))
    ce._chown_tree_to_worker(str(tmp_path))
    assert chowns == [], "non-root must not attempt chown (can't, and same uid anyway)"


def test_worker_uid_gid_detects_image_kali_user(monkeypatch):
    import muteki.solver.container_exec as ce
    ce._WORKER_ID_CACHE.clear()
    monkeypatch.delenv("MUTEKI_WORKER_UID", raising=False)
    monkeypatch.delenv("MUTEKI_WORKER_GID", raising=False)
    calls = []

    def fake_docker(*args, **kwargs):
        calls.append(args)
        if args[:2] == ("image", "inspect"):
            return type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})()
        if args and args[0] == "run":
            return type("R", (), {"returncode": 0, "stdout": "4242\n4243\n", "stderr": ""})()
        raise AssertionError(args)

    monkeypatch.setattr(ce, "_docker", fake_docker)

    assert ce._worker_uid_gid("worker:uid-test") == (4242, 4243)
    assert ce._worker_uid_gid("worker:uid-test") == (4242, 4243)
    assert [a[0] for a in calls].count("run") == 1, "image uid lookup should be cached"


def test_worker_uid_gid_env_override_skips_image_probe(monkeypatch):
    import muteki.solver.container_exec as ce
    ce._WORKER_ID_CACHE.clear()
    monkeypatch.setenv("MUTEKI_WORKER_UID", "2000")
    monkeypatch.setenv("MUTEKI_WORKER_GID", "2001")
    monkeypatch.setattr(ce, "_docker", lambda *a, **k: (_ for _ in ()).throw(AssertionError(a)))

    assert ce._worker_uid_gid("worker:override-test") == (2000, 2001)


def test_chown_tree_to_worker_recurses_when_root(monkeypatch, tmp_path):
    import muteki.solver.container_exec as ce
    (tmp_path / "graph").mkdir()
    (tmp_path / "graph" / "shared_graph.db").write_text("db")
    (tmp_path / "winner.json").write_text("{}")
    monkeypatch.setattr(os, "geteuid", lambda: 0)  # simulate root
    monkeypatch.setattr(ce, "_worker_uid_gid", lambda image=ce.WORKER_IMAGE: (1234, 1235))
    chowned = {}
    def fake_chown(path, uid, gid):
        chowned[os.path.abspath(path)] = (uid, gid)
    monkeypatch.setattr(os, "chown", fake_chown)
    ce._chown_tree_to_worker(str(tmp_path))
    db = os.path.abspath(str(tmp_path / "graph" / "shared_graph.db"))
    assert db in chowned, "the shared board DB must be chowned to the worker uid"
    assert chowned[db] == (1234, 1235)
    # the dir tree + sibling files are covered too
    assert os.path.abspath(str(tmp_path)) in chowned
    assert os.path.abspath(str(tmp_path / "graph")) in chowned


def test_chown_tree_to_worker_does_not_follow_skill_symlinks(monkeypatch, tmp_path):
    import muteki.solver.container_exec as ce
    target = tmp_path / "image-skill"
    target.mkdir()
    home = tmp_path / "home"
    home.mkdir()
    link = home / "muteki-blackboard"
    link.symlink_to(target, target_is_directory=True)
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr(ce, "_worker_uid_gid", lambda image=ce.WORKER_IMAGE: (1234, 1235))
    chowned = []
    lchowned = []
    monkeypatch.setattr(os, "chown", lambda path, uid, gid: chowned.append(os.path.abspath(path)))
    monkeypatch.setattr(os, "lchown", lambda path, uid, gid: lchowned.append(os.path.abspath(path)))

    ce._chown_tree_to_worker(str(home))

    assert os.path.abspath(str(home)) in chowned
    assert os.path.abspath(str(link)) in lchowned
    assert os.path.abspath(str(target)) not in chowned


def test_ensure_container_chowns_workspace_to_worker(monkeypatch, tmp_path):
    # End to end through ensure_container: a pre-existing root-owned board DB under
    # the workspace gets chowned to the worker uid before the container comes up.
    calls = []
    import muteki.solver.container_exec as ce
    monkeypatch.setattr(ce, "_docker", _fake_docker_factory(calls))
    monkeypatch.setattr(ce, "_USE_DOCKEREXEC", False)
    monkeypatch.setattr(ce, "_await_supervisor", lambda handle: None)
    import muteki.solver.control_receiver as cr
    class _FakeRcv:
        def expect(self, run_id, token): pass
    monkeypatch.setattr(cr.ControlReceiver, "instance", classmethod(lambda cls: _FakeRcv()))
    ws = tmp_path / "run" / "workspace"
    (ws / "graph").mkdir(parents=True)
    (ws / "graph" / "shared_graph.db").write_text("db")  # pre-created (swarm bootstrap)
    monkeypatch.setattr(os, "geteuid", lambda: 0)  # simulate the root web process
    chowned = []
    monkeypatch.setattr(os, "chown", lambda p, u, g: chowned.append((os.path.abspath(p), u, g)))

    ensure_container("run-cw", str(ws), image="img", network="bridge")

    db = os.path.abspath(str(ws / "graph" / "shared_graph.db"))
    assert any(p == db and (u, g) == (ce._WORKER_UID, ce._WORKER_GID) for p, u, g in chowned), \
        "ensure_container must chown the shared board DB to the worker uid"


def test_ensure_container_rcp_upgrades_none_network_to_bridge(monkeypatch, tmp_path):
    # rcp supervisor must DIAL OUT to the host receiver → `--network none` (no net)
    # would strand it. ensure_container upgrades none→bridge (offline is enforced by
    # CLI flags, not network). Regression: this was dropped once and every offline
    # container run silently degraded to local.
    calls = []
    import muteki.solver.container_exec as ce
    import muteki.solver.control_receiver as cr
    monkeypatch.setattr(ce, "_docker", _fake_docker_factory(calls))
    monkeypatch.setattr(ce, "_USE_DOCKEREXEC", False)
    monkeypatch.setattr(ce, "_await_supervisor", lambda handle: None)
    class _FakeRcv:
        def expect(self, *a): pass
    monkeypatch.setattr(cr.ControlReceiver, "instance", classmethod(lambda cls: _FakeRcv()))
    ws = tmp_path / "run" / "workspace"
    handle = ensure_container("run-off", str(ws), image="img", network="none")
    assert handle.network == "bridge"  # upgraded
    run_call = next(a for a in calls if a and a[0] == "run")
    assert "--network bridge" in " ".join(run_call)
    assert "none" not in [x for i, x in enumerate(run_call) if i > 0 and run_call[i-1] == "--network"]


def test_ensure_container_dockerexec_appends_sleep_infinity(monkeypatch, tmp_path):
    calls = []
    import muteki.solver.container_exec as ce
    monkeypatch.setattr(ce, "_docker", _fake_docker_factory(calls))
    monkeypatch.setattr(ce, "_USE_DOCKEREXEC", True)
    ws = tmp_path / "run" / "workspace"
    handle = ensure_container("run-y", str(ws), image="img", network="bridge")
    run_call = next(a for a in calls if a and a[0] == "run")
    # legacy mode: keepalive is `sleep infinity`, no control mount.
    assert run_call[-2:] == ("sleep", "infinity")
    assert "--tmpfs" in run_call and "/tmp:rw,exec,size=2g" in run_call
    assert handle.mode == "dockerexec"
    assert "/run/muteki/control" not in " ".join(run_call)


# ── signal routing ────────────────────────────────────────────────────────────

def _fake_popen():
    class _P:
        pid = 4321
        def kill(self):  # noqa: D401
            self.killed = True
    return _P()


def test_legacy_container_signal_maps_to_pkill_actions(monkeypatch):
    calls = []
    import muteki.solver.container_exec as ce
    monkeypatch.setattr(
        ce, "_docker",
        lambda *a, **k: calls.append(a) or type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})())
    proc = _ContainerProc("muteki-run-test", "tagX", _fake_popen())
    proc._container_signal(signal.SIGSTOP)
    proc._container_signal(signal.SIGCONT)
    proc._container_signal(signal.SIGKILL)
    sigs = [a[a.index("pkill") + 1] for a in calls if "pkill" in a]
    assert "-STOP" in sigs   # SIGSTOP → pkill -STOP
    assert "-CONT" in sigs   # SIGCONT → pkill -CONT
    assert "-KILL" in sigs   # SIGKILL → pkill -KILL
    for a in calls:
        assert "muteki-run-test" in a
        assert "muteki_wtag_tagX" in a


def test_rcp_proc_signal_maps_to_control_ops():
    # the rcp proc routes STOP/CONT/KILL to the link's Signal op (worker-scoped).
    import muteki.solver.control_client as cc
    sent = []
    class _FakeLink:
        def signal(self, worker_id, name, **k): sent.append((worker_id, name))
    proc = cc._RcpProc(_FakeLink(), "w-1-abcd")
    proc._container_signal(signal.SIGSTOP)
    proc._container_signal(signal.SIGCONT)
    proc._container_signal(signal.SIGKILL)
    proc.kill()
    assert [n for _, n in sent] == ["STOP", "CONT", "KILL", "KILL"]
    assert all(w == "w-1-abcd" for w, _ in sent)


def test_signal_proc_prefers_container_routing():
    # the solver's _signal_proc must route through _container_signal when present
    # (so a container worker's pause/kill goes into the container, not the host).
    from muteki.solver.cli_solver import CliSolver
    seen = []
    class _CP:
        pid = 999
        def _container_signal(self, sig): seen.append(sig)
    CliSolver._signal_proc(_CP(), signal.SIGKILL)
    assert seen == [signal.SIGKILL]


# ── run dispatch: rcp (default) vs legacy docker-exec ─────────────────────────

def test_run_cli_container_rcp_dispatch_records_runtime_status(monkeypatch):
    import muteki.solver.container_exec as ce

    class Driver:
        name = "codex"
        def parse(self, out, err):
            return CliResult(text=out)

    monkeypatch.setattr(ce, "_ensure_alive", lambda handle: None)

    # stub the rcp transport — assert container_exec forwards the container-side
    # argv + cwd + run_id and wraps the result with the registry record.
    captured = {}
    def fake_run_cli_rcp(driver, argv, *, run_id, container_cwd, timeout, env=None):
        captured.update(argv=argv, run_id=run_id, cwd=container_cwd)
        r = CliResult(text="ok")
        r.runtime_status = {"backend": "container_rcp", "status": "finished", "rc": 0}
        return r
    import muteki.solver.control_client as cc
    monkeypatch.setattr(cc, "run_cli_rcp", fake_run_cli_rcp)

    handle = ContainerHandle(run_id="nyu:rcp", host_workspace="/run/ws",
                             container="muteki-run-rcp", mode="rcp", token="tk")
    res = run_cli_container(Driver(), ["/host/codex", "exec"], handle=handle,
                            cwd="/run/ws", timeout=30, env={})
    assert res.text == "ok"
    assert captured["argv"][0] == "codex"  # host path translated to container bin
    assert captured["run_id"] == "nyu:rcp"
    assert captured["cwd"] == CONTAINER_WORKSPACE
    # the host-side registry wraps it as backend=container with a finished status.
    assert res.runtime_status["backend"] == "container"
    assert res.runtime_status["status"] == "finished"
    assert runtime_execs_for_run("nyu:rcp")[-1]["exec_id"] == res.runtime_status["exec_id"]


def test_run_cli_container_dockerexec_dispatch(monkeypatch):
    import muteki.solver.container_exec as ce

    class Driver:
        name = "codex"
        def parse(self, out, err):
            return CliResult(text=out)

    class R:
        returncode = 0
        stdout = "ok"
        stderr = ""

    monkeypatch.setattr(ce, "_ensure_alive", lambda handle: None)
    monkeypatch.setattr(ce, "_oom_kill_count", lambda container: 0)
    monkeypatch.setattr(ce.subprocess, "run", lambda *a, **k: R())

    handle = _handle("/run/ws", container="muteki-run-runtime", mode="dockerexec")
    res = run_cli_container(
        Driver(), ["/host/codex", "exec"], handle=handle,
        cwd="/run/ws", timeout=30, env={})

    assert res.text == "ok"
    assert res.runtime_status["backend"] == "container"
    assert res.runtime_status["status"] == "finished"
    assert res.runtime_status["container"] == "muteki-run-runtime"


# ── P2-v3 BLOCKER-c: host-path translation for sibling-container mounts ────────

def test_mount_source_identity_on_bare_host(monkeypatch):
    # No MUTEKI_HOST_DATA_ROOT → identity (abspath), the historical behaviour.
    monkeypatch.setattr(cx, "_HOST_DATA_ROOT", "")
    monkeypatch.setattr(cx, "_CONTAINER_DATA_ROOT", "")
    assert _mount_source("/some/abs/path") == "/some/abs/path"


def test_mount_source_identity_mirror(monkeypatch):
    # host root == container root (path mirrored at the SAME path) → identity.
    monkeypatch.setattr(cx, "_HOST_DATA_ROOT", "/opt/muteki/data")
    monkeypatch.setattr(cx, "_CONTAINER_DATA_ROOT", "/opt/muteki/data")
    assert _mount_source("/opt/muteki/data/run-x/ws") == "/opt/muteki/data/run-x/ws"


def test_mount_source_remaps_container_prefix_to_host(monkeypatch):
    # container data root differs from host root → remap the prefix so the HOST
    # daemon binds the real host path, not the (nonexistent) in-container path.
    monkeypatch.setattr(cx, "_HOST_DATA_ROOT", "/opt/muteki/data")
    monkeypatch.setattr(cx, "_CONTAINER_DATA_ROOT", "/app/data")
    assert _mount_source("/app/data/run-x/ws") == "/opt/muteki/data/run-x/ws"
    assert _mount_source("/app/data") == "/opt/muteki/data"


def test_mount_source_outside_root_passes_through(monkeypatch):
    monkeypatch.setattr(cx, "_HOST_DATA_ROOT", "/opt/muteki/data")
    monkeypatch.setattr(cx, "_CONTAINER_DATA_ROOT", "/app/data")
    # not under the mirrored root → pass through (best effort)
    assert _mount_source("/elsewhere/x") == "/elsewhere/x"
    # guard: /app/data2 must NOT match the /app/data prefix
    assert _mount_source("/app/data2/y") == "/app/data2/y"
