"""pi.rpc argv must not pass --approve on pi 0.73.1 (#147)."""

from __future__ import annotations

from muteki.external_agents.pi import PiAdapter


def test_rpc_argv_omits_approve_even_when_trust_requested():
    adapter = PiAdapter(binary="pi", trust_project=True)
    argv = adapter._rpc_argv(approve=True, no_session=True)
    assert "--approve" not in argv
    assert "-a" not in argv
    assert argv[:3] == ["pi", "--mode", "rpc"]
    assert "--no-session" in argv


def test_rpc_argv_with_extension_still_omits_approve():
    adapter = PiAdapter(binary="pi", trust_project=True)
    argv = adapter._rpc_argv(
        approve=True,
        no_session=True,
        extension_path="/tmp/fake-extension.ts",
    )
    assert "--approve" not in argv
    assert "--extension" in argv
    assert "/tmp/fake-extension.ts" in argv
    assert "--tools" in argv
    assert "bash,read,edit,write,grep,find,ls" in argv


def test_permission_flags_omit_approve():
    from muteki.solver.cli_engines.engines.pi import PiDriver
    from muteki.solver.cli_engines.types import WORKER_LAUNCH

    driver = PiDriver()
    assert "--approve" not in driver._permission_flags(WORKER_LAUNCH)


def test_pi_is_user_abort_classifies_request_aborted():
    from muteki.external_agents.pi import _pi_is_user_abort

    assert _pi_is_user_abort("Request was aborted.")
    assert _pi_is_user_abort("Command aborted")
    assert _pi_is_user_abort("aborted")
    assert _pi_is_user_abort("", abort_requested=True)
    assert not _pi_is_user_abort("Rate limit exceeded")
    assert not _pi_is_user_abort("")
