"""Blocking BTW snapshot/cleanup operations; callers offload them from the loop."""
from pathlib import Path
import re
import shutil
import sqlite3
import time


def snapshot_context(workdir: Path, jsonl: Path, graph: Path) -> tuple[Path, Path]:
    context = workdir / 'context'
    context.mkdir(parents=True, exist_ok=True)
    events, database = context / 'events.jsonl', context / 'shared_graph.db'
    if jsonl.is_file():
        # Bound copying to the size observed at open: a live append-only log
        # must not keep a snapshot operation running indefinitely.
        with jsonl.open('rb') as source, events.open('wb') as target:
            remaining = source.seek(0, 2)
            source.seek(0)
            while remaining:
                chunk = source.read(min(remaining, 1024 * 1024))
                if not chunk:
                    break
                target.write(chunk)
                remaining -= len(chunk)
    if graph.is_file():
        source = sqlite3.connect(graph.resolve().as_uri() + '?mode=ro', uri=True, timeout=2)
        try:
            target = sqlite3.connect(database)
            try:
                deadline = time.monotonic() + 5
                def progress(status, remaining, total):
                    if time.monotonic() > deadline:
                        raise TimeoutError('BTW graph snapshot timed out')
                source.backup(target, pages=256, progress=progress, sleep=0.05)
            finally:
                target.close()
        finally:
            source.close()
    return events, database


def cleanup_turn(root: Path, workdir: Path, label: str) -> None:
    # Only delete this request's UUID paths, never scan shared/main worker dirs.
    if not re.fullmatch(r'btw-[a-z0-9_-]+-[a-f0-9]{32}', label):
        raise ValueError('invalid BTW cleanup identity')
    if workdir.name != label.removeprefix('btw-'):
        raise ValueError('BTW cleanup identity mismatch')
    for path, parent in ((workdir, root / 'workers' / '_btw'),
                         (root / '.muteki-agent-state' / label, root / '.muteki-agent-state'),
                         (root / 'homes' / label, root / 'homes')):
        if path.is_symlink() or path.parent.resolve() != parent.resolve():
            raise ValueError('unsafe BTW cleanup path')
        # Parent paths must still be inside the run workspace.
        parent.resolve().relative_to(root.resolve())
        if path.exists():
            shutil.rmtree(path)
