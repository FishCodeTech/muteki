"""Durable usage observations. Tokens are consumption, never context occupancy.

One row is one measurable invocation/message, revised by later observations.
Only numeric usage metadata is retained; prompts and credentials are excluded.
"""
from __future__ import annotations

from contextlib import contextmanager, nullcontext
from contextvars import ContextVar
import json
import math
from pathlib import Path
import sqlite3
import time
from typing import Any
from uuid import uuid4

FIELDS = ('input_tokens', 'output_tokens', 'cache_read_tokens', 'cache_write_tokens', 'reasoning_tokens')
DIMS = ('run_id', 'thread_id', 'turn_id', 'competition_id', 'challenge_id', 'worker_id', 'role', 'actor_kind', 'model', 'engine', 'generation', 'workspace_kind')
request_identity: ContextVar[str | None] = ContextVar('usage_request_identity', default=None)
_context: ContextVar[tuple | None] = ContextVar('usage_context', default=None)


def number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value) if math.isfinite(value) and 0 <= value <= 2**53 - 1 else None


def normalize(raw: dict) -> dict:
    def pick(*keys):
        for key in keys:
            value = number(raw.get(key))
            if value is not None:
                return value
        return None
    inp = pick('input_tokens', 'prompt_tokens', 'inputTokens', 'input')
    out = pick('output_tokens', 'completion_tokens', 'outputTokens', 'output')
    read = pick('cache_read_tokens', 'cached_input_tokens', 'cache_read_input_tokens', 'cacheRead', 'cacheReadTokens')
    write = pick('cache_write_tokens', 'cache_creation_input_tokens', 'cacheWrite', 'cacheWriteTokens')
    reasoning = pick('reasoning_tokens', 'reasoning_output_tokens', 'reasoningOutputTokens')
    details = raw.get('prompt_tokens_details') or {}
    if read is None and isinstance(details, dict):
        read = number(details.get('cached_tokens'))
    details = raw.get('completion_tokens_details') or {}
    if reasoning is None and isinstance(details, dict):
        reasoning = number(details.get('reasoning_tokens'))
    # Native Anthropic/Pi/Cursor use disjoint input buckets. Normalized CLI
    # results explicitly say input includes cache. OpenAI/Codex already include it.
    separate = any(k in raw for k in ('cache_read_input_tokens', 'cacheRead', 'cacheReadTokens', 'cache_creation_input_tokens'))
    if separate and not raw.get('input_includes_cache') and inp is not None:
        inp += (read or 0) + (write or 0)
    result = dict(zip(FIELDS, (inp, out, read, write, reasoning)))
    result['quality'] = 'estimated' if raw.get('estimated') else ('reported' if inp is not None and out is not None else 'partial' if inp is not None or out is not None else 'missing')
    result['source'] = str(raw.get('source') or 'runtime')
    return result


def _cumulative_delta(current: dict, previous: dict) -> dict:
    """Turn two cumulative observations into one consumption interval.

    Providers may restart their counters when a resumed session is attached to a
    new local runtime.  A smaller value therefore starts a new counter epoch for
    that field and the current value is the consumption since zero.
    """
    delta = {"source": "runtime-cumulative", "measurement_scope": "interval", "input_includes_cache": True}
    rebased = False
    for key in FIELDS:
        value = current.get(key)
        baseline = previous.get(key)
        if value is None:
            continue
        if baseline is not None and value < baseline:
            delta[key] = value
            rebased = True
        else:
            delta[key] = value - (baseline or 0)
    if rebased:
        delta["status"] = "counter_reset_rebased"
    return delta



def _token_coverage(items, inp, out) -> str:
    """Classify input/output consumption coverage (not context-window occupancy).

    - missing: no observed input or output tokens
    - partial: only one side observed, or some records lack full reported IO
    - complete: every record reported/estimated both input and output
    """
    if not items:
        return 'missing'
    if inp is None and out is None:
        return 'missing'
    if inp is None or out is None:
        return 'partial'
    if any(row.get('quality') in ('missing', 'partial') for row in items):
        return 'partial'
    return 'complete'


class UsageStore:
    def __init__(self, root: str | Path):
        self.path = Path(root) / 'usage.sqlite3'
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.connect() as db:
            db.execute('PRAGMA journal_mode=WAL')
            db.execute('CREATE TABLE IF NOT EXISTS usage (id TEXT PRIMARY KEY, seq INTEGER NOT NULL, at REAL NOT NULL, updated REAL NOT NULL, data TEXT NOT NULL)')
            db.execute('CREATE INDEX IF NOT EXISTS usage_at ON usage(at)')
            for key in ('run_id', 'thread_id', 'model'):
                db.execute(f"CREATE INDEX IF NOT EXISTS usage_{key} ON usage(json_extract(data, '$.{key}'), at)")
            db.execute('CREATE TABLE IF NOT EXISTS usage_samples (series TEXT, seq INTEGER, identity TEXT, at REAL, data TEXT, context TEXT, PRIMARY KEY(series,seq))')
            db.execute('CREATE TABLE IF NOT EXISTS usage_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL)')
            db.execute("INSERT OR IGNORE INTO usage_meta VALUES ('revision',0)")
            db.execute("INSERT OR IGNORE INTO usage_meta VALUES ('coverage_start',?)", (int(time.time()*1000000),))
            db.execute("CREATE TABLE IF NOT EXISTS run_ownership (run_id TEXT PRIMARY KEY, competition_id TEXT NOT NULL, challenge_id TEXT NOT NULL DEFAULT '')")
            ownership_columns = {
                row[1] for row in db.execute('PRAGMA table_info(run_ownership)')
            }
            if 'challenge_id' not in ownership_columns:
                db.execute(
                    "ALTER TABLE run_ownership ADD COLUMN challenge_id TEXT NOT NULL DEFAULT ''"
                )
        self._repair_unknown_counter_resets()
        self._repair_worker_attribution()

    def _repair_unknown_counter_resets(self):
        """One-time repair for reset intervals written by the older policy."""
        with self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            if db.execute("SELECT 1 FROM usage_meta WHERE key='reset_rebase_v1'").fetchone():
                return
            rows = db.execute("""
                SELECT s.series,s.seq,s.identity,s.at,s.data,s.context
                FROM usage_samples s JOIN usage u ON u.id=s.identity
                WHERE json_extract(u.data, '$.status')='counter_reset_unknown'
            """).fetchall()
            for series, seq, identity, at, encoded, scope in rows:
                predecessor = db.execute(
                    'SELECT data FROM usage_samples WHERE series=? AND seq<? ORDER BY seq DESC LIMIT 1',
                    (series, seq),
                ).fetchone()
                delta = _cumulative_delta(json.loads(encoded), json.loads(predecessor[0]) if predecessor else {})
                self.record(delta, identity=identity, seq=seq, at=at, _db=db, **json.loads(scope))
            db.execute("INSERT INTO usage_meta VALUES ('reset_rebase_v1',1)")

    def _repair_worker_attribution(self):
        """Backfill configured CLI engine/model from durable worker events.

        Older settlement code looked for a nonexistent ``solver.profile`` field,
        which left every Worker row without an engine and sometimes priced/modelled
        it as the DeepSeek fallback.  ``worker.status`` is the durable source for
        the concrete profile selected for that worker generation.
        """
        with self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            if db.execute(
                "SELECT 1 FROM usage_meta WHERE key='worker_attribution_v1'"
            ).fetchone():
                return
            rows = db.execute("""
                SELECT id,data FROM usage
                WHERE json_extract(data, '$.actor_kind')='worker'
                  AND json_extract(data, '$.run_id') IS NOT NULL
                  AND json_extract(data, '$.worker_id') IS NOT NULL
            """).fetchall()
            by_run: dict[str, dict[tuple[str, str], dict[str, str]]] = {}
            changed = 0
            for identity, encoded in rows:
                data = json.loads(encoded)
                run_id = str(data.get('run_id') or '')
                worker_id = str(data.get('worker_id') or '')
                generation = str(data.get('generation'))
                if not run_id or not worker_id or Path(run_id).name != run_id:
                    continue
                if run_id not in by_run:
                    statuses: dict[tuple[str, str], dict[str, str]] = {}
                    try:
                        with (self.path.parent / f'{run_id}.jsonl').open() as stream:
                            for line in stream:
                                event = json.loads(line)
                                payload = event.get('payload') or {}
                                solver_id = str(event.get('solver_id') or '')
                                if (event.get('event_type') != 'worker.status'
                                        or not solver_id
                                        or not isinstance(payload, dict)):
                                    continue
                                statuses[(
                                    solver_id,
                                    str(payload.get('execution_generation')),
                                )] = {
                                    key: str(payload.get(key) or '').strip()
                                    for key in ('engine', 'model')
                                }
                    except (OSError, ValueError, TypeError):
                        pass
                    by_run[run_id] = statuses
                expected = by_run[run_id].get((worker_id, generation))
                if not expected:
                    continue
                revised = dict(data)
                for key in ('engine', 'model'):
                    if expected.get(key):
                        revised[key] = expected[key]
                if revised == data:
                    continue
                db.execute(
                    'UPDATE usage SET updated=?,data=? WHERE id=?',
                    (time.time(), json.dumps(revised, ensure_ascii=False), identity),
                )
                changed += 1
            if changed:
                db.execute(
                    "UPDATE usage_meta SET value=value+? WHERE key='revision'",
                    (changed,),
                )
            db.execute(
                "INSERT INTO usage_meta VALUES ('worker_attribution_v1',1)"
            )

    def connect(self):
        return sqlite3.connect(self.path, timeout=15)

    def bind_run(self, run_id: str, competition_id: str, challenge_id: str = ''):
        return self.bind_runs(((run_id, competition_id, challenge_id),))

    def bind_runs(self, ownerships):
        """Persist competition/challenge ownership in one transaction."""
        changed = 0
        with self.connect() as db:
            for run_id, competition_id, challenge_id in ownerships:
                existing = db.execute(
                    'SELECT competition_id,challenge_id FROM run_ownership WHERE run_id=?',
                    (run_id,),
                ).fetchone()
                if existing and existing[0] != competition_id:
                    raise ValueError('usage run ownership conflict')
                if existing and challenge_id and existing[1] and existing[1] != challenge_id:
                    raise ValueError('usage run challenge ownership conflict')
                if existing:
                    if challenge_id and not existing[1]:
                        db.execute(
                            'UPDATE run_ownership SET challenge_id=? WHERE run_id=?',
                            (challenge_id, run_id),
                        )
                        changed += 1
                else:
                    db.execute(
                        'INSERT INTO run_ownership (run_id,competition_id,challenge_id) VALUES (?,?,?)',
                        (run_id, competition_id, challenge_id),
                    )
                    changed += 1
            if changed:
                db.execute(
                    "UPDATE usage_meta SET value=value+? WHERE key='revision'",
                    (changed,),
                )
        return changed

    def record(self, raw: dict, *, identity: str | None = None, seq: int = 0, at: float | None = None, _db=None, **context):
        identity = identity or uuid4().hex
        data = {k: context[k] for k in DIMS if context.get(k) is not None}
        data.update(normalize(raw))
        for key in ('reported_cost', 'estimated_cost'):
            value = raw.get(key)
            if isinstance(value, (float, int)) and not isinstance(value, bool) and math.isfinite(value) and value >= 0:
                data[key] = value
        data['status'] = str(raw.get('status') or 'observed')
        data['currency'] = 'USD'
        data['measurement_scope'] = str(raw.get('measurement_scope') or 'invocation')
        now = time.time()
        with (nullcontext(_db) if _db is not None else self.connect()) as db:
            if _db is None:
                db.execute('BEGIN IMMEDIATE')
            old = db.execute('SELECT seq,at,data FROM usage WHERE id=?', (identity,)).fetchone()
            if old and data['quality'] == 'missing':
                previous = json.loads(old[2])
                if previous['quality'] != 'missing':
                    data = {**previous, 'status': data['status']}
            if old and (seq < old[0] or json.loads(old[2]) == data):
                return False
            db.execute('INSERT INTO usage VALUES (?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET seq=excluded.seq,updated=excluded.updated,data=excluded.data',
                       (identity, seq, old[1] if old else (at if at is not None else now), now, json.dumps(data, ensure_ascii=False)))
            db.execute("UPDATE usage_meta SET value=value+1 WHERE key='revision'")
        return True

    def import_run_history(self):
        """Import only pre-ledger snapshots. Re-running cannot overlap live records."""
        from datetime import datetime
        with self.connect() as db:
            cutoff = db.execute("SELECT value FROM usage_meta WHERE key='coverage_start'").fetchone()[0] / 1000000
        imported = 0
        skipped = 0
        for path in self.path.parent.glob('*.jsonl'):
            latest = {}
            generation = 0
            try:
                with path.open() as stream:
                    for line in stream:
                        try:
                            event = json.loads(line)
                            stamp = event.get('ts', 0)
                            if isinstance(stamp, str):
                                stamp = datetime.fromisoformat(stamp.replace('Z', '+00:00')).timestamp()
                            if not isinstance(stamp, (int,float)) or stamp >= cutoff:
                                continue
                            payload = event.get('payload') or {}
                            generation = payload.get('execution_generation', generation)
                            if event.get('event_type') != 'cost.update':
                                continue
                            sid = event.get('solver_id') or payload.get('solver_id')
                            scope = payload.get('scope')
                            key = (event.get('run_id'), generation, scope, sid or payload.get('challenge_id') or '')
                            latest[key] = (stamp, payload)
                        except (ValueError, TypeError, AttributeError):
                            skipped += 1
            except OSError:
                skipped += 1
                continue
            solver_runs = {(key[0],key[1]) for key in latest if key[2] == 'solver'}
            global_runs = {(key[0],key[1]) for key in latest if key[2] == 'global'}
            for (run, gen, scope, actor), (stamp, payload) in latest.items():
                if not run or ((run,gen) in solver_runs and scope != 'solver'):
                    continue
                if (run,gen) not in solver_runs and (run,gen) in global_runs and scope != 'global':
                    continue
                auxiliary = scope != 'solver' or actor in ('reason','titler','coordinator','report-value','reason-cognitive-shadow','reason-compact','compactor','summarizer')
                imported += bool(self.record({
                    'input_tokens': payload.get('input_tokens'), 'output_tokens': payload.get('output_tokens'),
                    'estimated_cost': payload.get('usd'), 'estimated': True,
                    'source': 'legacy-snapshot', 'status': 'historical_quality_unknown',
                    'measurement_scope': 'solver-snapshot',
                }, identity=f'legacy:{run}:{gen}:{scope}:{actor}', at=stamp, run_id=run,
                    generation=gen, workspace_kind='single-security-task', role=actor if auxiliary else 'worker',
                    worker_id=actor, actor_kind='auxiliary' if auxiliary else 'worker'))
        return {'imported': imported, 'skipped': skipped, 'coverage_start': cutoff}

    def cumulative(self, raw: dict, *, series: str, seq: int, at: float, **context):
        """Persist cumulative source checkpoints; materialize each interval once.

        A late checkpoint revises its successor interval, so replay order cannot
        change consumption. A decreasing counter is explicitly incomplete.
        """
        sample = normalize(raw)
        with self.connect() as db:
            db.execute('BEGIN IMMEDIATE')
            db.execute('INSERT OR REPLACE INTO usage_samples VALUES (?,?,?,?,?,?)',
                       (series, seq, f"{series}:{seq}", at, json.dumps(sample), json.dumps(context)))
            predecessor = db.execute('SELECT data FROM usage_samples WHERE series=? AND seq<? ORDER BY seq DESC LIMIT 1', (series, seq)).fetchone()
            rows = db.execute('SELECT seq,identity,at,data,context FROM usage_samples WHERE series=? AND seq>=? ORDER BY seq LIMIT 2', (series, seq)).fetchall()
            previous = json.loads(predecessor[0]) if predecessor else {}
            for index, uid, stamp, encoded, scope in rows:
                current = json.loads(encoded)
                delta = _cumulative_delta(current, previous)
                self.record(delta, identity=uid, seq=index, at=stamp, _db=db, **json.loads(scope))
                previous = current

    def query(self, *, start: float = 0, end: float | None = None, offset: int = 0, limit: int = 100, **filters):
        with self.connect() as db:
            db.execute('BEGIN')
            revision = db.execute("SELECT value FROM usage_meta WHERE key='revision'").fetchone()[0]
            owners = {
                run_id: (competition_id, challenge_id)
                for run_id, competition_id, challenge_id in db.execute(
                    'SELECT run_id,competition_id,challenge_id FROM run_ownership'
                )
            }
            clauses = ['at>=?', 'at<=?']
            values = [start, end if end is not None else time.time()]
            for key in ('run_id', 'thread_id', 'model', 'role', 'actor_kind', 'generation'):
                if filters.get(key) is not None and filters[key] != '':
                    clauses.append(f"json_extract(data, '$.{key}') = ?")
                    values.append(filters[key])
            rows = db.execute('SELECT id,at,data FROM usage WHERE ' + ' AND '.join(clauses) + ' ORDER BY at DESC,id DESC', values).fetchall()
        records = []
        for uid, at, encoded in rows:
            row = json.loads(encoded)
            row.update(id=uid, at=at)
            if row.get('run_id') in owners:
                competition_id, challenge_id = owners[row['run_id']]
                row['competition_id'] = competition_id
                if challenge_id:
                    row['challenge_id'] = challenge_id
                row['workspace_kind'] = 'competition'
            if any(str(row.get(k, '')) != str(v) for k, v in filters.items() if k in DIMS and v is not None and v != ''):
                continue
            records.append(row)
        def aggregate(items):
            # Sum only observed fields. Missing stays None so callers can tell
            # "never reported" from an explicit zero consumption.
            observed = {}
            result = {}
            for key in FIELDS:
                values = [row[key] for row in items if row.get(key) is not None]
                observed[key] = len(values)
                result[key] = sum(values) if values else None
            inp, out = result['input_tokens'], result['output_tokens']
            result['total_tokens'] = (inp + out) if inp is not None and out is not None else None
            result['records'] = len(items)
            result['quality'] = {
                q: sum(row.get('quality') == q for row in items)
                for q in ('reported', 'estimated', 'partial', 'missing')
            }
            result['observed'] = observed
            result['token_coverage'] = _token_coverage(items, inp, out)
            for key in ('reported_cost', 'estimated_cost'):
                values = [r[key] for r in items if key in r]
                result[key] = sum(values) if values else None
            result['unpriced'] = sum('reported_cost' not in r and 'estimated_cost' not in r for r in items)
            return result
        groups = {}
        for dim in ('challenge_id', 'role', 'model', 'engine', 'workspace_kind', 'worker_id'):
            buckets = {}
            for row in records:
                buckets.setdefault(str(row.get(dim) or '未上报'), []).append(row)
            groups[dim] = [dict(name=k, **aggregate(v)) for k,v in buckets.items()]
        buckets = {}
        for row in records:
            buckets.setdefault(int(row['at'] // 3600) * 3600, []).append(row)
        return dict(revision=revision, as_of=time.time(), scope=filters, totals=aggregate(records), groups=groups,
                    series=[dict(at=k, **aggregate(v)) for k,v in sorted(buckets.items())],
                    records=records[offset:offset+limit], count=len(records), offset=offset, limit=limit)


@contextmanager
def usage_context(root: str | Path, **scope):
    token = _context.set((UsageStore(root), scope))
    try:
        yield
    finally:
        _context.reset(token)


def record_context(raw: dict, **scope):
    current = _context.get()
    if current:
        store, context = current
        store.record(raw, identity=request_identity.get(), **{**context, **scope})


def cli_usage(result: Any) -> dict:
    return dict(input_tokens=result.input_tokens, output_tokens=result.output_tokens,
                cache_read_tokens=getattr(result, 'cache_read_tokens', None),
                cache_write_tokens=getattr(result, 'cache_write_tokens', None),
                reasoning_tokens=getattr(result, 'reasoning_tokens', None),
                estimated=getattr(result, 'usage_estimated', False),
                input_includes_cache=True, source='cli',
                **({'estimated_cost': result.cost_usd} if getattr(result, 'cost_estimated', False) else {'reported_cost': result.cost_usd}))
