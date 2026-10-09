"""Read-only usage views shared by global, Run and Competition pages."""
from __future__ import annotations
import time
from threading import Lock
from typing import Any, Literal

from fastapi import APIRouter, Query, HTTPException, Request
from muteki.competition.models import RunBinding
from muteki.core.cursor_usage import CursorAccountUsage, make_window
from muteki.core.provider_usage import ProviderUsage, make_usage_window
from muteki.core.provider_limits import ProviderLimits



def create_usage_router(manager, competition_store):
    router = APIRouter(tags=['usage'])
    cursor_account = CursorAccountUsage(manager.state_root)
    provider_usage = ProviderUsage(manager.state_root, cursor=cursor_account)
    provider_limits = ProviderLimits(manager.state_root, keychain=cursor_account.keychain)
    ownership_lock = Lock()
    ownership_signature: tuple[tuple[str, str, str], ...] | None = None

    def sync_competition_ownership():
        nonlocal ownership_signature
        signature = tuple(
            (binding.run_id, binding.competition_id,
             binding.competition_challenge_id)
            for binding in competition_store.list(RunBinding)
            if binding.run_id and binding.competition_id
        )
        if signature == ownership_signature:
            return
        with ownership_lock:
            if signature == ownership_signature:
                return
            manager.usage.bind_runs(signature)
            ownership_signature = signature

    @router.get('/api/usage')
    def usage(start: float = Query(0, ge=0), end: float | None = Query(None, ge=0),
              run_id: str | None = None, thread_id: str | None = None,
              competition_id: str | None = None, challenge_id: str | None = None,
              model: str | None = None, engine: str | None = None,
              role: str | None = None, workspace_kind: str | None = None,
              generation: int | None = Query(None, ge=0),
              offset: int = Query(0, ge=0), limit: int = Query(100, ge=1, le=500)):
        if end is not None and end < start:
            raise HTTPException(422, "结束时间必须晚于开始时间")
        # Persist ownership so archived/deleted bindings do not erase attribution.
        sync_competition_ownership()
        return manager.usage.query(start=start, end=end, run_id=run_id, thread_id=thread_id,
                                   competition_id=competition_id, challenge_id=challenge_id,
                                   model=model, engine=engine, role=role,
                                   generation=generation, workspace_kind=workspace_kind,
                                   actor_kind='worker' if competition_id else None,
                                   offset=offset, limit=limit)
    @router.post('/api/usage/import-history')
    def import_history():
        return manager.usage.import_run_history()

    @router.get('/api/usage/settings')
    def usage_settings():
        return cursor_account.settings_view()

    @router.put('/api/usage/settings')
    async def update_usage_settings(request: Request):
        body = await request.json()
        value = body.get('cursor_account_usage_enabled') if isinstance(body, dict) else None
        if not isinstance(value, bool):
            raise HTTPException(422, 'cursor_account_usage_enabled 必须是布尔值')
        return cursor_account.update_settings(cursor_account_usage_enabled=value)

    @router.get('/api/usage/cursor-account')
    def cursor_account_usage(days: int = Query(30), tz: str = Query('UTC'), refresh: bool = False):
        """Monthly limits and usage history of this host's Cursor CLI login.

        Reads the macOS Keychain only after the user enables it in settings.
        """
        try:
            window = make_window(days, tz)
        except ValueError as exc:
            raise HTTPException(422, str(exc)) from exc
        payload = cursor_account.read(window, refresh=refresh)
        return {key: value for key, value in payload.items() if not key.startswith("_")}

    @router.get('/api/usage/summary')
    def usage_summary(
        scope: Literal['history', 'muteki'] = 'history', days: str = '30', tz: str = 'UTC',
        refresh: bool = False, start: float | None = Query(None, ge=0),
        end: float | None = Query(None, ge=0), engine: str | None = None,
        run_id: str | None = None, thread_id: str | None = None,
        competition_id: str | None = None, challenge_id: str | None = None,
        model: str | None = None, role: str | None = None, workspace_kind: str | None = None,
        generation: int | None = Query(None, ge=0),
    ):
        """One priced overview for one selected source scope, never overlapping sums."""
        if days == 'all':
            numeric_days, start = 90, 0
        elif days == 'custom':
            numeric_days = 30
        else:
            try:
                numeric_days = int(days)
            except ValueError as exc:
                raise HTTPException(422, '无效时间范围') from exc
        if scope == 'history' and any(value is not None for value in
                (run_id, thread_id, competition_id, challenge_id, role, workspace_kind, generation)):
            raise HTTPException(422, '任务和角色筛选仅适用于 Muteki 内部用量')
        try:
            window = make_usage_window(numeric_days, tz, start=start, end=end)
        except (ValueError, OverflowError, OSError) as exc:
            raise HTTPException(422, str(exc)) from exc
        sync_competition_ownership()
        ledger = manager.usage.query(
            start=start if start is not None else window.since_ms / 1000,
            end=end if end is not None else window.until_ms / 1000,
            run_id=run_id, thread_id=thread_id, competition_id=competition_id,
            challenge_id=challenge_id, model=model, engine=engine, role=role,
            workspace_kind=workspace_kind, generation=generation,
            actor_kind='worker' if competition_id else None,
            limit=2**31 - 1,
        )
        options = dict(refresh=refresh, start=start, end=end, engine=engine, model=model)
        if scope == 'muteki':
            result = provider_usage.summarize_ledger(ledger['records'], numeric_days, tz, **options)
        else:
            result = provider_usage.read(numeric_days, tz, ledger_records=ledger['records'], **options)
        result['scope'] = scope
        result['revision'] = ledger['revision']
        return result

    @router.get('/api/usage/limits')
    def usage_limits(refresh: bool = False):
        """Read actual account windows without creating a model turn."""
        return provider_limits.read(refresh=refresh)

    def legacy_quota_entry(account: dict[str, Any], credential_id: str) -> dict[str, Any]:
        windows = account.get('windows') or []
        tightest = max(windows, key=lambda item: item['used_percent']) if windows else None
        status = account['status']
        return {
            'credential_id': credential_id,
            'account_id': credential_id.removeprefix('account:') if credential_id.startswith('account:') else '',
            'engine': account['engine'], 'label': account['label'],
            'quota_type': 'api_key' if account.get('error_code') == 'api_key' else 'subscription',
            'status': 'ok' if status == 'ok' and tightest else 'not_supported' if status == 'unsupported' else 'unknown',
            'remaining': 100 - tightest['used_percent'] if tightest else None,
            'total': 100 if tightest else None,
            'window_seconds': tightest['window_seconds'] if tightest else None,
            'reset_at': tightest['reset_at'] if tightest else None,
            'updated_at': account.get('updated_at'), 'unknown_reason': account.get('error'),
            'windows': windows, 'stale': account.get('stale', False),
        }

    @router.get('/api/usage/quota')
    def get_quota() -> Any:
        """Compatibility view for conversation drawers, backed by live multi-window probes."""
        result = provider_limits.read()
        return {'as_of': result['as_of'], 'quota': [
            legacy_quota_entry(account, identifier)
            for account in result['accounts']
            for identifier in account.get('credential_ids', [account['id']])
        ]}

    @router.post('/api/usage/quota/{credential_id}/refresh')
    def refresh_quota(credential_id: str) -> Any:
        from muteki.solver.credential_accounts import canonical_credential_id
        try:
            identifier = canonical_credential_id(credential_id)
        except ValueError as exc:
            raise HTTPException(400, str(exc)) from exc
        try:
            result = provider_limits.read(refresh=True, credential_id=identifier)
        except ValueError as exc:
            raise HTTPException(404, '额度账户不存在') from exc
        for account in result['accounts']:
            if identifier in account.get('credential_ids', [account['id']]):
                return {'ok': account['status'] == 'ok', 'credential_id': identifier,
                        'entry': legacy_quota_entry(account, identifier)}
        raise HTTPException(404, '额度账户不存在')

    return router
