from __future__ import annotations

from typing import Any

from .artifacts import SrcArtifactStore
from .store import SrcStore, SrcStoreError

EXTRACTOR_VERSION = "src.run-projection.v1"


_EVENT_KIND_MAP = {
    "dead_end": "dead_end",
    "deadend": "dead_end",
    "observation": "observation",
    "fact": "fact",
    "step": "step",
    "intent": "step",
    "tool": "tool_call",
    "tool_call": "tool_call",
    "command": "tool_call",
    "report": "report_version",
    "report_version": "report_version",
    "error": "run_error",
    "failed": "run_error",
    "directive": "operator_directive",
    "operator": "operator_directive",
    "artifact": "artifact_event",
}


def _kind(event_type: str) -> str:
    value = event_type.strip().lower()
    if not value:
        return "run_event"
    if value in _EVENT_KIND_MAP:
        return _EVENT_KIND_MAP[value]
    segments = [seg for seg in value.replace("-", "_").split(".") if seg]
    for seg in reversed(segments):
        if seg in _EVENT_KIND_MAP:
            return _EVENT_KIND_MAP[seg]
        tokens = [tok for tok in seg.split("_") if tok]
        if "dead" in tokens and "end" in tokens:
            return "dead_end"
        for tok in tokens:
            if tok in _EVENT_KIND_MAP:
                return _EVENT_KIND_MAP[tok]
    return "run_event"


class SrcProjector:
    def __init__(self, store: SrcStore, artifacts: SrcArtifactStore) -> None:
        self.store = store
        self.artifacts = artifacts

    def project(self, task_id: str, run_id: str, event: Any) -> bool:
        data = event.model_dump(mode="json")
        seq = int(data.get("seq") or 0)
        event_type = str(data.get("event_type") or "")
        source_event_id = f"{EXTRACTOR_VERSION}:{run_id}:{seq}:{self.store.digest(data)}"
        if self.store.idempotent(task_id, "projection", source_event_id, data) is not None:
            return False
        binding = self.store.require(task_id, "run_binding", run_id)
        raw_generation = (data.get("payload") or {}).get("execution_generation")
        generation = int(binding.generation if raw_generation is None else raw_generation)
        current_generation = int(binding.payload.get("generation") or 0)
        if generation < int(binding.generation) or generation < current_generation:
            raise SrcStoreError("authorization_stale", "event belongs to an old Run generation")
        record_id = f"{run_id}:{seq}:{EXTRACTOR_VERSION}"
        existing_record = self.store.get(task_id, _kind(event_type), record_id)
        if existing_record is not None and existing_record.payload.get("source") != data:
            raise SrcStoreError(
                "version_conflict",
                "projected run event sequence already exists with different content",
                detail={"run_id": run_id, "seq": seq},
            )
        with self.store.platform.transaction():
            artifact = self.artifacts.put(
                task_id, event.model_dump_json().encode("utf-8"),
                media_type="application/json", name=f"{run_id}-{seq}.json",
                source_run_id=run_id, source_event_seq=seq,
            )
            kind = _kind(event_type)
            record = self.store.put(
                task_id, kind, {
                    "source": data, "artifact": artifact.model_dump(mode="json"),
                    "event_type": event_type, "source_run_id": run_id,
                    "source_event_seq": seq, "extractor_version": EXTRACTOR_VERSION,
                    "observed_at": data.get("occurred_at"),
                },
                record_id=f"{run_id}:{seq}:{EXTRACTOR_VERSION}",
                parent_id=run_id, natural_key=f"{run_id}:{seq}:{kind}",
                generation=generation,
            )
            if generation > binding.generation:
                self.store.put(
                    task_id, "run_binding", binding.payload, record_id=run_id,
                    expected_revision=binding.revision, parent_id=binding.parent_id,
                    natural_key=binding.natural_key, generation=generation,
                    created_at=binding.created_at,
                )
            self.store.advance_source(
                task_id, run_id, expected_generation=generation,
                next_seq=seq, extractor_version=EXTRACTOR_VERSION,
            )
            self.store.remember(task_id, "projection", source_event_id, data, {
                "record_id": record.record_id, "kind": kind, "artifact": artifact.model_dump(mode="json"),
            })
        return True

    async def recover_run(self, task_id: str, run_id: str, gateway: Any) -> dict[str, Any]:
        watermark = self.store.source_watermark(task_id, run_id)
        projected = 0
        try:
            async for event in gateway.events(run_id, after_seq=int(watermark.get("next_seq") or 0)):
                projected += int(self.project(task_id, run_id, event))
        except Exception as exc:
            current_watermark = self.store.source_watermark(task_id, run_id)
            self.store.advance_source(
                task_id, run_id,
                expected_generation=int(current_watermark.get("generation") or watermark.get("generation") or 0),
                next_seq=int(current_watermark.get("next_seq") or watermark.get("next_seq") or 0),
                extractor_version=EXTRACTOR_VERSION, state="failed",
                error=f"{type(exc).__name__}: {exc}",
            )
            raise
        return {"run_id": run_id, "projected": projected, "watermark": self.store.source_watermark(task_id, run_id)}


__all__ = ["SrcProjector", "EXTRACTOR_VERSION"]
