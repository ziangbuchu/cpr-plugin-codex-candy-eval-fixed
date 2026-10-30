#!/usr/bin/env python3
"""Run on the target host only.

Triggers one real test through the official CPR plugin management route and
reports an aggregate-only result: statuses, counts, timings and token totals.
Account identifiers, credentials, admin keys and model output text are never
printed; error strings pass through a category-level redaction first.

Usage: python3 remote-verify-run.py [candy|fingerprint|modeltrace|pelican] [model] [effort]
"""
import json
import re
import ssl
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

BASE = Path('/opt/codex-proxy-rs')
PG = 'codex-proxy-rs-postgres-1'
PLUGIN = 'nmbb.codex-candy-eval'
CONTEXT = ssl._create_unverified_context()
POLL_LIMIT_SECONDS = 420

# 插件路由名 -> (管理路由, 状态中该套件的历史字段, 附加请求体)
SUITES = {
    'candy': ('api/run', 'results', {'runs': 1}),
    'fingerprint': ('api/fingerprint/run', 'fingerprints', {'mode': 'quick', 'concurrency': 1}),
    'modeltrace': ('api/modeltrace/run', 'modeltraces', {'concurrency': 1}),
    'pelican': ('api/pelican/run', 'pelicans', {}),
}

EMAIL = re.compile(r'[\w.+-]+@[\w-]+\.[\w.-]+')
UUID = re.compile(r'\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b')
TOKEN = re.compile(r'\b[A-Za-z0-9_\-]{20,}\b')
IPV4 = re.compile(r'\b\d{1,3}(?:\.\d{1,3}){3}\b')
IPV6 = re.compile(r'\b(?:[0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F]{0,4}\b')
URL = re.compile(r'https?://\S+')


def redact(text):
    """Category-level detail only; never echo raw upstream or identity text."""
    if not text:
        return ''
    for pattern, label in ((EMAIL, 'email'), (UUID, 'uuid'), (URL, 'url'),
                           (IPV4, 'ipv4'), (IPV6, 'ipv6'), (TOKEN, 'token')):
        text = pattern.sub('<redacted-{0}>'.format(label), text)
    return text[:240]


def pg_command(tool, arguments, **options):
    # The rotated password is authoritative in the gateway config, not container env.
    config = (BASE / 'deploy/config.yaml').read_text()
    match = re.search(r"password:\s*&postgres_password\s*'([^']+)'", config)
    if not match:
        raise RuntimeError('database_password_not_found')
    return subprocess.run(['docker', 'exec', '-i', PG, 'sh', '-c',
        'IFS= read -r PGPASSWORD; export PGPASSWORD; exec "$@" -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB"',
        'sh', tool, *arguments], input=(match.group(1) + '\n').encode(),
        check=True, timeout=120, **options)


def credentials():
    source = Path('/usr/local/bin/cpr-candy-inspect.sh').read_text()
    match = re.search(r'https://127\.0\.0\.1:\d+', source)
    if not match:
        raise RuntimeError('local_endpoint_not_found')
    result = pg_command('psql', ['-tAc', 'SELECT admin_api_key FROM runtime_settings;'], capture_output=True)
    key = result.stdout.decode().strip()
    if not key:
        raise RuntimeError('admin_key_unavailable')
    return match.group(), key


def api(path, method='GET', payload=None):
    url, key = credentials()
    body = json.dumps(payload).encode() if payload is not None else None
    headers = {'x-api-key': key}
    if body is not None:
        headers['Content-Type'] = 'application/json'
    request = urllib.request.Request(url + path, data=body, method=method, headers=headers)
    with urllib.request.urlopen(request, context=CONTEXT, timeout=180) as response:
        data = json.load(response)
    if isinstance(data, dict) and 'code' in data:
        if data['code'] not in (0, 200):
            raise RuntimeError('api_envelope_error')
        return data['data']
    return data


def active_instance():
    artifacts = api('/api/admin/plugins/artifacts')
    digests = {a['metadata']['sha256'] for a in artifacts if a['metadata']['pluginId'] == PLUGIN}
    matches = [i for i in api('/api/admin/plugins/instances') if i['artifactSha256'] in digests and i['enabled']]
    if len(matches) != 1:
        raise RuntimeError('active_instance_not_unique')
    return matches[0]


def management(instance, route, method='GET', payload=None):
    # CPR 的插件管理路由模板是 `.../{revision}/api/{*path}`，而插件自身注册的路由名
    # 形如 `api/state`，因此真实 URL 是 `.../api/api/state`。
    base = '/api/admin/plugins/extensions/{0}/{1}/{2}/api/'.format(
        instance['id'], instance['artifactSha256'], instance['revision'])
    return api(base + route, method, payload)


def accounts(value):
    """`api/state` 返回 {"auths":[{...,"results":[...]}],...}；只提取账号条目。"""
    if isinstance(value, dict) and isinstance(value.get('auths'), list):
        return [entry for entry in value['auths'] if isinstance(entry, dict)]
    return []


def history(entry, key):
    records = entry.get(key)
    return records if isinstance(records, list) else []


def newest(entry, key):
    records = history(entry, key)
    if not records:
        return None
    return max(records, key=lambda item: str(item.get('time', '')))


def summarise(record):
    """Schema-level summary: field names and magnitudes, never payload text."""
    error = str(record.get('error', ''))
    if record.get('skipped'):
        status = 'skipped'
    elif record.get('ok'):
        status = 'ok'
    elif error:
        status = 'error'
    else:
        status = 'unknown'
    return {
        'status': status,
        'numbers': {k: v for k, v in sorted(record.items()) if isinstance(v, int)},
        'string_chars': {k: len(v) for k, v in sorted(record.items()) if isinstance(v, str)},
        'keys': sorted(record),
        'error': redact(error),
    }


def timeline(entry, key, limit=3):
    records = sorted(history(entry, key), key=lambda item: str(item.get('time', '')), reverse=True)
    return [summarise(record) for record in records[:limit]]


def main():
    suite = sys.argv[1] if len(sys.argv) > 1 else 'candy'
    model = sys.argv[2] if len(sys.argv) > 2 else 'gpt-6.1-sol'
    effort = sys.argv[3] if len(sys.argv) > 3 else 'low'
    route, key, extra = SUITES[suite]
    if suite == 'modeltrace' and len(sys.argv) > 4:
        extra = {**extra, 'concurrency': int(sys.argv[4])}

    instance = active_instance()
    before = accounts(management(instance, 'api/state'))
    baseline = {entry.get('id'): str((newest(entry, key) or {}).get('time', '')) for entry in before}

    payload = {'model': model, 'effort': effort, 'all': True, **extra}
    started = management(instance, route, 'POST', payload)
    print(json.dumps({'phase': 'submitted', 'suite': suite, 'started': started,
                      'accounts': len(before)}), flush=True)

    deadline = time.monotonic() + POLL_LIMIT_SECONDS
    attempts = 0
    while True:
        attempts += 1
        state = accounts(management(instance, 'api/state'))
        pending_ids = [entry.get('id') for entry in state
                       if str((newest(entry, key) or {}).get('time', '')) <= baseline.get(entry.get('id'), '')]
        if state and not pending_ids:
            break
        if time.monotonic() >= deadline:
            print(json.dumps({'phase': 'timeout', 'suite': suite, 'attempts': attempts,
                              'pending_accounts': len(pending_ids)}))
            return
        time.sleep(5)

    print(json.dumps({'phase': 'result', 'suite': suite, 'attempts': attempts, 'accounts': len(state),
        'per_account': [{'records': len(history(entry, key)), 'newest_first': timeline(entry, key)}
                        for entry in state]}))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:  # Aggregate failure category only.
        print(json.dumps({'phase': 'failed', 'error': type(error).__name__, 'detail': redact(str(error))}))
        raise SystemExit(1)
