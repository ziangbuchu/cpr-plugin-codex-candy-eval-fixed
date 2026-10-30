#!/usr/bin/env python3
"""Run on the target host only.

Reloads the candy-eval plugin instance through the official instance-save route,
which quiesces and restarts the child process. That is how an in-flight
fingerprint/modelTrace run is abandoned safely: the plugin keeps only persisted
history, so interrupted runs disappear without touching other services.

Only identity-safe fields are echoed back; `secrets` is omitted on purpose so
existing secret values are preserved. Aggregate output only.

Usage: python3 remote-reload-instance.py
"""
import json
import time

import remote_verify_run as verify

PLUGIN = verify.PLUGIN


def target_instance():
    artifacts = verify.api('/api/admin/plugins/artifacts')
    digests = {a['metadata']['sha256'] for a in artifacts if a['metadata']['pluginId'] == PLUGIN}
    matches = [i for i in verify.api('/api/admin/plugins/instances') if i['artifactSha256'] in digests]
    if len(matches) != 1:
        raise RuntimeError('plugin_instance_not_unique')
    return matches[0]


def main():
    instance = target_instance()
    payload = {
        'id': instance['id'],
        'instance': {
            'expectedRevision': instance['revision'],
            'replaceInstances': [],
            'name': instance['name'],
            'artifactSha256': instance['artifactSha256'],
            'enabled': instance['enabled'],
            'configuration': instance['configuration'],
            'bindings': instance['bindings'],
            # secrets 省略 => 保留既有密文，不做任何凭据变更。
        },
    }
    result = verify.api('/api/admin/plugins/instances/update', 'POST', payload)
    print(json.dumps({'phase': 'reloaded', 'previous_revision': instance['revision'],
                      'config_revision': result.get('configRevision')}), flush=True)

    deadline = time.monotonic() + 180
    while True:
        current = target_instance()
        status = (current.get('runtime') or {}).get('status')
        if current.get('running') and status == 'running':
            break
        if time.monotonic() >= deadline:
            print(json.dumps({'phase': 'timeout', 'running': bool(current.get('running')),
                              'status': status}))
            return
        time.sleep(3)

    state = verify.accounts(verify.management(current, 'api/state'))
    print(json.dumps({'phase': 'ready', 'revision': current['revision'], 'status': status,
        'running': bool(current['running']),
        'load_error': bool(current.get('loadError')),
        'compatibility_warning': bool(current.get('compatibilityWarning')),
        'accounts': len(state),
        'in_flight': [bool(entry.get('fingerprint_running')) or bool(entry.get('modeltrace_running'))
                      or bool(entry.get('running')) for entry in state],
        'persisted': [{'results': len(verify.history(entry, 'results')),
                       'fingerprints': len(verify.history(entry, 'fingerprints')),
                       'modeltraces': len(verify.history(entry, 'modeltraces')),
                       'pelicans': len(verify.history(entry, 'pelicans'))} for entry in state]}))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:  # Aggregate failure category only.
        print(json.dumps({'phase': 'failed', 'error': type(error).__name__,
                          'detail': verify.redact(str(error))}))
        raise SystemExit(1)
