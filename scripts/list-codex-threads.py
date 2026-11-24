#!/usr/bin/env python3
"""List recorded Muteki-created Codex thread IDs for sidebar backfill (read-only)."""
import argparse
from contextlib import closing
import json
from pathlib import Path
import sqlite3
import sys


def recorded_threads(state_dir):
    database = state_dir / 'muteki.sqlite'
    with closing(sqlite3.connect(database.as_uri() + '?mode=ro', uri=True)) as connection:
        return connection.execute("""
            SELECT DISTINCT events.thread_id,
                json_extract(events.payload, '$.payload.provider_session_id') AS codex_id
            FROM chat_events AS events
            LEFT JOIN documents AS thread
                ON thread.collection = 'conversation_threads' AND thread.id = events.thread_id
            WHERE events.event_type IN ('core.session.started', 'core.session.resumed')
              AND json_extract(events.payload, '$.payload.adapter_id') = 'codex.app_server'
              AND (
                events.event_type = 'core.session.started'
                OR (
                    events.event_type = 'core.session.resumed'
                    AND json_extract(thread.value, '$.native_fork.consumed') = 1
                    AND codex_id != json_extract(thread.value, '$.native_fork.thread_id')
                )
              )
              AND typeof(codex_id) = 'text' AND codex_id != ''
            ORDER BY codex_id, events.thread_id
        """).fetchall()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--state-dir', type=Path, action='append', required=True,
                        help='Muteki data directory; repeat to combine Web/Desktop histories')
    args = parser.parse_args()
    states = sorted({path.expanduser().resolve() for path in args.state_dir})
    threads = {}
    for state in states:
        for muteki_id, codex_id in recorded_threads(state):
            entry = threads.setdefault(codex_id, {'thread_id': codex_id,
                                                  'muteki_thread_ids': set(), 'state_dirs': set()})
            entry['muteki_thread_ids'].add(muteki_id)
            entry['state_dirs'].add(str(state))
    rows = [{**entry, 'muteki_thread_ids': sorted(entry['muteki_thread_ids']),
             'state_dirs': sorted(entry['state_dirs'])}
            for _, entry in sorted(threads.items())]
    print(json.dumps({'schema_version': 1, 'threads': rows}, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (OSError, sqlite3.Error) as error:
        print(f'Cannot read Muteki Codex thread inventory: {error}', file=sys.stderr)
        sys.exit(1)
