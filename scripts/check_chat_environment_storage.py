"""Regression checks for native environment storage, without model calls."""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import asyncio
from hashlib import sha256
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from unittest.mock import patch
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from apps.web.worker_models import _temporary_model_probe_env
from muteki.conversation.chat_plugins import ChatPluginError, ChatPluginService
from muteki.conversation.chat_providers import provider_for
from muteki.conversation.executor import ExternalAgentSessionExecutor


async def check_archive_order() -> None:
    executor = object.__new__(ExternalAgentSessionExecutor)
    executor._session_record = lambda _: SimpleNamespace(closed_at=None)
    released = []
    executor.chat_plugins = SimpleNamespace(release_thread_assets=lambda identity: released.append(identity))
    async def failed_close(record, *, reason, strict):
        assert strict
        raise RuntimeError("synthetic process close failure")
    executor._close_record = failed_close
    try:
        await executor.close_thread("thread")
    except RuntimeError:
        pass
    else:
        raise AssertionError("failed process close was hidden")
    assert not released
    async def closed(record, *, reason, strict):
        assert strict
    executor._close_record = closed
    assert await executor.close_thread("thread")
    assert released == ["thread"]


def check() -> None:
    asyncio.run(check_archive_order())
    with tempfile.TemporaryDirectory(prefix="muteki-storage-check-") as directory:
        root = Path(directory)
        host = root / "host"
        native = host / ".codex"
        asset = native / "plugins/package/data.bin"
        asset.parent.mkdir(parents=True)
        asset.write_bytes(b"immutable-test-data" * 100000)
        (native / "config.toml").write_text('model = "example"\n')
        (native / "auth.json").write_text('{"token":"first"}')
        service = ChatPluginService(root / "state")
        with patch.dict(os.environ, {}, clear=True), patch("pathlib.Path.home", return_value=host), patch("muteki.conversation.chat_providers.native_user_home", return_value=host), patch("muteki.conversation.chat_plugins.native_user_home", return_value=host):
            provider = provider_for("codex")
            first_revision = provider.revision()
            first = service.prepare_environment("codex", "thread:account", {}, isolated=True)
            private = Path(first["CODEX_HOME"])
            data = private / "plugins/package/data.bin"
            inode = data.stat().st_ino
            history = private / "sessions/history.jsonl"
            history.parent.mkdir()
            history.write_text("preserve native history\n")
            for index in range(20):
                (native / "auth.json").write_text(json.dumps({"token": str(index)}))
                os.utime(native / "config.toml", None)
                assert provider.revision() == first_revision
                result = service.prepare_environment("codex", "thread:account", {}, isolated=True)
                assert result["CODEX_HOME"] == first["CODEX_HOME"]
                assert data.stat().st_ino == inode, "unchanged assets were recopied"
            assert len(list((service.root / "sessions").iterdir())) == 1
            assert not (service.root / "native-cache").exists()
            assert json.loads((private / "auth.json").read_text())["token"] == "19"

            # Same-length content changes are detected; source deletion removes
            # only unchanged imports. Native edits and histories are retained.
            asset.write_bytes(b"changed-test-data!!" * 100000)
            assert provider.revision() != first_revision
            service.prepare_environment("codex", "thread:account", {}, isolated=True)
            assert data.read_bytes() == asset.read_bytes()
            data.write_bytes(b"session-local edit")
            asset.unlink()
            service.prepare_environment("codex", "thread:account", {}, isolated=True)
            assert data.read_bytes() == b"session-local edit"
            assert history.read_text() == "preserve native history\n"

            extra = native / "plugins/package/extra.txt"
            extra.write_text("unchanged import")
            service.prepare_environment("codex", "thread:account", {}, isolated=True)
            assert service.release_thread_assets("thread") > 0
            assert not (private / "plugins/package/extra.txt").exists()
            assert history.exists() and data.exists()
            service.prepare_environment("codex", "thread:account", {}, isolated=True)
            assert (private / "plugins/package/extra.txt").read_text() == "unchanged import"

            package = root / "extension-source"
            (package / "extensions").mkdir(parents=True)
            (package / "extensions/main.js").write_text("export default function(api) {}")
            (package / "plugin.json").write_text(json.dumps({
                "name": "storage-extension", "version": "1.0.0",
                "muteki": {"extensions": {"pi": ["extensions/main.js"]}},
            }))
            service.install({"path": str(package)})
            ext_env = service.prepare_environment("pi", "extension:account", {}, isolated=True)
            ext_home = Path(ext_env["PI_CODING_AGENT_DIR"])
            assert list((ext_home / "extensions").glob("muteki-storage-extension-*.js"))
            service.update("storage-extension", enabled=False)
            ext_after = service.prepare_environment("pi", "extension:account", {}, isolated=True)
            assert ext_after["HOME"] == ext_env["HOME"]
            assert not list((ext_home / "extensions").glob("muteki-storage-extension-*.js"))

            # Adopt the precise recorded legacy home without moving absolute
            # resume paths or losing native state.
            revision = "recorded-old-revision"
            legacy = service.root / "sessions" / sha256(f"v3:codex:legacy:account:{revision}".encode()).hexdigest()[:24]
            legacy.mkdir()
            (legacy / ".ready").touch()
            (legacy / "native-history").write_text("old history")
            adopted = service.prepare_environment("codex", "legacy:account", {}, previous_revision=revision, isolated=True)
            assert adopted["MUTEKI_CHAT_PRIVATE_ROOT"] == str(legacy)
            assert (legacy / "native-history").read_text() == "old history"

            # Separate service instances must serialize the same identity.
            def prepare(_):
                return ChatPluginService(root / "state").prepare_environment("codex", "parallel:account", {}, isolated=True)
            with ThreadPoolExecutor(max_workers=8) as pool:
                homes = {r["CODEX_HOME"] for r in pool.map(prepare, range(16))}
            assert len(homes) == 1

            # A failed import must stay retryable and expose its actual cause.
            with patch("muteki.conversation.chat_plugins._snapshot_copy", side_effect=OSError(28, "synthetic full disk")):
                try:
                    service.prepare_environment("codex", "failed:account", {}, isolated=True)
                except ChatPluginError as exc:
                    assert exc.code == "chat_plugin.environment_prepare_failed"
                    assert isinstance(exc.__cause__, OSError) and exc.__cause__.errno == 28
                else:
                    raise AssertionError("import failure hidden")
            recovered = service.prepare_environment("codex", "failed:account", {}, isolated=True)
            assert (Path(recovered["CODEX_HOME"]) / "config.toml").is_file()

            # Probe assets are omitted, even when source plugins are large.
            try:
                with _temporary_model_probe_env("codex", root / "probes", "account", {}) as env:
                    probe = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"])
                    assert not (Path(env["CODEX_HOME"]) / "plugins").exists()
                    raise RuntimeError("synthetic probe failure")
            except RuntimeError:
                pass
            assert not probe.exists()
            assert not list((root / "probes/_model_probe_environments").iterdir())

            cursor = host / ".cursor/extensions/desktop-extension"
            cursor.mkdir(parents=True)
            (cursor / "large.bin").write_bytes(b"not for the CLI")
            env = service.prepare_environment("cursor", "cursor:account", {}, isolated=True)
            assert not (Path(env["CURSOR_CONFIG_DIR"]) / "extensions").exists()

            # Ordinary chats retain native login/config paths, while Muteki
            # extensions and hooks live in application-owned resources.
            supplied = {"CODEX_HOME": str(native), "XDG_CONFIG_HOME": str(host / ".config")}
            shared = service.prepare_environment("codex", "shared:account", supplied)
            assert shared["CODEX_HOME"] == str(native)
            assert shared["XDG_CONFIG_HOME"] == supplied["XDG_CONFIG_HOME"]
            assert not shared.get("MUTEKI_CHAT_PRIVATE_ROOT")
            assert shared["USERPROFILE"] == shared["HOME"]
            assert not Path(shared["XDG_DATA_HOME"]).is_relative_to(service.root)
            assert Path(shared["MUTEKI_CHAT_ASSETS_ROOT"]).is_relative_to(service.root)
            assert (native / "config.toml").read_text() == 'model = "example"\n'
            assert json.loads((native / "auth.json").read_text())["token"] == "19"
            service.update("storage-extension", enabled=True)
            pi_native = host / ".pi/agent"
            shared_pi = service.prepare_environment("pi", "shared-pi:account", {"PI_CODING_AGENT_DIR": str(pi_native)})
            options = service.native_launch_options("pi", shared_pi)
            assert options["chat_native_extensions"]
            assert all(Path(p).is_relative_to(Path(shared_pi["MUTEKI_CHAT_ASSETS_ROOT"])) for p in options["chat_native_extensions"])
            assert not pi_native.exists(), "Muteki extensions wrote into native config"

            # Legacy transcripts migrate without copying auth/config, remain
            # available for rollback, and never overwrite a resumed transcript.
            native_thread_id = "00000000-0000-0000-0000-000000000001"
            rollout = private / "sessions" / ("rollout-2026-10-09-" + native_thread_id + ".jsonl")
            rollout.write_text(json.dumps({"type": "session_meta", "payload": {"id": native_thread_id}}) + "\n")
            service.prepare_environment("codex", "thread:account", supplied)
            assert service.codex_rollout_path("thread:account", native_thread_id, supplied) == str(native / "sessions" / rollout.name)
            assert service.codex_rollout_path("shared:account", native_thread_id, supplied) == ""
            assert (native / "sessions/history.jsonl").read_text() == "preserve native history\n"
            assert history.exists()
            (native / "sessions/history.jsonl").write_text("updated by native client\n")
            service.prepare_environment("codex", "thread:account", supplied)
            assert (native / "sessions/history.jsonl").read_text() == "updated by native client\n"
            service.prepare_codex_fork_history("thread:account", supplied)
            assert history.exists()

            from muteki.conversation.native_environment import environment_home
            import sqlite3
            cursor_id = "00000000-0000-0000-0000-000000000002"
            with environment_home(service.root, "cursor", "cursor-history:account") as cursor_root:
                cursor_chat = cursor_root / "home/.cursor/chats/workspace-hash" / cursor_id
                cursor_chat.mkdir(parents=True)
                (cursor_chat / "meta.json").write_text(json.dumps({"id": cursor_id, "title": "synthetic history"}))
            old_cursor = sqlite3.connect(cursor_chat / "store.db")
            try:
                old_cursor.execute("PRAGMA journal_mode=WAL")
                old_cursor.execute("CREATE TABLE history(id TEXT PRIMARY KEY, body TEXT)")
                old_cursor.execute("INSERT INTO history VALUES ('original','committed WAL history')")
                old_cursor.commit()
                assert (cursor_chat / "store.db-wal").exists()
                cursor_env = {"CURSOR_CONFIG_DIR": str(host / ".cursor")}
                cursor_target = Path(service.prepare_cursor_history("cursor-history:account", cursor_id, cursor_env))
                assert cursor_target == host / ".cursor/chats/workspace-hash" / cursor_id
                assert not (cursor_target / "store.db-wal").exists(), "raw WAL was copied"
                assert not (cursor_target / "store.db-shm").exists(), "raw SHM was copied"
                native_cursor = sqlite3.connect(cursor_target / "store.db")
                try:
                    assert native_cursor.execute("SELECT body FROM history WHERE id='original'").fetchone()[0] == "committed WAL history"
                    native_cursor.execute("INSERT INTO history VALUES ('native','new native turn')")
                    native_cursor.commit()
                    service.prepare_cursor_history("cursor-history:account", cursor_id, cursor_env)
                    assert native_cursor.execute("SELECT body FROM history WHERE id='native'").fetchone()[0] == "new native turn"
                    old_cursor.execute("UPDATE history SET body='conflicting legacy change' WHERE id='original'")
                    old_cursor.commit()
                    try:
                        service.prepare_cursor_history("cursor-history:account", cursor_id, cursor_env)
                    except ChatPluginError as exc:
                        assert exc.code == "conversation.resume.history_migration_failed"
                        assert "conflict" in str(exc).lower()
                    else:
                        raise AssertionError("Cursor history conflict replaced native chat")
                    assert native_cursor.execute("SELECT body FROM history WHERE id='original'").fetchone()[0] == "committed WAL history"
                    assert (cursor_chat / "store.db").exists()
                finally:
                    native_cursor.close()
            finally:
                old_cursor.close()

            # Writes through symlink destinations are rejected explicitly.
            (private / "config.toml").unlink()
            (private / "config.toml").symlink_to(native / "config.toml")
            try:
                service.prepare_environment("codex", "thread:account", {}, isolated=True)
            except ChatPluginError as exc:
                assert exc.code == "chat_plugin.environment_prepare_failed"
            else:
                raise AssertionError("unsafe destination accepted")
            assert (native / "config.toml").read_text() == 'model = "example"\n'

        # Exercise interprocess locking with real independent Python processes.
        code = """from pathlib import Path
from muteki.conversation.chat_plugins import ChatPluginService
import sys
print(ChatPluginService(Path(sys.argv[1])).prepare_environment('codex', 'process:account', {}, isolated=True)['CODEX_HOME'])
"""
        env = {**os.environ, "CODEX_HOME": str(native), "HOME": str(host)}
        processes = [subprocess.Popen([sys.executable, "-c", code, str(root / "multiprocess")],
                                      env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                     for _ in range(4)]
        homes = set()
        for process in processes:
            out, err = process.communicate(timeout=30)
            assert process.returncode == 0, err
            homes.add(out.strip())
        assert len(homes) == 1
    print("PASS: explicit isolation, native sharing, resource separation, Codex rollout identity, Cursor WAL backup and conflicts, OpenCode native import receipts and parents, auth/config preservation, concurrency, probe cleanup")


if __name__ == "__main__":
    check()
