#!/usr/bin/env python3
"""Run on the target host only.

Cancels an in-flight fingerprint/modelTrace run and waits until the host clears
its progress flag. Aggregate output only: counts and phase, never identifiers.

Usage: python3 remote-cancel-suite.py [fingerprint|modeltrace]
"""
import json
import sys
import time

import remote_verify_run as verify

SUITES = {
    'fingerprint': ('api/fingerprint/cancel', 'fingerprints', 'fingerprint_running'),
    'modeltrace': ('api/modeltrace/cancel', 'modeltraces', 'modeltrace_running'),
}


def main():
    suite = sys.argv[1] if len(sys.argv) > 1 else 'fingerprint'
    route, key, flag = SUITES[suite]
    instance = verify.active_instance()

    targets = [entry.get('id') for entry in verify.accounts(verify.management(instance, 'api/state'))
               if entry.get(flag)]
    print(json.dumps({'phase': 'cancel', 'suite': suite, 'targets': len(targets)}), flush=True)
    for target in targets:
        verify.management(instance, route, 'POST', {'id': target})

    deadline = time.monotonic() + 180
    while True:
        state = verify.accounts(verify.management(instance, 'api/state'))
        still = [entry for entry in state if entry.get(flag)]
        if not still:
            break
        if time.monotonic() >= deadline:
            print(json.dumps({'phase': 'timeout', 'suite': suite, 'still_running': len(still)}))
            return
        time.sleep(5)

    print(json.dumps({'phase': 'cancelled', 'suite': suite, 'accounts': len(state),
        'remaining': [{'phase': (entry.get(flag) or {}).get('phase'),
                       'done': (entry.get(flag) or {}).get('done'),
                       'total': (entry.get(flag) or {}).get('total')} for entry in state],
        'records': [len(verify.history(entry, key)) for entry in state]}))


if __name__ == '__main__':
    try:
        main()
    except Exception as error:  # Aggregate failure category only.
        print(json.dumps({'phase': 'failed', 'error': type(error).__name__,
                          'detail': verify.redact(str(error))}))
        raise SystemExit(1)
