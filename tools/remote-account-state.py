#!/usr/bin/env python3
"""Run on the target host only.

Read-only probe of the host account directory: prints only whitelisted scalar
status fields (booleans, short enums, numbers) plus the observed field names.
Never prints identifiers, credentials, addresses or payload text.

Usage: python3 remote-account-state.py
"""
import json
import sys

import remote_verify_run as verify

SAFE_KEYS = {
    'enabled', 'disabled', 'status', 'state', 'available', 'unavailable', 'healthy',
    'cooldown', 'cooldown_until', 'cooldown_ms', 'cooldown_seconds', 'penalty',
    'penalty_until', 'penalty_ms', 'quota', 'quota_remaining', 'remaining',
    'remaining_quota', 'model_access', 'model_access_mode', 'models_count',
    'model_count', 'provider', 'provider_id', 'provider_kind', 'kind', 'type',
    'plan', 'plan_type', 'authentication_kind', 'auth_kind', 'in_flight',
    'in_flight_count', 'success_count', 'failure_count', 'request_count',
    'rate_limited', 'expired', 'expires_at', 'reset_at', 'last_error_at',
}
MAX_ENUM = 24


def safe_scalar(value):
    if isinstance(value, bool) or isinstance(value, (int, float)):
        return value
    if isinstance(value, str) and len(value) <= MAX_ENUM:
        return value
    return None


def scrub(account):
    if not isinstance(account, dict):
        return {'type': type(account).__name__}
    out = {}
    for key, value in account.items():
        if key not in SAFE_KEYS:
            continue
        scalar = safe_scalar(value)
        if scalar is not None:
            out[key] = scalar
    out['_keys'] = sorted(account)
    # 嵌套结构只报告形状与短枚举，不报告其中的标识或正文。
    for key, value in account.items():
        if not isinstance(value, dict):
            continue
        shape = {'keys': sorted(value)}
        scalars = {k: safe_scalar(v) for k, v in value.items()}
        shape['scalars'] = {k: v for k, v in scalars.items() if v is not None}
        shape['list_lengths'] = {k: len(v) for k, v in value.items() if isinstance(v, list)}
        out['_' + key] = shape
    return out


def main():
    payload = verify.api('/api/admin/accounts')
    items = None
    for container in (payload, payload.get('data') if isinstance(payload, dict) else None):
        if isinstance(container, list):
            items = container
        elif isinstance(container, dict):
            for key in ('items', 'accounts', 'list', 'data'):
                if isinstance(container.get(key), list):
                    items = container[key]
                    break
        if items is not None:
            break
    if not isinstance(items, list):
        print(json.dumps({
            'phase': 'unexpected_shape',
            'top_keys': sorted(payload) if isinstance(payload, dict) else None,
            'data_type': type(items).__name__,
            'data_keys': sorted(items) if isinstance(items, dict) else None,
        }, ensure_ascii=False))
        return 1
    print(json.dumps({
        'phase': 'ok',
        'count': len(items),
        'summary': scrub(payload.get('summary')) if isinstance(payload, dict) else None,
        'accounts': [scrub(item) for item in items],
    }, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    sys.exit(main())
