from __future__ import annotations

import os
import stat
from collections.abc import Iterator
from pathlib import Path

import pytest
from pydantic import ValidationError

import muteki.control.secrets as secret_module
from apps.web.control_adapter import ControlPayloadError, compile_control_command
from muteki.control.secrets import (
    InvalidSecretReference,
    SecretMetadata,
    SecretNotFound,
    SecretStore,
    SecretStoreError,
)


def _factory(values: list[str]) -> Iterator[str]:
    return iter(values)


def _mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


def test_store_permissions_reference_and_safe_metadata(tmp_path: Path) -> None:
    secret_id = "a" * 32
    value = "not-for-events-or-sqlite"
    root = tmp_path / "run-1" / "control" / "secrets"
    store = SecretStore(root, id_factory=_factory([secret_id]).__next__)

    reference = store.put(value)

    assert reference == f"secret://{secret_id}"
    assert _mode(root) == 0o700
    secret_file = root / f"{secret_id}.secret"
    assert _mode(secret_file) == 0o600
    assert secret_file.read_text(encoding="utf-8") == value
    assert [path.name for path in root.iterdir()] == [f"{secret_id}.secret"]

    metadata = store.get(reference)
    assert metadata == SecretMetadata(reference=reference, secret_id=secret_id)
    assert store.list() == (metadata,)
    assert value not in repr(metadata)
    assert not hasattr(metadata, "value")
    assert not list(root.glob("*.sqlite"))
    assert not list(root.glob("*.log"))
    assert store.resolve(reference) == value


def test_existing_permissions_are_tightened_without_exposing_value(tmp_path: Path) -> None:
    root = tmp_path / "secrets"
    root.mkdir(mode=0o777)
    os.chmod(root, 0o777)
    secret_id = "b" * 32
    path = root / f"{secret_id}.secret"
    path.write_text("classified", encoding="utf-8")
    os.chmod(path, 0o666)

    store = SecretStore(root)
    metadata = store.get(f"secret://{secret_id}")

    assert _mode(root) == 0o700
    assert _mode(path) == 0o600
    assert metadata.reference == f"secret://{secret_id}"
    assert "classified" not in repr(metadata)


def test_atomic_no_overwrite_retries_id_collision(tmp_path: Path) -> None:
    first_id = "c" * 32
    second_id = "d" * 32
    ids = _factory([first_id, first_id, second_id])
    store = SecretStore(tmp_path / "secrets", id_factory=ids.__next__)

    first_ref = store.put("first")
    second_ref = store.put("second")

    assert first_ref == f"secret://{first_id}"
    assert second_ref == f"secret://{second_id}"
    assert store.resolve(first_ref) == "first"
    assert store.resolve(second_ref) == "second"
    assert not list(store.root.glob(".secret-tmp-*"))


def test_atomic_failure_leaves_no_target_or_plaintext_in_error(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    secret_id = "e" * 32
    value = "must-not-leak-via-error"
    store = SecretStore(
        tmp_path / "secrets",
        id_factory=_factory([secret_id]).__next__,
    )

    def fail_link(*_args: object, **_kwargs: object) -> None:
        raise OSError("simulated publication failure")

    monkeypatch.setattr(secret_module.os, "link", fail_link)
    with pytest.raises(SecretStoreError) as caught:
        store.put(value)

    assert value not in str(caught.value)
    assert not (store.root / f"{secret_id}.secret").exists()
    assert not list(store.root.glob(".secret-tmp-*"))


@pytest.mark.parametrize(
    "reference",
    [
        "../outside",
        "secret://../outside",
        "secret:///absolute/path",
        "secret://abc%2Foutside",
        "secret://abc.def",
        "secret://..",
        "secret://ab",
        "secret://abc/def",
    ],
)
def test_references_cannot_traverse_store(tmp_path: Path, reference: str) -> None:
    store = SecretStore(tmp_path / "run" / "secrets")
    outside = tmp_path / "outside"
    outside.write_text("untouched", encoding="utf-8")

    for operation in (store.get, store.resolve, store.delete):
        with pytest.raises(InvalidSecretReference):
            operation(reference)

    assert outside.read_text(encoding="utf-8") == "untouched"
    assert store.list() == ()


def test_symlink_entries_are_never_followed_or_deleted(tmp_path: Path) -> None:
    store = SecretStore(tmp_path / "run" / "secrets")
    outside = tmp_path / "outside"
    outside.write_text("outside-secret", encoding="utf-8")
    secret_id = "f" * 32
    reference = f"secret://{secret_id}"
    entry = store.root / f"{secret_id}.secret"
    entry.symlink_to(outside)

    assert store.list() == ()
    for operation in (store.get, store.resolve, store.delete):
        with pytest.raises(SecretStoreError):
            operation(reference)

    assert entry.is_symlink()
    assert outside.read_text(encoding="utf-8") == "outside-secret"


def test_root_cannot_be_a_symlink(tmp_path: Path) -> None:
    actual = tmp_path / "actual"
    actual.mkdir()
    alias = tmp_path / "alias"
    alias.symlink_to(actual, target_is_directory=True)

    with pytest.raises(SecretStoreError, match="real directory"):
        SecretStore(alias)


def test_run_stores_are_isolated_and_delete_returns_safe_metadata(tmp_path: Path) -> None:
    secret_id = "1" * 32
    run_a = SecretStore(
        tmp_path / "run-a" / "secrets",
        id_factory=_factory([secret_id]).__next__,
    )
    run_b = SecretStore(
        tmp_path / "run-b" / "secrets",
        id_factory=_factory([secret_id]).__next__,
    )
    reference_a = run_a.put("value-a")
    reference_b = run_b.put("value-b")

    assert reference_a == reference_b
    assert run_a.resolve(reference_a) == "value-a"
    assert run_b.resolve(reference_b) == "value-b"

    deleted = run_a.delete(reference_a)
    assert deleted == SecretMetadata(reference=reference_a, secret_id=secret_id)
    assert "value-a" not in repr(deleted)
    with pytest.raises(SecretNotFound):
        run_a.get(reference_a)
    assert run_b.resolve(reference_b) == "value-b"


def test_constructor_removes_only_stale_regular_temp_files(tmp_path: Path) -> None:
    root = tmp_path / "secrets"
    root.mkdir()
    stale = root / ".secret-tmp-stale"
    stale.write_text("interrupted-value", encoding="utf-8")
    outside = tmp_path / "outside"
    outside.write_text("preserve", encoding="utf-8")
    unsafe = root / ".secret-tmp-symlink"
    unsafe.symlink_to(outside)

    SecretStore(root)

    assert not stale.exists()
    assert unsafe.is_symlink()
    assert outside.read_text(encoding="utf-8") == "preserve"


def test_compile_rejects_unknown_secret_reference(tmp_path: Path) -> None:
    store = SecretStore(tmp_path / "secrets")
    with pytest.raises(ControlPayloadError, match="secret reference"):
        compile_control_command(
            "run-1",
            {
                "command_id": "C-fake-ref",
                "action": "hint",
                "payload": {
                    "metadata": {
                        "credential": (
                            "secret://0123456789abcdef0123456789abcdef"),
                    },
                },
            },
            secrets=store,
        )
    assert store.list() == ()


def test_compile_rolls_back_secret_when_command_validation_fails(
    tmp_path: Path,
) -> None:
    store = SecretStore(tmp_path / "secrets")
    with pytest.raises(ValidationError):
        compile_control_command(
            "run-1",
            {
                "command_id": "x",  # model min_length=3, after payload staging
                "action": "hint",
                "text": "password=temporary-value",
            },
            secrets=store,
        )
    assert store.list() == ()


def test_nested_sensitive_note_is_reference_only(tmp_path: Path) -> None:
    store = SecretStore(tmp_path / "secrets")
    plaintext = "password=nested-value"
    command = compile_control_command(
        "run-1",
        {
            "command_id": "C-nested-secret",
            "action": "ask",
            "payload": {"metadata": {"note": plaintext}},
        },
        secrets=store,
    )
    reference = command.payload["metadata"]["note"]
    assert str(reference).startswith("secret://")
    assert plaintext not in command.model_dump_json()
    assert store.resolve(reference) == plaintext
