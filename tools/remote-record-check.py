#!/usr/bin/env python3
"""Run on the target host only.

Fetches the newest record of one suite through the same management route the page
uses (including its query string) and reports the transport result only: status,
field names and magnitudes. Never prints record payload text.

Usage: python3 remote-record-check.py [candy|fingerprint|modeltrace|pelican]
"""
import json
import sys
import urllib.parse

import remote_verify_run as verify


def main():
    suite = sys.argv[1] if len(sys.argv) > 1 else 'modeltrace'
    _, key, _ = verify.SUITES[suite]
    instance = verify.active_instance()
    entries = verify.accounts(verify.management(instance, 'api/state'))
    if not entries:
        print(json.dumps({'phase': 'no_account'}))
        return 1
    entry = entries[0]
    latest = verify.newest(entry, key)
    if latest is None:
        print(json.dumps({'phase': 'no_record'}))
        return 1
    query = urllib.parse.urlencode({'auth_id': entry.get('id', ''), 'id': latest.get('id', '')})
    try:
        record = verify.management(instance, 'api/' + suite + '/record?' + query)
    except Exception as error:  # noqa: BLE001 - diagnostics only
        print(json.dumps({
            'phase': 'failed',
            'error_class': type(error).__name__,
            'http_status': getattr(error, 'code', None),
            'detail': verify.redact(str(error))[:200],
        }, ensure_ascii=False))
        return 1
    print(json.dumps({
        'phase': 'ok',
        'record_keys': sorted(record) if isinstance(record, dict) else None,
        'record_type': type(record).__name__,
        'summary': verify.summarise(record) if isinstance(record, dict) else None,
    }, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    sys.exit(main())
