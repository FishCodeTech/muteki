"""Concurrency, history, export and baseline checks for SRC assets/monitoring.

Run from the repository root: uv run python scripts/check_src_assets_monitoring.py
Only temporary databases and controlled local inputs are used.
"""
from __future__ import annotations

import asyncio
from pathlib import Path
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from muteki.platform.store import PlatformStore
from muteki.src import SrcService
from muteki.src.assets import SrcAssetService
from muteki.src.contracts import ArtifactRef
from muteki.src.store import SrcStoreError


def make_src(root: Path, platform: PlatformStore) -> SrcService:
    class Conversation:
        def activate_thread(self, thread, principal, selection=None):
            platform.save(thread)

    return SrcService(
        platform, artifact_root=root / "cas", conversation=Conversation(), run_gateway=None,
    )


def make_task(src: SrcService, title: str):
    class Actor:
        id = "local-user"

    thread, task, binding = src.prepare_task(Actor(), {"title": title})
    src.commit_task(thread, task, binding, principal_id="local-user")
    return thread, task


async def check_assets(root: Path) -> None:
    platform = PlatformStore(root / "asset-platform.db")
    src = make_src(root, platform)
    _thread_a, task_a = make_task(src, "assets-a")
    _thread_b, task_b = make_task(src, "assets-b")
    service = SrcAssetService(src)

    async def observe(index: int):
        return await asyncio.to_thread(
            service.observe, task_a.task_id, "domain", {"name": "Example.TEST."},
            source_kind="fixture", source_id=f"source-{index}",
            attributes={"title": f"title-{index}"}, observed_at=f"2026-01-{index + 1:02d}T00:00:00Z",
            raw_values={"name": "Example.TEST."}, success=True,
            source_event_id=f"concurrent-{index}",
        )

    results = await asyncio.gather(*(observe(index) for index in range(8)))
    asset_id = results[0]["asset"]["asset_id"]
    assert len({row["asset"]["asset_id"] for row in results}) == 1
    assert results[0]["asset"]["identity"]["name"] == "example.test"
    detail = service.detail(task_a.task_id, asset_id)
    assert detail["asset"]["observation_count"] == 8 and len(detail["history"]) == 8

    service.observe(task_a.task_id, "domain", {"name": "example.test"}, source_kind="http", source_id="new",
                    attributes={"title": "new"}, observed_at="2026-03-01T00:00:00Z", success=True)
    service.observe(task_a.task_id, "domain", {"name": "example.test"}, source_kind="http", source_id="old",
                    attributes={"title": "old"}, observed_at="2024-01-01T00:00:00Z", success=False)
    projected = service.detail(task_a.task_id, asset_id)["asset"]
    assert projected["current_profile"]["title"].startswith("title-") is False
    assert projected["current_profile"]["title"] == "new"
    assert projected["last_success"] == "2026-03-01T00:00:00+00:00"
    assert projected["last_failure"] == "2024-01-01T00:00:00+00:00"
    assert len(projected["conflicts"]["title"]) == 10
    service.correct(task_a.task_id, asset_id, {"company_attribution": "explicit-owner"}, actor_id="local-user", reason="human confirmed")
    assert service.detail(task_a.task_id, asset_id)["asset"]["current_profile"]["company_attribution"] == "explicit-owner"

    service.lifecycle(task_a.task_id, asset_id, "archive", actor_id="local-user")
    assert service.detail(task_a.task_id, asset_id)["asset"]["lifecycle"] == "archived"
    service.lifecycle(task_a.task_id, asset_id, "restore", actor_id="local-user")
    assert service.detail(task_a.task_id, asset_id)["asset"]["lifecycle"] == "active"
    raw = src.artifacts.put(task_a.task_id, b"Cookie: sid=original-value", media_type="text/plain", name="selected-cookie")
    published = service.publish(
        task_a.task_id, task_b.task_id,
        {"assets": [{"asset_id": asset_id, "fields": ["title"]}], "artifact_refs": [raw.model_dump(mode="json")]},
        actor_id="local-user", purpose="selected reuse",
    )
    publication_id = published["publication"]["publication_id"]
    visible = service.published(task_b.task_id, publication_id)
    assert set(visible["assets"][0]["entity"]["current_profile"]) == {"title"}
    pinned = ArtifactRef.model_validate(visible["artifact_refs"][0])
    assert src.artifacts.read(pinned, expected_task_id=task_b.task_id) == b"Cookie: sid=original-value"
    try:
        service.destroy(task_a.task_id, asset_id, actor_id="local-user")
    except SrcStoreError as exc:
        assert exc.code == "artifact_in_use"
    else:
        raise AssertionError("shared asset destruction ignored publication references")
    service.revoke_publication(task_a.task_id, publication_id, actor_id="local-user")
    try:
        service.published(task_b.task_id, publication_id)
    except SrcStoreError as exc:
        assert exc.code == "source_revoked"
    else:
        raise AssertionError("revoked publication remained readable")
    platform.close()
    print("PASS: asset concurrent identity, immutable history, freshness, publication and revocation")


async def check_ingestion_export(root: Path) -> None:
    import hashlib
    import io
    import json
    import zipfile
    from muteki.src.ingestion import SrcIngestionService
    from muteki.src.query import SrcQueryService

    platform = PlatformStore(root / "ingestion-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src, "ingestion-fixture")
    ingestion = SrcIngestionService(src)
    content = (
        b'{"type":"domain","name":"Example.TEST","title":"one","uuid":"valid-1"}\n'
        b'{not json}\n'
        b'{"type":"ip","address":"not-an-ip","uuid":"invalid-2"}\n'
        b'{"type":"domain","name":"example.test","title":"two","uuid":"valid-3"}\n'
    )
    batch = ingestion.ingest(task.task_id, "jsonl", content, media_type="application/jsonl", source_id="mixed", idempotency_key="mixed-import")
    assert batch["status"] == "partial" and batch["counts"]["processed"] == 4 and batch["counts"]["failed"] == 2
    assert {item["index"] for item in batch["items"] if not item["ok"]} == {1, 2}
    repeated = ingestion.ingest(task.task_id, "jsonl", content, media_type="application/jsonl", source_id="mixed", idempotency_key="mixed-import")
    assert repeated["batch_id"] == batch["batch_id"]
    original = src.artifacts.read(ArtifactRef.model_validate(batch["original_artifact"]), expected_task_id=task.task_id)
    assert original == content
    observations_before = len(src.store.all(task.task_id, "asset_observation"))
    retried = ingestion.retry_failed(task.task_id, batch["batch_id"])
    assert retried["status"] == "partial" and len(src.store.all(task.task_id, "asset_observation")) == observations_before

    raw = b"GET /items?a=1&a=2 HTTP/1.1\r\nHost: example.test\r\nX-Test: one\r\nX-Test: two\r\n\r\n"
    http_batch = ingestion.ingest(
        task.task_id, "burp", raw, media_type="message/http", source_id="burp",
        source_metadata={"scheme": "http", "captured_at": "2026-01-01T00:00:00Z"}, idempotency_key="raw-import",
    )
    assert http_batch["status"] == "completed" and http_batch["items"][0]["live_replay"] is False
    exchange = src.store.require(task.task_id, "import_exchange", http_batch["items"][0]["exchange_id"])
    assert exchange.payload["capture_state"] == "partial" and exchange.payload["request_raw_ref"]["sha256"] == hashlib.sha256(raw).hexdigest()

    unknown = ingestion.ingest(
        task.task_id, "jsonl", b'{"type":"domain","name":"cost.example"}\n',
        media_type="application/jsonl", source_id="cost", idempotency_key="cost-import",
        cost={"state": "unknown"},
    )
    blocked = ingestion.retry_failed(task.task_id, unknown["batch_id"])
    assert blocked["status"] == "external_unknown" and blocked["cost"]["state"] == "unknown"
    reconciled = ingestion.reconcile_external(task.task_id, unknown["batch_id"], {"receipt": "upstream-cost", "cost": {"actual": 0.5, "state": "confirmed"}})
    assert reconciled["status"] == "reconciled" and reconciled["cost"]["actual"] == 0.5

    query = SrcQueryService(src)
    first_page = query.query(task.task_id, "type=domain", limit=1)
    assert first_page["returned"] == 1 and first_page["next_cursor"]
    data, media_type, manifest = query.export(task.task_id, "type=domain", export_format="zip")
    assert media_type == "application/zip" and not manifest["partial"]
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        document = json.loads(archive.read("assets.json"))
        exported_ids = {row["asset_id"] for row in document["assets"]}
        expected_ids = {row["asset_id"] for row in query.query(task.task_id, "type=domain", limit=100)["items"]}
        assert exported_ids == expected_ids and len(exported_ids) > first_page["returned"]
        for entry in manifest["attachments"]:
            content_bytes = archive.read(entry["path"])
            assert len(content_bytes) == entry["bytes"]
            assert hashlib.sha256(content_bytes).hexdigest() == entry["sha256"]
    csv_data, csv_media, _manifest = query.export(task.task_id, "type=domain", export_format="csv")
    assert csv_media.startswith("text/csv") and b"does not restore the graph" not in csv_data
    platform.close()
    print("PASS: mixed ingestion/idempotent retry, raw import, unknown cost and complete export")


async def check_monitoring(root: Path) -> None:
    from datetime import datetime, timedelta, timezone
    from muteki.src.monitoring import SrcMonitoringService

    platform = PlatformStore(root / "monitor-platform.db")
    src = make_src(root, platform)
    _thread, task = make_task(src, "monitor-fixture")
    now = datetime(2026, 1, 1, tzinfo=timezone.utc)
    clock = lambda: now
    delivered = []
    service = SrcMonitoringService(src, clock=clock, notify=lambda row: delivered.append(row) or asyncio.sleep(0))
    monitor = service.create_monitor(task.task_id, {
        "continuous": True, "chain_type": "asset", "query_ref": "query-v1",
        "target_selection": {"asset_ids": ["asset-a"]},
        "source": {"connector_id": "fixture", "capability_manifest_ref": "manifest-v1"},
        "authorization_ref": "policy-v1", "schedule": {"interval_seconds": 1, "timezone": "UTC", "cycle_timeout_seconds": 2},
        "budget": {"total": 5, "per_cycle": 1, "call_limit": 10, "currency": "USD"},
        "notification": {"enabled": True, "merge_window_seconds": 60, "notify_on_completion": False},
        "baseline_policy": {"denoise": {"version": "v1", "ignore_keys": ["nonce"]}},
    }, actor_id="local-user")
    monitor_id = monitor["monitor_id"]

    async def run(snapshot, *, complete=True, errors=None, calls=1):
        nonlocal now
        now = now + timedelta(seconds=1)
        cycle = service._new_cycle(task.task_id, monitor_id)
        return await service.run_cycle(
            task.task_id, monitor_id, cycle["cycle_id"],
            runner=lambda _context: asyncio.sleep(0, result={
                "raw_snapshot": snapshot, "completeness": "complete" if complete else "partial",
                "complete": complete, "errors": errors or [], "calls": calls,
                "observations": [{"key": "asset-a", "attributes": snapshot.get("asset-a", {}), "success": complete}],
                "cost": {"actual": 0.1, "unknown": False}, "source_watermarks": {"fixture": calls},
                "revalidation": {"source_available": True, "permission_valid": True, "plan_valid": True, "authorization_valid": True},
            }),
        )

    first = await run({"asset-a": {"title": "same", "nonce": 1}})
    assert first["status"] == "completed" and first["baseline_id"] and not first["compared_baseline_id"], first
    first_baseline = first["baseline_id"]
    second = await run({"asset-a": {"title": "same", "nonce": 2}})
    assert second["status"] == "completed"
    assert all(not row["meaningful"] for row in service.list_changes(task.task_id, monitor_id))
    assert service.list_notifications(task.task_id, monitor_id) == []
    third = await run({"asset-a": {"title": "same", "nonce": 2}}, complete=False, errors=[{"code": "missing_page"}])
    assert third["status"] == "partial" and not third["baseline_id"]
    partial_failures = service.list_notifications(task.task_id, monitor_id)
    assert len(partial_failures) == 1 and partial_failures[0]["kind"] == "cycle_failed"
    fourth = await run({"asset-a": {"title": "changed", "nonce": 3}}, calls=2)
    assert fourth["status"] == "completed" and fourth["compared_baseline_id"] == second["baseline_id"]
    assert fourth["baseline_id"] != second["baseline_id"]
    comparison = fourth["comparison"]
    assert comparison["reason"] == "compared_last_valid_baseline"
    assert {row["cycle_id"] for row in comparison["missing_cycles"]} == {third["cycle_id"]}
    changes = [row for row in service.list_changes(task.task_id, monitor_id) if row["meaningful"]]
    assert len(changes) == 1 and changes[0]["kind"] == "changed"
    assert changes[0]["raw_diff"] and changes[0]["denoise"]
    candidates = [row for row in src.store.all(task.task_id, "monitor_candidate")]
    assert candidates and candidates[0].payload["projection_kind"] == "observation"
    pending = service.list_notifications(task.task_id, monitor_id)
    assert any(row["kind"] == "meaningful_change" for row in pending)
    sent = await service.deliver_notifications(task.task_id)
    assert sent and len(delivered) == len(sent)
    again = await service.deliver_notifications(task.task_id)
    assert again == [] and len(delivered) == len(sent)

    current = service.get_monitor(task.task_id, monitor_id)
    paused = service.set_state(task.task_id, monitor_id, "paused", expected_revision=current["revision"])
    assert paused["status"] == "paused" and service.due_monitors(task.task_id) == []
    resumed = service.set_state(task.task_id, monitor_id, "active", expected_revision=paused["revision"])
    assert resumed["status"] == "active"

    now = now + timedelta(seconds=1)
    pending_cycle = service._new_cycle(task.task_id, monitor_id)
    cycle_record = src.store.require(task.task_id, "monitor_cycle", pending_cycle["cycle_id"])
    src.store.put(task.task_id, "monitor_cycle", {
        **cycle_record.payload, "status": "running", "dispatch_state": "dispatched",
    }, record_id=pending_cycle["cycle_id"], expected_revision=cycle_record.revision, parent_id=monitor_id)
    recovered = service.recover(task.task_id)
    assert pending_cycle["cycle_id"] in recovered["blocked_external_unknown"]
    reconciled = service.reconcile_external(task.task_id, pending_cycle["cycle_id"], {"receipt": "upstream-1", "cost": {"actual": 0.25, "unknown": False}})
    assert reconciled["cost"]["actual"] == 0.25 and reconciled["external_receipt"]["receipt"] == "upstream-1"
    repeated_cycle = await service.run_cycle(task.task_id, monitor_id, fourth["cycle_id"])
    assert repeated_cycle["cycle_id"] == fourth["cycle_id"]
    platform.close()
    print("PASS: monitor baseline, incomplete cycle, diff/notice dedup, pause and external recovery")


async def check() -> None:
    failures: list[str] = []
    for scenario in (check_assets, check_ingestion_export, check_monitoring):
        with tempfile.TemporaryDirectory(prefix="muteki-src-assets-") as temp:
            try:
                await scenario(Path(temp))
            except Exception as exc:
                failures.append(f"{scenario.__name__}: {type(exc).__name__}: {exc}")
                print(f"FAIL: {failures[-1]}")
    if failures:
        raise RuntimeError("SRC asset/monitor checks failed: " + " | ".join(failures))


if __name__ == "__main__":
    asyncio.run(check())
