"""muteki_kit's view of Result + the artifact store, re-exported from the core.

Solver-generated code does `from muteki_kit.result import Result` and ends with
`_RESULT = Result.output(...)`. Keeping a single Result type across the kit and
the kernel means no serialization mismatch.
"""

import os

from muteki.solver.result import ArtifactStore, PeekResult, Result, peek

# A process-global artifact store the in-kernel tools write to. The host (Solver)
# and the kernel subprocess MUST share one absolute dir so an artifact saved
# in-kernel is peekable from the host. The Solver sets MUTEKI_ARTIFACTS_DIR and
# propagates it into the kernel env; absent that we fall back to "artifacts/".
ARTIFACTS = ArtifactStore(root=os.environ.get("MUTEKI_ARTIFACTS_DIR", "artifacts"))


def save_artifact(content, suffix: str = ".txt") -> str:
    """Persist raw output, return artifact_id (peekable later)."""
    return ARTIFACTS.put(content, suffix=suffix)


__all__ = ["Result", "PeekResult", "ArtifactStore", "peek", "ARTIFACTS", "save_artifact"]
