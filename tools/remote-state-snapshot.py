#!/usr/bin/env python3
"""Run on the target host only.

Read-only snapshot of the plugin's own state: counts, verdicts, progress flags and
timestamps. Never prints account identifiers, credentials or provider metadata.

Usage: python3 remote-state-snapshot.py [candy|fingerprint|modeltrace|pelican]
"""
import json
import sys

import remote_verify_run as verify


def main():
    suite = sys.argv[1] if len(sys.argv) > 1 else 'candy'
    _, key, _ = verify.SUITES[suite]
    instance = verify.active_instance()
    entries = verify.accounts(verify.management(instance, 'api/state'))
    rows = []
    for entry in entries:
        latest = verify.newest(entry, key)
        progress = entry.get(suite + '_progress') if suite != 'candy' else entry.get('progress')
        sample_shape = None
        if isinstance(latest, dict) and 'samples' in latest:
            samples = latest['samples']
            first = samples[0] if isinstance(samples, list) and samples else None
            sample_shape = {
                'type': type(samples).__name__,
                'len': len(samples) if isinstance(samples, (list, dict)) else None,
                'item_type': type(first).__name__ if first is not None else None,
                'item_keys': sorted(first) if isinstance(first, dict) else None,
            }
        rows.append({
            'records': len(verify.history(entry, key)),
            'latest_time': (latest or {}).get('time'),
            'latest': verify.summarise(latest) if latest else None,
            'running_flags': {k: v for k, v in entry.items() if isinstance(v, bool) and 'running' in k},
            'progress': {k: progress.get(k) for k in ('phase', 'done', 'total')} if isinstance(progress, dict) else None,
            'sample_shape': sample_shape,
        })
    print(json.dumps({'revision': instance['revision'], 'accounts': len(entries), 'suite': suite, 'rows': rows}, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    sys.exit(main())
