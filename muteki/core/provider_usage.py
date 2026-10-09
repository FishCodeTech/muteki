"""Read-only provider history and usage-only pricing, independent of budgets.

Transcript parsing, rate lookup and deduplication ported from T3 Code nightly
fd1c3386c4d60f3477ab3f13c87537848de099f5 (usageTranscripts.ts,
opencodeUsageReader.ts, usagePricing.ts and UsageService.ts).

MIT License — Copyright (c) 2026 T3 Tools Inc.
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:
The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.
THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

Only numerical usage metadata is retained in the cache. Transcript content,
credentials and request text never leave their source files. Failed sources
and unknown prices remain visible, including when a stale rate cache is used.
"""
from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict
from datetime import datetime, timedelta, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import threading
import time
from typing import Any, Iterable, Mapping
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

import httpx

from muteki.core.cursor_usage import CursorAccountUsage, CursorUsageError, UsageWindow, make_window

RATE_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
NATIVE_ENGINES = frozenset(("codex", "claude", "grok", "opencode", "cursor"))
LEDGER_ENGINES = ("pi", "kimi", "omp", "devin", "droid")
TOKEN_FIELDS = ("uncached_input", "cache_read", "cache_write", "unclassified_input", "output", "reasoning")
AMOUNT_FIELDS = ("total_tokens", *TOKEN_FIELDS, "cost", "reported_cost", "estimated_cost")
COUNT_FIELDS = ("unpriced", "records", "sessions", "missing", "historical_scope_unknown")
UNPRICEABLE = frozenset(("<synthetic>", "synthetic", "opus", "sonnet", "haiku", "fable", "auto", "default", "unknown"))


def _object(value: Any) -> dict:
    return value if isinstance(value, dict) else {}


def _number(value: Any) -> float | None:
    return (float(value) if isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and value >= 0 else None)


def _tokens(value: Any) -> int:
    return int(_number(value) or 0)


def canonical_engine(value: Any) -> str:
    """Collapse transport names, retaining unknown engines as their own group."""
    name = str(value or "unknown").lower().strip().split(":", 1)[0]
    if name.startswith("cli."):
        name = name[4:]
    name = name.split(".", 1)[0]
    return {"claude-code": "claude", "claude_code": "claude", "open-code": "opencode",
            "open_code": "opencode", "cursor-agent": "cursor", "codex-cli": "codex"}.get(name, name)


def _timestamp(value: Any) -> int | None:
    if isinstance(value, str):
        try:
            return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp() * 1000)
        except (ValueError, OverflowError):
            return None
    number = _number(value)
    return None if number is None else int(number if number > 1e12 else number * 1000)


def _record(engine: str, at: int, model: str, session: str, *, inp: int = 0,
            read: int = 0, write: int = 0, out: int = 0, reasoning: int = 0,
            reported: float | None = None, key: str | None = None,
            speed: str = "standard") -> dict:
    return {"engine": engine, "at_ms": at, "model": model, "session": session,
            "uncached_input": inp, "cache_read": read, "cache_write": write,
            "unclassified_input": 0,
            "output": out, "reasoning": reasoning, "total_tokens": inp + read + write + out,
            "reported_cost": reported, "estimated_cost": None, "cost": reported,
            "key": key, "speed": speed, "missing": 0}


def parse_claude(record: dict) -> list[dict]:
    message = _object(record.get("message"))
    usage = message.get("usage")
    at = _timestamp(record.get("timestamp"))
    model = message.get("model")
    if record.get("type") != "assistant" or not isinstance(usage, dict) or at is None or not model:
        return []
    message_id, request_id = message.get("id"), record.get("requestId")
    return [_record("claude", at, str(model), str(record.get("sessionId") or ""),
                    inp=_tokens(usage.get("input_tokens")), read=_tokens(usage.get("cache_read_input_tokens")),
                    write=_tokens(usage.get("cache_creation_input_tokens")), out=_tokens(usage.get("output_tokens")),
                    reported=_number(record.get("costUSD")),
                    key=f"{message_id or ''}:{request_id or ''}" if message_id or request_id else None,
                    speed="fast" if usage.get("speed") == "fast" else "standard")]


def parse_codex(record: dict, state: dict) -> list[dict]:
    payload = _object(record.get("payload"))
    kind, event = record.get("type"), payload.get("type")
    at = _timestamp(record.get("timestamp"))
    if kind == "session_meta":
        if not state.get("saw_meta"):
            state.update(saw_meta=True, session=str(payload.get("id") or payload.get("session_id") or ""))
            spawn = _object(_object(_object(payload.get("source")).get("subagent")).get("thread_spawn"))
            if (payload.get("forked_from_id") or spawn.get("parent_thread_id")) and at is not None:
                state.update(suppress=True, anchor=at)
        return []
    if kind == "turn_context":
        if isinstance(payload.get("model"), str):
            state["model"] = payload["model"]
        return []
    if event == "thread_settings_applied":
        tier = _object(payload.get("thread_settings")).get("service_tier")
        state["speed"] = "fast" if tier in ("priority", "fast") else "ultrafast" if tier == "ultrafast" else "standard"
        return []
    if event != "token_count" or at is None or not state.get("model"):
        return []
    usage = _object(payload.get("info")).get("last_token_usage")
    if not isinstance(usage, dict):
        return []
    signature = hashlib.sha256(json.dumps(usage, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    if signature == state.get("signature"):
        return []
    state["signature"] = signature
    if state.get("suppress"):
        if at - state["anchor"] < 1000:
            state["anchor"] = at
            return []
        state["suppress"] = False
    inp, read = _tokens(usage.get("input_tokens")), _tokens(usage.get("cached_input_tokens"))
    write, out = _tokens(usage.get("cache_write_input_tokens")), _tokens(usage.get("output_tokens"))
    row = _record("codex", at, state["model"], state.get("session", ""),
                  inp=max(0, inp - read - write), read=read, write=write, out=out,
                  reasoning=min(out, _tokens(usage.get("reasoning_output_tokens"))), speed=state.get("speed", "standard"))
    return [row] if row["total_tokens"] else []


def parse_grok(record: dict) -> list[dict]:
    params = _object(record.get("params"))
    update = _object(params.get("update"))
    usage = update.get("usage")
    if update.get("sessionUpdate") != "turn_completed" or not isinstance(usage, dict):
        return []
    precise = _number(_object(params.get("_meta")).get("agentTimestampMs"))
    at = int(precise) if precise is not None else _timestamp(record.get("timestamp"))
    if at is None:
        return []
    session, prompt = str(params.get("sessionId") or ""), update.get("prompt_id")
    entries = [(model, value) for model, value in _object(usage.get("modelUsage")).items() if model and isinstance(value, dict)]
    rows = []
    for model, raw in entries or [("grok", usage)]:
        read, write = _tokens(raw.get("cachedReadTokens")), _tokens(raw.get("cacheCreationTokens"))
        out, ticks = _tokens(raw.get("outputTokens")), _number(raw.get("costUsdTicks"))
        row = _record("grok", at, model, session, inp=max(0, _tokens(raw.get("inputTokens")) - read - write),
                      read=read, write=write, out=out, reasoning=min(out, _tokens(raw.get("reasoningTokens"))),
                      reported=ticks / 10_000_000_000 if ticks is not None else None,
                      key=f"{session}:{prompt}:{model}" if prompt is not None else None)
        if row["total_tokens"]:
            rows.append(row)
    top_ticks = _number(usage.get("costUsdTicks"))
    if entries and top_ticks is not None:
        remaining = max(0, top_ticks / 10_000_000_000 - sum(row["reported_cost"] or 0 for row in rows))
        unticked = [row for row in rows if row["reported_cost"] is None]
        denominator = sum(row["total_tokens"] for row in unticked)
        for row in unticked:
            row["reported_cost"] = remaining * row["total_tokens"] / denominator
            row["cost"] = row["reported_cost"]
    return rows


def parse_opencode(message: dict, *, uid: str = "", session: str = "", at: Any = None) -> list[dict]:
    if message.get("role") not in (None, "assistant"):
        return []
    usage = _object(message.get("tokens"))
    cache = _object(usage.get("cache"))
    model_ref = _object(message.get("model"))
    model = model_ref.get("id") or model_ref.get("modelID") or message.get("modelID")
    stamp = _number(_object(message.get("time")).get("created", at))
    if not model or stamp is None:
        return []
    reasoning, cost = _tokens(usage.get("reasoning")), _number(message.get("cost"))
    uid = uid or str(message.get("id") or "")
    row = _record("opencode", int(stamp), str(model), session or str(message.get("sessionID") or ""),
                  inp=_tokens(usage.get("input")), read=_tokens(cache.get("read")), write=_tokens(cache.get("write")),
                  out=_tokens(usage.get("output")) + reasoning, reasoning=reasoning,
                  reported=cost if cost and cost > 0 else None, key=uid or None)
    return [row] if row["total_tokens"] else []


def ledger_to_records(rows: Iterable[dict]) -> list[dict]:
    """Adapt canonical ledger IO (cache in input, reasoning in output)."""
    result = []
    for row in rows:
        at = _timestamp(row.get("at"))
        if at is None:
            continue
        inp, out = _number(row.get("input_tokens")), _number(row.get("output_tokens"))
        read, write = _number(row.get("cache_read_tokens")), _number(row.get("cache_write_tokens"))
        reasoning = _number(row.get("reasoning_tokens"))
        engine = canonical_engine(row.get("engine"))
        # Old CLI estimates and legacy snapshots are not trustworthy prices.
        legacy = (row.get("cost_status") in ("legacy_pi_estimate_unverified", "legacy_estimate_unverified")
                  or row.get("source") == "legacy-snapshot"
                  or (engine in ("pi", "omp") and row.get("source") == "cli"
                      and row.get("price_source") not in ("model_price", "driver_estimate")))
        reported = None if legacy else _number(row.get("reported_cost"))
        if engine == "opencode" and reported == 0:
            reported = None
        entry = _record(engine, at, str(row.get("model") or "unknown"),
                        str(row.get("thread_id") or row.get("run_id") or ""), reported=reported,
                        key=str(row["id"]) if row.get("id") else None)
        entry.update(uncached_input=max(0, int(inp) - int(read) - int(write))
                     if inp is not None and read is not None and write is not None else None,
                     unclassified_input=(0 if read is not None and write is not None
                                         else max(0, int(inp) - int(read or 0) - int(write or 0)))
                     if inp is not None else None,
                     cache_read=int(read) if read is not None else None,
                     cache_write=int(write) if write is not None else None,
                     output=int(out) if out is not None else None,
                     reasoning=min(int(out), int(reasoning)) if out is not None and reasoning is not None else None,
                     total_tokens=int(inp + out) if inp is not None and out is not None else None,
                     missing=int(inp is None or out is None), input_tokens=int(inp) if inp is not None else None,
                     scope="muteki")
        # The pricing path must know if the cache breakdown was actually given.
        entry["cache_breakdown_known"] = read is not None and write is not None
        entry["historical_scope_unknown"] = int(
            row.get("measurement_scope") == "historical_scope_unknown"
            or row.get("normalization_repair") == "droid_historical_scope_unknown"
        )
        entry["estimated_cost"] = (_number(row.get("estimated_cost")) if not legacy
                                   and row.get("price_source") in ("model_price", "driver_estimate") else None)
        result.append(entry)
    return result


def parse_rate_table(document: dict) -> dict[str, dict]:
    def rates(raw: dict, suffix: str, standard: dict | None = None) -> dict | None:
        inp, out = _number(raw.get(f"input_cost_per_token{suffix}")), _number(raw.get(f"output_cost_per_token{suffix}"))
        if inp is None or out is None:
            return None
        result = {"uncached_input": inp, "output": out}
        for field, name in (("cache_read", "cache_read_input_token_cost"), ("cache_write", "cache_creation_input_token_cost")):
            value = _number(raw.get(name + suffix))
            result[field] = value if value is not None else (standard[field] / standard["uncached_input"] * inp
                           if standard and standard["uncached_input"] else inp)
        return result

    table: dict[str, dict] = {}
    for model, raw in _object(document).items():
        if not isinstance(raw, dict) or (standard := rates(raw, "")) is None:
            continue
        multiple = _number(_object(raw.get("provider_specific_entry")).get("fast"))
        table[model.strip().lower()] = {
            "standard": standard,
            "fast": {key: value * multiple for key, value in standard.items()} if multiple else rates(raw, "_priority", standard),
            "ultrafast": rates(raw, "_ultrafast", standard),
        }
    aliases: dict[str, dict | None] = {}
    for model, rate in table.items():
        bare = model.rsplit("/", 1)[-1]
        if bare == model or bare in table:
            continue
        if bare not in aliases:
            aliases[bare] = rate
        elif aliases[bare] != rate:
            aliases[bare] = None
    table.update({name: rate for name, rate in aliases.items() if rate is not None})
    return table


def price_records(records: Iterable[dict], rates: dict[str, dict]) -> list[dict]:
    result = []
    for original in records:
        row = dict(original)
        reported = _number(row.get("reported_cost"))
        key = str(row.get("rate_model") or row.get("model") or "").strip().lower().split("[", 1)[0]
        unpriceable = key.rsplit("/", 1)[-1] in UNPRICEABLE
        rate = None if unpriceable else rates.get(key)
        estimated = _number(row.get("estimated_cost")) if reported is None and not unpriceable else None
        if reported is None and rate is not None and not row.get("missing"):
            tier = rate.get(row.get("speed", "standard")) or rate["standard"]
            # Native records have complete disjoint buckets; the ledger's
            # undisclosed cache mix cannot be priced as all uncached input.
            cache_known = row.get("cache_breakdown_known", True)
            if cache_known:
                estimated = sum((row.get(field) or 0) * tier[field] for field in ("uncached_input", "cache_read", "cache_write", "output"))
            elif tier["uncached_input"] == tier["cache_read"] == tier["cache_write"]:
                estimated = (row.get("input_tokens") or 0) * tier["uncached_input"] + (row.get("output") or 0) * tier["output"]
        row.update(reported_cost=reported, estimated_cost=estimated, cost=reported if reported is not None else estimated)
        result.append(row)
    return result


def _empty_totals() -> dict:
    return {**dict.fromkeys(AMOUNT_FIELDS), **dict.fromkeys(COUNT_FIELDS, 0)}


def _sum_known(target: dict, source: dict, fields: Iterable[str]) -> None:
    for field in fields:
        if source.get(field) is not None:
            target[field] = (target.get(field) or 0) + source[field]


def make_usage_window(days: int, tz: str, start: float | None = None,
                      end: float | None = None) -> UsageWindow:
    """Custom bounds are unix seconds; the default uses native calendar days."""
    if start is None:
        return make_window(days, tz, now=end)
    end = time.time() if end is None else end
    if not math.isfinite(start) or not math.isfinite(end) or start < 0 or end < start:
        raise ValueError("用量时间范围无效")
    try:
        zone = ZoneInfo(tz)
    except (ZoneInfoNotFoundError, ValueError):
        raise ValueError(f"无法识别的时区：{tz}") from None
    if start == 0:
        # Used only while reading all history; the service builds real slots
        # after it knows the first observed event instead of drawing from 1970.
        return UsageWindow(days, tz, 0, int(end * 1000), "day", ())
    since, until = datetime.fromtimestamp(start, zone), datetime.fromtimestamp(end, zone)
    hourly = end - start <= 86400
    slots = []
    if hourly:
        point = since.replace(minute=0, second=0, microsecond=0)
        while point.timestamp() <= end:
            slots.append(point.strftime("%Y-%m-%dT%H:00"))
            point = (point.astimezone(timezone.utc) + timedelta(hours=1)).astimezone(zone)
    else:
        point = since.date()
        while point <= until.date():
            slots.append(point.isoformat())
            point += timedelta(days=1)
    return UsageWindow(days, tz, int(start * 1000), int(end * 1000), "hour" if hourly else "day", tuple(slots))


def _select(records: Iterable[dict], engine: str | None, model: str | None) -> list[dict]:
    wanted = canonical_engine(engine) if engine else None
    return [row for row in records if (not wanted or row["engine"] == wanted) and (not model or row["model"] == model)]


def summarize(records: Iterable[dict], window: UsageWindow) -> dict:
    """Aggregate priced numerical records with explicit nulls and zero-fill."""
    totals, providers, models = _empty_totals(), {}, {}
    zone = ZoneInfo(window.time_zone)
    series = {slot: {"slot": slot, "total_tokens": 0, "cost": 0, "providers": {}} for slot in window.slots}
    dedupe: set[tuple] = set()
    session_sets: dict[tuple, set[tuple]] = {}
    for row in records:
        if not window.since_ms <= row["at_ms"] <= window.until_ms:
            continue
        engine, model = row["engine"], row["model"]
        if row.get("key"):
            key = (engine, row["key"])
            if key in dedupe:
                continue
            dedupe.add(key)
        provider = providers.setdefault(engine, {"engine": engine, "status": "ready", **_empty_totals()})
        model_row = models.setdefault((engine, model), {"engine": engine, "model": model, **_empty_totals()})
        for aggregate, group in ((totals, ()), (provider, (engine,)), (model_row, (engine, model))):
            _sum_known(aggregate, row, AMOUNT_FIELDS)
            aggregate["records"] += 1
            aggregate["unpriced"] += int(row.get("cost") is None)
            aggregate["missing"] += int(bool(row.get("missing")))
            aggregate["historical_scope_unknown"] += int(bool(row.get("historical_scope_unknown")))
            if row.get("session"):
                sessions = session_sets.setdefault(group, set())
                sessions.add((engine, row["session"]))
                aggregate["sessions"] = len(sessions)
        moment = datetime.fromtimestamp(row["at_ms"] / 1000, zone)
        slot = moment.strftime("%Y-%m-%dT%H:00") if window.resolution == "hour" else moment.date().isoformat()
        point = series.get(slot)
        if point is not None:
            engine_point = point["providers"].setdefault(engine, {"total_tokens": None, "cost": None})
            _sum_known(engine_point, row, ("total_tokens", "cost"))
    # Empty time buckets are real zeros. Occupied-but-unreported buckets are
    # unknown, while partial known totals are accompanied by missing counts.
    for point in series.values():
        for engine, provider in providers.items():
            if engine not in point["providers"]:
                point["providers"][engine] = {field: 0 if provider[field] is not None else None
                                               for field in ("total_tokens", "cost")}
        if point["providers"]:
            for field in ("total_tokens", "cost"):
                values = [item[field] for item in point["providers"].values() if item[field] is not None]
                point[field] = sum(values) if values else None
    order = lambda row: (-(row["cost"] or 0), -(row["total_tokens"] or 0), row.get("model", row.get("engine", "")))
    return {"as_of": time.time(), "window": asdict(window), "totals": totals,
            "providers": sorted(providers.values(), key=order), "models": sorted(models.values(), key=order),
            "series": list(series.values()), "sources": []}


class ProviderUsage:
    """Local history service. Call from a worker thread, never an async loop."""

    def __init__(self, state_root: str | Path, *, cursor: CursorAccountUsage | None = None,
                 env: Mapping[str, str] | None = None, home: Path | None = None) -> None:
        self.root = Path(state_root)
        self.env = dict(os.environ if env is None else env)
        self.home = home or Path.home()
        self.cursor = cursor or CursorAccountUsage(self.root, env=self.env)
        self._lock = threading.Lock()
        self._files: dict[str, dict] = {}
        self._dirty_files: set[str] = set()
        self._rates: dict[str, dict] = {}
        self._rates_at = 0.0
        self._rate_attempt_at = 0.0
        self._pricing: dict = {"status": "unavailable", "source": RATE_URL, "fetched_at": None}
        self._cache_loaded = False
        self._cache_issue: str | None = None

    def _load_rates(self, refresh: bool) -> dict:
        now = time.time()
        max_age = 60 if refresh else 86400
        path = self.root / "_usage_model_rates.json"
        if not self._rates_at:
            try:
                cache = json.loads(path.read_text())
                self._rates = parse_rate_table(cache["document"])
                self._rates_at = float(cache["fetched_at"]) if self._rates else 0
                self._pricing.update(status="cached" if self._rates else "unavailable", fetched_at=self._rates_at or None)
            except FileNotFoundError:
                pass
            except (OSError, ValueError, KeyError, TypeError) as exc:
                self._pricing["message"] = f"价格缓存读取失败（{type(exc).__name__}）"
        if now - self._rates_at < max_age or now - self._rate_attempt_at < 60:
            return self._rates
        self._rate_attempt_at = now
        try:
            response = httpx.get(RATE_URL, timeout=10, follow_redirects=False)
            response.raise_for_status()
            document = response.json()
            parsed = parse_rate_table(document)
            if not parsed:
                raise ValueError("empty rate table")
            self._rates, self._rates_at = parsed, now
            self._pricing = {"status": "fresh", "source": RATE_URL, "fetched_at": now}
            self.root.mkdir(parents=True, exist_ok=True)
            temporary = path.with_suffix(".tmp")
            temporary.write_text(json.dumps({"fetched_at": now, "document": document}))
            temporary.replace(path)
        except (httpx.HTTPError, OSError, ValueError, TypeError) as exc:
            self._pricing.update(status="cached" if self._rates else "unavailable",
                                 message=f"价格表更新失败（{type(exc).__name__}）；" + ("使用已缓存价格。" if self._rates else "未知模型保持未定价。"))
        return self._rates

    def _homes(self, engine: str) -> list[Path]:
        from muteki.solver.credential_accounts import host_discovery_enabled

        if not host_discovery_enabled(self.env):
            roots = []
        elif engine == "opencode":
            data = self.env.get("XDG_DATA_HOME", "")
            base = Path(data) if data and Path(data).is_absolute() else self.home / ".local/share"
            roots = [base / "opencode"]
        else:
            variable = {"codex": "CODEX_HOME", "claude": "CLAUDE_CONFIG_DIR", "grok": "GROK_HOME"}[engine]
            roots = [Path(self.env.get(variable) or self.home / f".{engine}").expanduser()]
        # Only inspect directory names; account credentials are never read.
        accounts = self.root / "_secrets/accounts"
        if accounts.is_dir():
            for account in accounts.iterdir():
                if account.is_dir() and not account.is_symlink():
                    candidate = account / f"{engine}-home"
                    if candidate.is_dir():
                        roots.append(candidate)
        return list(dict.fromkeys(root.resolve() for root in roots))

    def _load_scan_cache(self) -> None:
        if self._cache_loaded:
            return
        self._cache_loaded = True
        path = self.root / "_provider_usage_cache.sqlite3"
        if not path.exists():
            return
        try:
            with sqlite3.connect(path) as db:
                for key, data in db.execute("SELECT path,data FROM files"):
                    cached = json.loads(data)
                    if not isinstance(cached, dict) or not isinstance(cached.get("records"), list):
                        raise ValueError("invalid usage cache")
                    self._files[key] = cached
        except (sqlite3.Error, ValueError, OSError, TypeError) as exc:
            self._cache_issue = f"历史缓存读取失败（{type(exc).__name__}）；本次重新扫描。"
            self._files.clear()

    def _save_scan_cache(self, active: set[str]) -> None:
        removed = self._files.keys() - active
        self._files = {key: value for key, value in self._files.items() if key in active}
        changed = self._dirty_files & active
        if not removed and not changed:
            return
        try:
            self.root.mkdir(parents=True, exist_ok=True)
            with sqlite3.connect(self.root / "_provider_usage_cache.sqlite3", timeout=1) as db:
                db.execute("CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY, data TEXT NOT NULL)")
                db.executemany("DELETE FROM files WHERE path=?", ((key,) for key in removed))
                db.executemany("INSERT OR REPLACE INTO files VALUES (?,?)", ((key, json.dumps(self._files[key])) for key in changed))
            self._dirty_files.clear()
        except (sqlite3.Error, OSError) as exc:
            self._cache_issue = f"历史缓存写入失败（{type(exc).__name__}）；本次结果仍来自实际文件。"

    def _scan_file(self, path: Path, engine: str, cutoff: int) -> tuple[list[dict], int]:
        stat = path.stat()
        key = str(path)
        cached = self._files.get(key, {})
        signature = [stat.st_ino, stat.st_mtime_ns, stat.st_size]
        cutoff_covered = cached.get("cutoff", cutoff + 1) <= cutoff
        if cached.get("signature") == signature and cutoff_covered:
            return cached["records"], cached.get("errors", 0)
        append = (cutoff_covered and cached.get("signature", [None])[0] == stat.st_ino
                  and stat.st_size > cached.get("signature", [0, 0, 0])[2])
        rows = list(cached.get("records", [])) if append else []
        state = dict(cached.get("state", {})) if append else {}
        offset = cached.get("offset", 0) if append else 0
        errors = cached.get("errors", 0) if append else 0
        with path.open("rb") as handle:
            handle.seek(offset)
            for raw in handle:
                final_fragment = not raw.endswith(b"\n")
                position = handle.tell()
                if not raw.strip():
                    offset = position
                    continue
                if engine == "claude" and b'"usage"' not in raw:
                    if not final_fragment:
                        offset = position
                    continue
                if engine == "grok" and b'"turn_completed"' not in raw:
                    if not final_fragment:
                        offset = position
                    continue
                if engine == "codex" and not any(marker in raw for marker in (b'"token_count"', b'"turn_context"', b'"session_meta"', b'"thread_settings_applied"')):
                    if not final_fragment:
                        offset = position
                    continue
                try:
                    record = json.loads(raw)
                except (ValueError, UnicodeDecodeError):
                    # Retry an unfinished active-writer line on the next scan.
                    if final_fragment:
                        break
                    errors += 1
                    offset = position
                    continue
                offset = position
                if not isinstance(record, dict):
                    continue
                parsed = parse_codex(record, state) if engine == "codex" else parse_claude(record) if engine == "claude" else parse_grok(record)
                for row in parsed:
                    if row["at_ms"] >= cutoff:
                        if not row["session"]:
                            row["session"] = hashlib.sha256(key.encode()).hexdigest()[:20]
                        # Copies of a rollout in multiple homes must not count
                        # twice; native event identity survives path changes.
                        if engine == "codex":
                            row["key"] = f'{row["session"]}:{row["at_ms"]}:{state.get("signature", "")}'
                        rows.append(row)
        self._files[key] = {"signature": signature, "offset": offset, "state": state, "records": rows, "errors": errors, "cutoff": cutoff}
        self._dirty_files.add(key)
        return rows, errors

    def _scan_transcripts(self, engine: str, cutoff: int) -> tuple[list[dict], dict, set[str]]:
        from muteki.solver.credential_accounts import host_discovery_enabled

        rows, errors, found, active = [], 0, False, set()
        error_types: set[str] = set()
        try:
            roots = self._homes(engine)
            for root in roots:
                directories = [root / ("projects" if engine == "claude" else "sessions")]
                if engine == "codex":
                    directories.append(root / "archived_sessions")
                for directory in directories:
                    if not directory.exists():
                        continue
                    found = True
                    def walk_error(exc: OSError) -> None:
                        error_types.add(type(exc).__name__)
                    for base, names, files in os.walk(directory, followlinks=False, onerror=walk_error):
                        names[:] = sorted(name for name in names if not (Path(base) / name).is_symlink())
                        for name in sorted(files):
                            path = Path(base) / name
                            if not name.endswith(".jsonl") or path.is_symlink():
                                continue
                            try:
                                if path.stat().st_mtime * 1000 < cutoff - 36 * 3600 * 1000:
                                    continue
                                active.add(str(path))
                                parsed, malformed = self._scan_file(path, engine, cutoff)
                                rows.extend(parsed)
                                errors += malformed
                            except OSError as exc:
                                error_types.add(type(exc).__name__)
        except OSError as exc:
            error_types.add(type(exc).__name__)
        disabled = not found and not host_discovery_enabled(self.env)
        status = "partial" if errors or error_types else "ready" if found else "disabled" if disabled else "missing"
        source = {"engine": engine, "status": status, "scope": "native", "message": "原生 CLI 历史" if found else "未找到此主机的原生历史；Muteki 记录可在内部用量查看。"}
        if disabled:
            source["message"] = "宿主历史发现已禁用；未找到注册账户历史。"
        if errors or error_types:
            source["message"] = f"部分历史无法读取：{errors} 条格式错误；" + ", ".join(sorted(error_types))
        return rows, source, active

    def _scan_opencode(self, cutoff: int) -> tuple[list[dict], dict, set[str]]:
        from muteki.solver.credential_accounts import host_discovery_enabled

        rows, found, errors = [], False, set()
        try:
            for root in self._homes("opencode"):
                if not root.exists():
                    continue
                databases = sorted((item for item in root.iterdir() if item.is_file() and not item.is_symlink()
                                    and re.fullmatch(r"opencode(?:-[a-zA-Z0-9_-]+)?\.db", item.name)),
                                   key=lambda item: (item.name != "opencode.db", item.name))
                for path in databases:
                    found = True
                    try:
                        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=0.1) as db:
                            tables = {item[0] for item in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                            if not tables.intersection(("message", "session_message")):
                                errors.add("unsupported_schema")
                            for table in ("message", "session_message"):
                                if table not in tables:
                                    continue
                                columns = {item[1] for item in db.execute(f"PRAGMA table_info({table})")}
                                if not {"id", "session_id", "data"}.issubset(columns):
                                    errors.add("unsupported_schema")
                                    continue
                                stamp = "time_created" if "time_created" in columns else "NULL"
                                conditions = ["type='assistant'"] if table == "session_message" and "type" in columns else []
                                if stamp != "NULL":
                                    conditions.append("time_created >= ?")
                                where = " WHERE " + " AND ".join(conditions) if conditions else ""
                                for uid, session, data, at in db.execute(f"SELECT id,session_id,data,{stamp} FROM {table}{where}", (cutoff,) if stamp != "NULL" else ()):
                                    try:
                                        rows.extend(parse_opencode(_object(json.loads(data)), uid=uid, session=session, at=at))
                                    except (ValueError, TypeError):
                                        errors.add("invalid_message")
                    except sqlite3.Error as exc:
                        errors.add(type(exc).__name__)
                directory = root / "storage/message"
                if directory.exists():
                    found = True
                    for base, names, files in os.walk(directory, followlinks=False, onerror=lambda exc: errors.add(type(exc).__name__)):
                        names[:] = sorted(name for name in names if not (Path(base) / name).is_symlink())
                        for name in sorted(files):
                            path = Path(base) / name
                            if not name.endswith(".json") or path.is_symlink():
                                continue
                            try:
                                if path.stat().st_mtime * 1000 >= cutoff - 36 * 3600 * 1000:
                                    rows.extend(parse_opencode(_object(json.loads(path.read_text())), uid=path.stem))
                            except (OSError, ValueError) as exc:
                                errors.add(type(exc).__name__)
        except OSError as exc:
            errors.add(type(exc).__name__)
        disabled = not found and not host_discovery_enabled(self.env)
        return rows, {"engine": "opencode", "scope": "native", "status": "partial" if errors else "ready" if found else "disabled" if disabled else "missing",
                      "message": ("部分 OpenCode 历史不可读：" + ", ".join(sorted(errors))) if errors else "原生 CLI 历史" if found else "宿主历史发现已禁用；未找到注册账户历史。" if disabled else "未找到 OpenCode 历史；Muteki 记录可在内部用量查看。"}, set()

    def summarize_ledger(self, rows: Iterable[dict], days: int, tz: str, *, refresh: bool = False,
                         start: float | None = None, end: float | None = None,
                         engine: str | None = None, model: str | None = None) -> dict:
        with self._lock:
            records = _select(price_records(ledger_to_records(rows), self._load_rates(refresh)), engine, model)
            if start == 0:
                start = min((row["at_ms"] / 1000 for row in records), default=end or time.time())
            window = make_usage_window(days, tz, start, end)
            result = summarize(records, window)
            result["pricing"] = dict(self._pricing)
        result["sources"] = [{"engine": row["engine"], "status": "ready", "scope": "muteki", "message": "仅统计 Muteki 内部用量"}
                             for row in result["providers"]]
        return result

    def _read_cursor(self, window: UsageWindow, refresh: bool) -> dict:
        from muteki.solver.credential_accounts import host_discovery_enabled

        if not host_discovery_enabled(self.env):
            return {"history": None, "error": {"code": "host_discovery_disabled",
                    "message": "此服务已禁用宿主登录发现，未读取 Cursor 宿主账户。"}}
        try:
            return self.cursor.read(window, refresh=refresh)
        except CursorUsageError as exc:
            return {"history": None, "error": exc.view()}
        except (OSError, ValueError, httpx.HTTPError) as exc:
            # Source failures stay local; never expose network response bodies
            # or credential file contents through exception text.
            return {"history": None, "error": {"code": "cursor_history_read_failed",
                    "message": f"Cursor 账户历史读取失败（{type(exc).__name__}）"}}

    def read(self, days: int, tz: str, *, refresh: bool = False, ledger_records: Iterable[dict] = (),
             start: float | None = None, end: float | None = None,
             engine: str | None = None, model: str | None = None) -> dict:
        window = make_usage_window(days, tz, start, end)
        cutoff = min(int((time.time() - 91 * 86400) * 1000), window.since_ms)
        with self._lock:
            self._load_scan_cache()
            records, sources, active = [], [], set()
            with ThreadPoolExecutor(max_workers=4, thread_name_prefix="usage-read") as pool:
                scans = [pool.submit(self._scan_transcripts, engine, cutoff) for engine in ("codex", "claude", "grok")]
                scans.append(pool.submit(self._scan_opencode, cutoff))
                cursor_future = pool.submit(self._read_cursor, window, refresh or start is not None or end is not None)
                rates_future = pool.submit(self._load_rates, refresh)
                for future in scans:
                    rows, source, paths = future.result()
                    records.extend(rows)
                    sources.append(source)
                    active.update(paths)
                rates = rates_future.result()
                cursor = cursor_future.result()
            supplemental = [row for row in ledger_to_records(ledger_records) if row["engine"] not in NATIVE_ENGINES]
            records.extend(supplemental)
            for ledger_engine in sorted(set(LEDGER_ENGINES) | {row["engine"] for row in supplemental}):
                sources.append({"engine": ledger_engine, "status": "ready", "scope": "muteki", "message": "仅统计 Muteki 内部用量"})
            if isinstance(cursor.get("_records"), list):
                for raw in cursor["_records"]:
                    records.append(_record("cursor", raw["at_ms"], raw["model"], raw["session"],
                                           inp=raw["uncached_input"], read=raw["cache_read"], write=raw["cache_write"],
                                           out=raw["output"], reported=raw["reported_cost"]))
                error = cursor.get("error") or cursor.get("history_error")
                sources.append({"engine": "cursor", "scope": "account", "status": "error" if error else "ready",
                                "message": error.get("message") if error else "Cursor 账户历史", "code": error.get("code") if error else None})
            records = _select(price_records(records, rates), engine, model)
            if start == 0:
                first = min((row["at_ms"] / 1000 for row in records), default=end or time.time())
                window = make_usage_window(days, tz, first, end)
            result = summarize(records, window)
            if not isinstance(cursor.get("_records"), list) and (not engine or canonical_engine(engine) == "cursor"):
                if model and cursor.get("history"):
                    cursor = {"history": None, "error": {"code": "history_records_unavailable",
                              "message": "Cursor 旧缓存缺少模型明细，请刷新后重试。"}}
                self._merge_cursor(result, cursor)
            sources.extend(result["sources"])
            sources = [source for source in sources if not engine or source["engine"] == canonical_engine(engine)]
            result["sources"] = sources
            result["pricing"] = dict(self._pricing)
            self._save_scan_cache(active)
            if self._cache_issue:
                result["cache_warning"] = self._cache_issue
            by_engine = {row["engine"]: row for row in result["providers"]}
            for source in sources:
                row = by_engine.get(source["engine"])
                if row is None:
                    row = {"engine": source["engine"], **_empty_totals()}
                    result["providers"].append(row)
                row.update(status=source["status"], message=source.get("message"), scope=source.get("scope"))
            return result

    @staticmethod
    def _merge_cursor(result: dict, payload: dict) -> None:
        history = payload.get("history")
        error = payload.get("error") or payload.get("history_error")
        status = "disabled" if error and error.get("code") == "host_discovery_disabled" else "error" if error else "ready" if history else "missing"
        result["sources"].append({"engine": "cursor", "scope": "account", "status": status,
                                  "message": error.get("message") if error else "Cursor 账户历史", "code": error.get("code") if error else None})
        if not history:
            return
        def convert(raw: dict) -> dict:
            output = {**_empty_totals(), **{field: raw.get(field) for field in ("total_tokens", *TOKEN_FIELDS)}}
            output.update(reported_cost=raw.get("reported_cost"), cost=raw.get("reported_cost"), records=raw.get("events", 0),
                          unpriced=raw.get("unpriced_events", 0), sessions=raw.get("sessions", 0), missing=0)
            return output
        totals = convert(history["totals"])
        result["providers"].append({"engine": "cursor", "status": "ready", **totals})
        _sum_known(result["totals"], totals, (*AMOUNT_FIELDS, *COUNT_FIELDS))
        for row in history["models"]:
            result["models"].append({"engine": "cursor", "model": row["model"], **convert(row)})
        points = {row["slot"]: row for row in history["series"]}
        for point in result["series"]:
            if point["slot"] in points:
                source = points[point["slot"]]
                addition = {"total_tokens": source["total_tokens"], "cost": source["reported_cost"]}
                point["providers"]["cursor"] = addition
                _sum_known(point, addition, ("total_tokens", "cost"))
        result["providers"].sort(key=lambda row: (-(row["cost"] or 0), -(row["total_tokens"] or 0)))
        result["models"].sort(key=lambda row: (-(row["cost"] or 0), -(row["total_tokens"] or 0)))
