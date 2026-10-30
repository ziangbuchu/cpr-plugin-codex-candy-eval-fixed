#!/usr/bin/env python3
"""Run on the target host only; output aggregate JSON, never configs or API bodies.

Installs through the official fixed-hash artifact and revision-checked version API.
Retains an accepted old artifact plus a private pg_dump and plugin-store archive.
No files are deleted and no gateway/container/network settings are changed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import ssl
import subprocess
import tarfile
from datetime import datetime, timezone
import urllib.error
import urllib.request

BASE = Path('/opt/codex-proxy-rs')
PG = 'codex-proxy-rs-postgres-1'
PLUGIN = 'nmbb.codex-candy-eval'
CONTEXT = ssl._create_unverified_context()  # Existing same-host TLS endpoint only.


def pg_command(tool, arguments, **options):
    # The rotated password is authoritative in the gateway config, not container env.
    # Pass it only through stdin to a target-side process, never argv or output.
    config = (BASE / 'deploy/config.yaml').read_text()
    match = re.search(r"password:\s*&postgres_password\s*'([^']+)'", config)
    if not match:
        raise RuntimeError('database_password_not_found')
    return subprocess.run(['docker', 'exec', '-i', PG, 'sh', '-c',
        'IFS= read -r PGPASSWORD; export PGPASSWORD; exec "$@" -h 127.0.0.1 -U "$POSTGRES_USER" -d "$POSTGRES_DB"',
        'sh', tool, *arguments], input=(match.group(1) + '\n').encode(),
        check=True, timeout=120, **options)


def db(sql):
    result = pg_command('psql', ['-tAc', sql], capture_output=True)
    return result.stdout.decode().strip()


def credentials():
    source = Path('/usr/local/bin/cpr-candy-inspect.sh').read_text()
    match = re.search(r'https://127\.0\.0\.1:\d+', source)
    if not match:
        raise RuntimeError('local_endpoint_not_found')
    key = db('SELECT admin_api_key FROM runtime_settings;')
    if not key:
        raise RuntimeError('admin_key_unavailable')
    return match.group(), key


def api(path, method='GET', payload=None, raw=False):
    url, key = credentials()
    if payload is not None and not raw:
        payload = json.dumps(payload).encode()
    headers = {'x-api-key': key}
    if payload is not None:
        headers['Content-Type'] = 'application/octet-stream' if raw else 'application/json'
    request = urllib.request.Request(url + path, data=payload, method=method, headers=headers)
    with urllib.request.urlopen(request, context=CONTEXT, timeout=180) as response:
        data = json.load(response)
    if isinstance(data, dict) and 'code' in data:
        if data['code'] not in (0, 200):
            raise RuntimeError('api_envelope_error')
        return data['data']
    return data


def current():
    artifacts = api('/api/admin/plugins/artifacts')
    identities = {a['metadata']['sha256'] for a in artifacts if a['metadata']['pluginId'] == PLUGIN}
    matches = [i for i in api('/api/admin/plugins/instances') if i['artifactSha256'] in identities and i['enabled']]
    if len(matches) != 1:
        raise RuntimeError('active_instance_not_unique')
    return matches[0]


def switch(instance, digest):
    return api('/api/admin/plugins/instances/switch-version', 'POST',
        {'id': instance['id'], 'target': {'artifactSha256': digest, 'expectedRevision': instance['revision']}})


def require_idle(instance):
    state = api('/api/admin/plugins/extensions/{}/{}/{}/api/api/state'.format(
        instance['id'], instance['artifactSha256'], instance['revision']))
    rows = state.get('auths') if isinstance(state, dict) else None
    if not isinstance(rows, list) or not all(isinstance(row, dict) for row in rows):
        raise RuntimeError('plugin_state_schema_unrecognized')
    if any(bool(value) for row in rows for key, value in row.items()
           if key == 'running' or key.endswith('_running')):
        raise RuntimeError('tests_in_progress')

def private_backup(instance):
    stamp = datetime.now(timezone.utc).strftime('%Y%m%dT%H%M%SZ')
    backup = Path('/root/cpr-candy-backups') / ('ui-' + stamp)
    backup.parent.mkdir(mode=0o700, exist_ok=True)
    backup.parent.chmod(0o700)
    backup.mkdir(mode=0o700)
    dump = backup / 'database.dump'
    with dump.open('xb') as output:
        dump.chmod(0o600)
        pg_command('pg_dump', ['-Fc'], stdout=output, stderr=subprocess.PIPE)
    with dump.open('rb') as source:
        result = subprocess.run(['docker', 'exec', '-i', PG, 'pg_restore', '--list'],
            stdin=source, capture_output=True, check=True, timeout=30)
    if not result.stdout or dump.stat().st_size == 0:
        raise RuntimeError('backup_unreadable')
    archive = backup / 'plugin-store.tar.gz'
    with archive.open('xb') as output:
        archive.chmod(0o600)
        with tarfile.open(fileobj=output, mode='w:gz') as tar:
            tar.add(BASE / '.runtime/data/plugins', arcname='plugins', recursive=True)
    snapshot = backup / 'instance.json'
    with snapshot.open('x') as output:
        snapshot.chmod(0o600)
        json.dump(instance, output)
    script = backup / 'inspect.sh'
    shutil.copyfile('/usr/local/bin/cpr-candy-inspect.sh', script)
    script.chmod(0o600)
    return backup


def package_version(package):
    # Release version is read from the official artifact name so the deployed
    # identity check never depends on a hand-edited constant.
    match = re.fullmatch(
        r'nmbb\.codex-candy-eval-(\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?)-x86_64-unknown-linux-gnu\.tar\.gz',
        package.name,
    )
    if not match:
        raise RuntimeError('unrecognized_package_name')
    return match.group(1)


def deploy(package, expected, allow_inflight=False):
    if package.is_symlink() or not package.is_file() or not re.fullmatch(r'[a-f0-9]{64}', expected):
        raise RuntimeError('invalid_package_input')
    version = package_version(package)
    body = package.read_bytes()
    if hashlib.sha256(body).hexdigest() != expected:
        raise RuntimeError('package_hash_mismatch')
    previous = current()
    if not allow_inflight:
        require_idle(previous)
    else:
        print(json.dumps({'phase': 'authorization', 'inflight_interruption_authorized': True}), flush=True)
    verified = api('/api/admin/plugins/artifacts/upload/verify', 'POST', body, raw=True)
    metadata = verified['metadata']
    if metadata['pluginId'] != PLUGIN or metadata['version'] != version or metadata['sha256'] != expected:
        raise RuntimeError('verified_identity_mismatch')
    backup = private_backup(previous)
    print(json.dumps({'phase': 'backup', 'private_backup': str(backup), 'backup_readable': True}), flush=True)
    paused = False
    changed = False
    try:
        subprocess.run(['systemctl', 'stop', 'cpr-candy-inspect.timer'], check=True, capture_output=True)
        paused = True
        installed = api('/api/admin/plugins/artifacts/upload?sha256=' + expected, 'POST', body, raw=True)
        if installed['artifact']['metadata']['sha256'] != expected or installed['defaultInstanceId'] is not None:
            raise RuntimeError('installation_result_mismatch')
        latest = current()
        if latest['id'] != previous['id'] or latest['artifactSha256'] != previous['artifactSha256']:
            raise RuntimeError('concurrent_instance_change')
        if not allow_inflight:
            require_idle(latest)  # Default: never interrupt a test started during backup/upload.
        changed = True  # Also reconcile a lost response after a committed switch.
        switch(latest, expected)
        running = current()
        if not running['running'] or running['artifactSha256'] != expected or running['loadError']:
            raise RuntimeError('publication_check_failed')
        extensions = api('/api/admin/plugins/extensions')
        extension = next(e for e in extensions if e['target']['instanceId'] == running['id'])
        print(json.dumps({'phase': 'published', 'version': version, 'same_instance': running['id'] == previous['id'],
            'revision': extension['target']['revision'], 'running': running['running'],
            'resources': len(extension['resources']), 'routes': len(extension['routes']),
            'compatibility_warning': bool(running['compatibilityWarning']), 'load_error': bool(running['loadError'])}), flush=True)
    except Exception:
        if changed:
            active = current()
            if active['id'] == previous['id'] and active['artifactSha256'] == expected:
                switch(active, previous['artifactSha256'])
                print(json.dumps({'phase': 'rollback', 'old_artifact_restored': current()['artifactSha256'] == previous['artifactSha256']}), flush=True)
        raise
    finally:
        if paused:
            subprocess.run(['systemctl', 'start', 'cpr-candy-inspect.timer'], capture_output=True, check=True)


if __name__ == '__main__':
    os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument('package', type=Path)
    parser.add_argument('sha256')
    parser.add_argument('--allow-inflight', action='store_true',
        help='Allow interruption of running tests only with explicit operator authorization')
    arguments = parser.parse_args()
    try:
        deploy(arguments.package, arguments.sha256, allow_inflight=arguments.allow_inflight)
    except Exception as error:
        print(json.dumps({'ok': False, 'error_class': type(error).__name__, 'http_status': error.code if isinstance(error, urllib.error.HTTPError) else None}), flush=True)
        raise SystemExit(1)
