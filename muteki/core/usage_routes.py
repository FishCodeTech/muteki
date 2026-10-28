"""Recover missing historical usage routes from invocation-scoped evidence only."""
from __future__ import annotations

import json
import os
from pathlib import Path
from urllib.parse import urlparse


def _cloud_provider(endpoint: str) -> str | None:
    # Ollama local/self-hosted endpoints do not share the cloud tariff.
    try:
        parsed = urlparse(endpoint if '://' in endpoint else 'https://' + endpoint)
        return 'ollama-cloud' if parsed.scheme == 'https' and parsed.hostname == 'ollama.com' and parsed.port in (None, 443) else None
    except ValueError:
        return None


def staged_pi_provider(agent_state_dir: str | Path, model: str) -> str | None:
    """Read only public routing fields from the invocation's staged model file."""
    path = Path(agent_state_dir) / 'models.json'
    try:
        document = json.loads(path.read_text())
        matches = []
        for provider in document.get('providers', {}).values():
            if not isinstance(provider, dict):
                continue
            if any(isinstance(entry, dict) and entry.get('id') == model for entry in provider.get('models', [])):
                matches.append(_cloud_provider(str(provider.get('baseUrl') or '')))
        return matches[0] if len(matches) == 1 else None
    except (OSError, ValueError, TypeError, AttributeError):
        return None


def recover_usage_routes(rows: list[dict], state_root: str | Path) -> list[dict]:
    """Return enriched copies, preserving original identities and provenance.

    Worker lifecycle receipts must match worker/model/generation. Pi Decide uses
    its own staged configuration only when its mtime falls inside that exact
    invocation and the persisted run contains one execution generation.
    """
    from muteki.core.provider_usage import canonical_engine
    root = Path(state_root)
    sessions = Path(os.environ.get('MUTEKI_SESSIONS_ROOT') or root.parent / 'sessions')
    logs = {}
    result = []
    for original in rows:
        row = original
        run = row.get('run_id')
        if row.get('catalog_provider') or not isinstance(run, str) or Path(run).name != run or canonical_engine(row.get('engine')) != 'pi':
            result.append(row)
            continue
        if run not in logs:
            events, generations = [], set()
            try:
                with (root / f'{run}.jsonl').open() as stream:
                    for line in stream:
                        event = json.loads(line)
                        payload = event.get('payload') or {}
                        generation = payload.get('execution_generation')
                        if generation is not None:
                            generations.add(generation)
                        if event.get('event_type') in ('worker.lifecycle', 'worker.status'):
                            events.append((event, payload))
            except (OSError, ValueError, TypeError, AttributeError):
                events, generations = [], set()
            logs[run] = events, generations
        events, generations = logs[run]
        provenance = None
        if row.get('source') == 'pi-decide' and row.get('worker_id') == 'reason':
            config = sessions / run / 'workspace' / '.muteki-agent-state' / 'reason' / 'pi-agent' / 'models.json'
            try:
                start = int(str(row.get('id') or '').removeprefix(f'pi-decide:{run}:')) / 1000
                exact_invocation = str(row.get('id') or '').startswith(f'pi-decide:{run}:')
                valid_time = start <= config.stat().st_mtime <= float(row['at'])
                if (exact_invocation and valid_time and generations == {row.get('generation')}
                        and staged_pi_provider(config.parent, row.get('model')) == 'ollama-cloud'):
                    provenance = {'source': 'invocation_staged_model', 'evidence': str(config.relative_to(sessions)),
                                  'generation': row.get('generation'), 'config_mtime': config.stat().st_mtime}
            except (OSError, ValueError, TypeError, KeyError):
                pass
        else:
            matches = [(event, payload) for event, payload in events
                       if event.get('solver_id') == row.get('worker_id')
                       and payload.get('execution_generation') == row.get('generation')
                       and payload.get('model') == row.get('model')
                       and canonical_engine(payload.get('engine')) == 'pi'
                       and isinstance(event.get('ts'), (int, float)) and event['ts'] <= row.get('at', 0)
                       and payload.get('endpoint_host')]
            providers = {_cloud_provider(payload['endpoint_host']) for _, payload in matches}
            accounts = {payload.get('account_id') for _, payload in matches if payload.get('account_id')}
            if providers == {'ollama-cloud'}:
                event, payload = matches[-1]
                provenance = {'source': 'run_worker_lifecycle', 'evidence': f'{run}.jsonl',
                              'event_seq': event.get('seq'), 'generation': row.get('generation')}
                if not row.get('credential_id') and len(accounts) == 1:
                    from muteki.solver.credential_accounts import canonical_credential_id
                    try:
                        provenance['credential_id'] = canonical_credential_id('account:' + next(iter(accounts)))
                    except ValueError:
                        pass
        if provenance:
            row = dict(original)
            row['route_provenance'] = {**provenance, 'captured_identity': {
                key: original.get(key) for key in ('engine', 'model', 'provider_id', 'credential_id')}}
            row['catalog_provider'] = 'ollama-cloud'
            if provenance.get('credential_id'):
                row['credential_id'] = provenance['credential_id']
        result.append(row)
    return result


def current_pricing_route(item: dict, state_root: str | Path) -> dict:
    """Resolve a currently selected account for a picker quote, never history."""
    from muteki.solver.credential_accounts import account_id_from_credential_id, account_store_root
    from muteki.core.provider_usage import canonical_engine
    if canonical_engine(item.get('engine')) != 'pi':
        return item
    account = account_id_from_credential_id(item.get('credential_id'))
    if not account:
        return item
    base = account_store_root(state_root) / account
    try:
        public_provider = (base / 'PROVIDER').read_text().strip()
        if item.get('provider_id') and item['provider_id'] != public_provider:
            return item
        if _cloud_provider((base / 'BASE_URL').read_text().strip()) == 'ollama-cloud':
            return {**item, 'catalog_provider': 'ollama-cloud',
                    'route_provenance': {'source': 'current_selected_account_endpoint'}}
    except OSError:
        pass
    return item
