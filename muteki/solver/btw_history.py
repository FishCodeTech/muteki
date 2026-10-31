"""Coordinator-owned BTW history, kept with (and purged with) its run."""
from __future__ import annotations

import os
from contextlib import contextmanager
from pathlib import Path
import sqlite3
import time
import uuid


class BtwHistory:
    def __init__(self, directory: Path):
        self.path = Path(directory) / 'btw-history.sqlite3'

    @contextmanager
    def _connect(self):
        db = sqlite3.connect(self.path.resolve().as_uri() + '?mode=rw', uri=True, timeout=10)
        db.row_factory = sqlite3.Row
        try:
            with db:
                db.execute('PRAGMA secure_delete=ON')
                db.execute('CREATE TABLE IF NOT EXISTS turns (\n                    id TEXT PRIMARY KEY, question TEXT NOT NULL, answer TEXT NOT NULL,\n                    status TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL,\n                    engine TEXT NOT NULL, model TEXT NOT NULL)')
                yield db
        finally:
            db.close()

    def list(self) -> list[dict]:
        if not self.path.is_file():
            return []
        with self._connect() as db:
            return [dict(row) for row in db.execute('SELECT * FROM turns ORDER BY created_at, id')]

    def begin(self, question: str, *, engine: str, model: str) -> str:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        # Keep the database private, including its initial creation.
        fd = os.open(self.path, os.O_CREAT | os.O_WRONLY, 0o600)
        os.close(fd)
        turn_id = uuid.uuid4().hex
        now = time.time()
        with self._connect() as db:
            db.execute('INSERT INTO turns VALUES (?, ?, ?, ?, ?, ?, ?, ?)',
                       (turn_id, question, '', 'incomplete', now, now, engine, model))
        return turn_id

    def update(self, turn_id: str, answer: str, status: str) -> None:
        if status not in {'incomplete', 'completed', 'failed', 'cancelled'}:
            raise ValueError('invalid BTW history status')
        if not self.path.is_file():
            return
        with self._connect() as db:
            # UPDATE only: a deleted turn cannot be resurrected by a late stream.
            db.execute('UPDATE turns SET answer=?, status=?, updated_at=? WHERE id=?',
                       (answer, status, time.time(), turn_id))

    def delete(self, turn_id: str | None = None) -> int:
        if not self.path.is_file():
            return 0
        with self._connect() as db:
            cursor = db.execute('DELETE FROM turns WHERE id=?', (turn_id,)) if turn_id is not None else db.execute('DELETE FROM turns')
            return cursor.rowcount

    def transcript(self) -> list[dict[str, str]]:
        # Preserve complete retained turns, including a single very long answer.
        # Provider context limits must surface as failures, not silent data loss.
        return [turn for row in self.list() if row['status'] == 'completed' for turn in (
            {'role': 'user', 'content': row['question']},
            {'role': 'assistant', 'content': row['answer']},
        )]
