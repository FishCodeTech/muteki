"""Exact, route-aware price quotes and reversible local price configuration.

Prices are USD per million tokens. Configuration never alters captured usage.
"""
from __future__ import annotations

import json
import math
from pathlib import Path
import sqlite3
from typing import Any

RATE_FIELDS = ('input', 'output', 'cache_read', 'cache_write')
BUCKETS = dict(input='uncached_input', output='output', cache_read='cache_read', cache_write='cache_write')


def identity(value: dict) -> dict:
    from muteki.core.provider_usage import canonical_engine
    if not isinstance(value, dict):
        raise ValueError('价格条目必须是对象')
    engine, model = value.get('engine'), value.get('model')
    if not isinstance(engine, str) or not engine.strip() or not isinstance(model, str) or not model.strip():
        raise ValueError('engine 和 model 必填')
    result = dict(engine=canonical_engine(engine), model=model.strip().lower())
    speed = value.get('speed')
    if speed is not None:
        if not isinstance(speed, str) or not speed.strip():
            raise ValueError('speed 必须是非空标识符')
        speed = speed.strip().lower()
        # Legacy speedless keys denote standard, so standard updates/deletes keep their existing key.
        if speed != 'standard':
            result['speed'] = speed
    for field in ('credential_id', 'provider_id'):
        item = value.get(field)
        if item is not None and (not isinstance(item, str) or not item.strip()):
            raise ValueError(f'{field} 必须是非空标识符')
        if item is not None:
            # Identity is a local account/provider ID, never a URL or credential.
            if '://' in item or len(item) > 256 or any(char in item for char in '\r\n'):
                raise ValueError(f'{field} 必须使用本地标识符')
            result[field] = item.strip()
    return result


def _key(item: dict) -> str:
    return json.dumps(identity(item), sort_keys=True, separators=(',', ':'))


class PricingStore:
    def __init__(self, root: str | Path):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.path = self.root / '_usage_pricing.sqlite3'
        with sqlite3.connect(self.path) as db:
            db.execute('CREATE TABLE IF NOT EXISTS config (kind TEXT, identity TEXT, data TEXT, PRIMARY KEY(kind,identity))')

    def entries(self, kind: str) -> list[dict]:
        with sqlite3.connect(self.path) as db:
            return [json.loads(row[0]) for row in db.execute('SELECT data FROM config WHERE kind=? ORDER BY identity', (kind,))]

    def put(self, kind: str, body: dict) -> dict:
        entry = identity(body)
        if kind == 'override':
            raw = body.get('rates')
            if not isinstance(raw, dict) or any(key not in RATE_FIELDS for key in raw):
                raise ValueError('rates 仅接受 input、output、cache_read、cache_write')
            rates = {}
            for field in RATE_FIELDS:
                value = raw.get(field)
                if value is not None and (isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0):
                    raise ValueError(f'{field} 必须是非负有限数字或 null')
                rates[field] = value
            if all(value is None for value in rates.values()):
                raise ValueError('至少填写一项价格；0 表示免费，null 表示未知')
            entry['rates'] = rates
        elif kind == 'reference':
            model = body.get('reference_model')
            if not isinstance(model, str) or not model.strip():
                raise ValueError('reference_model 必填')
            entry['reference_model'] = model.strip().lower()
        else:
            raise ValueError('无效价格设置类型')
        with sqlite3.connect(self.path) as db:
            db.execute('INSERT OR REPLACE INTO config VALUES (?,?,?)', (kind, _key(entry), json.dumps(entry)))
        return entry

    def delete(self, kind: str, body: dict) -> dict:
        with sqlite3.connect(self.path) as db:
            cursor = db.execute('DELETE FROM config WHERE kind=? AND identity=?', (kind, _key(body)))
        return {'ok': True, 'deleted': bool(cursor.rowcount)}

    @staticmethod
    def _match(entries: list[dict], item: dict) -> tuple[dict | None, bool]:
        matches = [entry for entry in entries if entry['engine'] == item['engine'] and entry['model'] == item['model']
                   and entry.get('speed', 'standard') == item.get('speed', 'standard')
                   and all(entry.get(field) is None or entry[field] == item.get(field) for field in ('credential_id', 'provider_id'))]
        if not matches:
            return None, False
        specificity = max(sum(field in entry for field in ('credential_id', 'provider_id')) for entry in matches)
        best = [entry for entry in matches if sum(field in entry for field in ('credential_id', 'provider_id')) == specificity]
        return (best[0], False) if len(best) == 1 else (None, True)

    def resolver(self, rates: dict):
        """Snapshot config once per batch, without network requests per model."""
        overrides, references = self.entries('override'), self.entries('reference')
        candidates_by_model: dict[str, list[str]] = {}
        for name in sorted(rates):
            if not name.startswith('_'):
                candidates_by_model.setdefault(name.rsplit('/', 1)[-1], []).append(name)
        def resolve(raw: dict) -> dict:
            from muteki.core.provider_usage import _record_rate, _number, UNPRICEABLE
            item = identity(raw)
            related = [entry for entry in (*overrides, *references) if entry['engine'] == item['engine'] and entry['model'] == item['model']
                       and entry.get('speed', 'standard') == item.get('speed', 'standard')]
            missing_scope = sorted({field for entry in related for field in ('credential_id', 'provider_id') if entry.get(field) and not item.get(field)})
            result = {**item, 'status': 'missing', 'rates': None, 'candidates': [], 'missing_scope': missing_scope}
            if raw.get('speed'):
                result['speed'] = item.get('speed', 'standard')
            custom, conflict = self._match(overrides, item)
            if conflict:
                return {**result, 'status': 'ambiguous'}
            if custom:
                return {**result, 'status': 'custom', 'rates': custom['rates']}
            reference, conflict = self._match(references, item)
            if conflict:
                return {**result, 'status': 'ambiguous'}
            model = reference['reference_model'] if reference else item['model']
            if reference:
                result['reference_model'] = model
            key = model
            if not reference and raw.get('catalog_provider') == 'ollama-cloud' and raw.get('route_provenance') and '/' not in key:
                key = 'ollama-cloud/' + key
            elif item['engine'] in ('cursor', 'devin') and not reference and not key.startswith(item['engine'] + '/'):
                key = item['engine'] + '/' + key
            elif (not reference and item['engine'] == 'grok' and item.get('credential_id') == 'system:grok'
                  and not item.get('provider_id') and '/' not in key):
                # This identity denotes the official logged-in Grok account.
                # Other accounts/routes remain exact and can be ambiguous.
                # Build-only products have their own published tariff; they
                # are not public API model aliases or the standard API rate.
                product_key = 'grok/' + key
                key = product_key if product_key in rates else 'xai/' + key
            elif not reference and '/' not in key and item.get('provider_id'):
                key = item['provider_id'].lower() + '/' + key
            candidates = [name for name in candidates_by_model.get(key.rsplit('/', 1)[-1], ())
                          if item['engine'] not in ('cursor', 'devin') or name.startswith(item['engine'] + '/')]
            result['candidates'] = candidates
            if key not in rates and candidates and all(rates[name].get('pricing_status') == 'subscription_unknown' for name in candidates):
                return {**result, 'status': 'subscription_unknown'}
            if model.rsplit('/', 1)[-1] in UNPRICEABLE and not reference:
                return result
            if key in rates and rates[key].get('pricing_status') == 'subscription_unknown':
                return {**result, 'status': 'subscription_unknown', 'catalog_model': key}
            if key not in rates:
                return {**result, 'status': 'ambiguous' if len(candidates) > 1 else 'missing'}
            rate = _record_rate({**raw, **item, 'speed': item.get('speed', 'standard') if raw.get('speed') else None,
                                 **({'rate_model': model} if reference else {})}, key, rates)
            if rate is None:
                return {**result, 'status': 'unknown-tier'}
            # Cursor's product-specific Fast variant is already selected by
            # _record_rate; its row's standard price is the applicable tariff.
            speed = 'standard' if item['engine'] == 'cursor' else item.get('speed', 'standard')
            tier = rate.get(speed)
            prompt_tokens = _number(raw.get('input_tokens'))
            if prompt_tokens is None:
                parts = [_number(raw.get(field)) for field in ('uncached_input', 'cache_read', 'cache_write')]
                if all(part is not None for part in parts):
                    prompt_tokens = sum(parts)
            if rate.get('long_context_threshold') and (prompt_tokens or 0) > rate['long_context_threshold']:
                tier = rate.get('long_context') if speed == 'standard' else None
            if tier is None:
                return {**result, 'status': 'unknown-tier'}
            return {**result, 'status': 'catalog', 'catalog_model': key, 'rates': {field: tier.get(bucket) * 1e6 if tier.get(bucket) is not None else None for field, bucket in BUCKETS.items()}}
        return resolve


def price_ledger_rows(rows: list[dict], provider=None) -> list[dict]:
    """Price presentation copies only; the SQLite observations remain immutable."""
    if not rows:
        return []
    from muteki.core.provider_usage import ledger_to_records, price_records
    if provider is None:
        raise ValueError('provider pricing service is required')
    rates, _ = provider.pricing_snapshot()
    priced = price_records(ledger_to_records(rows), rates, resolver=provider.price_store.resolver(rates))
    by_id = {row['key']: row for row in priced}
    result = []
    for original in rows:
        row = dict(original)
        priced_row = by_id.get(str(row.get('id')))
        if priced_row:
            row['captured_cost'] = original.get('captured_cost') or {field: original.get(field) for field in ('reported_cost', 'estimated_cost', 'price_source', 'cost_status')}
            row.update(reported_cost=priced_row['reported_cost'], estimated_cost=priced_row['estimated_cost'], cost=priced_row['cost'],
                       pricing_issues=priced_row.get('pricing_issues', []), pricing={**(priced_row.get('pricing') or {}), 'cache_write_applicability': priced_row.get('cache_write_applicability'), 'token_quality': original.get('quality')}, amount_source=priced_row.get('amount_source'), price_source='model_price', cost_status='priced_view')
        result.append(row)
    return result


_services: dict[str, Any] = {}
def pricing_service(root: str | Path):
    from muteki.core.provider_usage import ProviderUsage
    key = str(Path(root).resolve())
    if key not in _services:
        _services[key] = ProviderUsage(root)
    return _services[key]


def pricing_issues(row: dict) -> list[dict]:
    """Explain missing prices/usage without exposing configuration or endpoints."""
    quote = row.get('pricing') or {}
    identity_fields = ('engine', 'model', 'credential_id', 'provider_id', 'speed', 'worker_id', 'role')
    base = {key: row[key] for key in identity_fields if row.get(key)}
    if quote.get('engine'):
        base['engine'] = quote['engine']
    reasons = []
    historical = row.get('historical_scope_unknown') or row.get('measurement_scope') == 'historical_scope_unknown'
    if historical:
        reasons.append('historical_scope_unknown')
    missing_tokens = (row.get('missing') or row.get('input_lower_bound')
                      or row.get('quality') in ('missing', 'partial')
                      or row.get('token_quality') in ('missing', 'partial'))
    if missing_tokens:
        reasons.append('missing_tokens')
    if row.get('cost') is None:
        status = quote.get('status')
        if status == 'ambiguous':
            reasons.append('ambiguous_model')
            if not row.get('provider_id') and not row.get('catalog_provider'):
                reasons.append('missing_provider')
        elif status == 'unknown-tier':
            reasons.append('unknown_tier')
        elif status == 'subscription_unknown':
            reasons.append('subscription_unknown')
        elif status == 'missing':
            reasons.append('missing_price')
        elif status in ('catalog', 'custom'):
            if row.get('cache_breakdown_known') is False:
                if 'missing_tokens' not in reasons:
                    reasons.append('missing_tokens')
            elif not missing_tokens:
                reasons.append('missing_price')
        elif not reasons:
            reasons.append('missing_price')
    return [{**base, 'reason': reason, 'count': 1,
             'candidates': quote.get('candidates', []), 'missing_scope': quote.get('missing_scope', [])}
            for reason in dict.fromkeys(reasons)]


def aggregate_pricing_issues(rows) -> list[dict]:
    groups = {}
    for row in rows:
        for issue in row.get('pricing_issues', []):
            value = {key: val for key, val in issue.items() if key != 'count'}
            key = json.dumps(value, sort_keys=True, ensure_ascii=False)
            if key not in groups:
                groups[key] = {**value, 'count': 0}
            groups[key]['count'] += issue.get('count', 1)
    return list(groups.values())
