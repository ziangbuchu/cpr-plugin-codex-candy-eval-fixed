#!/usr/bin/env python3
"""Temporary same-host, loopback-only browser validation proxy.

Uses the existing deployment admin API Key exclusively on the target host.
The local frontend auth guard is bootstrapped only after real server validation;
this is NOT cookie-login verification. Plugin resources, bridge calls and results
remain live. API identities are redacted before leaving the target. No HTTP logs.
"""
import http.cookiejar
import http.server
import json
import os
from pathlib import Path
import re
import signal
import ssl
import threading
import urllib.error
import urllib.parse
import urllib.request
from importlib.util import spec_from_file_location, module_from_spec

spec = spec_from_file_location('ops', Path(__file__).with_name('update.py'))
ops = module_from_spec(spec)
spec.loader.exec_module(ops)
upstream, admin_key = ops.credentials()
jar = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=ssl._create_unverified_context()), urllib.request.HTTPCookieProcessor(jar))
# Fail closed if the deployment Key cannot authenticate to the real admin API.
# Do not read or retry an obsolete initial password; do not alter real sessions.
try:
    ops.current()
except Exception as error:
    print(json.dumps({'ready': False, 'error_class': type(error).__name__}), flush=True)
    raise SystemExit(1)
aliases = {}
lock = threading.RLock()
UUID = re.compile(r'\b[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\b', re.I)
EMAIL = re.compile(r'[\w.+-]+@[\w.-]+\.[a-zA-Z]{2,}')
DOMAIN = re.compile(r'(?<![\w/])(?:[a-zA-Z0-9-]+\.)+[a-zA-Z]{2,}(?::\d+)?')
IPV4 = re.compile(r'\b(?:\d{1,3}\.){3}\d{1,3}\b')
IPV6 = re.compile(r'(?<!\w)(?:[0-9a-fA-F]{0,4}:){2,}[0-9a-fA-F:.%]*(?!\w)')
SECRET_FIELD = re.compile(r'(?:password|secret|token|cookie|authorization|api[_-]?key)', re.I)
IDENTITY_FIELD = re.compile(r'^(?:email|username|login|account_?name|base_?url|proxy_?url)$', re.I)


def alias(value):
    with lock:
        if value in aliases.values():
            return value
        # Keep UUID syntax for the host's route validator, never real identities.
        return aliases.setdefault(value, '00000000-0000-4000-8000-' + format(len(aliases) + 1, '012x'))


def sanitise(value, field=''):
    if isinstance(value, dict):
        account_record = ('provider' in value and 'id' in value) or any(k in value for k in ('accountId', 'account_id'))
        public_page = 'entry' in value and 'title' in value and 'id' in value
        return {k: v if public_page and k == 'id' else sanitise(v, 'account_name' if account_record and k == 'name' else k) for k, v in value.items()}
    if isinstance(value, list):
        return [sanitise(v, field) for v in value]
    if not isinstance(value, str):
        return value
    # This is a fixed public package identifier, not a host/domain/client ID.
    if value == ops.PLUGIN:
        return value
    # Declared web resource names are public build artifacts, not domains.
    if field in ('path', 'entry') and re.fullmatch(r'web/[A-Za-z0-9_./-]+', value) and '..' not in value.split('/'):
        return value
    if field.lower() in ('id', 'credential_id', 'auth_id', 'account_id', 'accountid', 'client_key_id') and value:
        return alias(value)
    if SECRET_FIELD.search(field) and value:
        return '<redacted-secret>'
    if IDENTITY_FIELD.match(field) and value:
        return '<redacted-identity>'
    value = re.sub(r'(https?://[^\s?]+)\?[^\s]*', r'\1', value)
    value = UUID.sub(lambda match: alias(match.group()), value)
    value = EMAIL.sub('<redacted-identity>', value)
    value = IPV4.sub('<redacted-address>', value)
    value = IPV6.sub('<redacted-address>', value)
    value = DOMAIN.sub('<redacted-domain>', value)
    if field.lower() not in ('sha256', 'artifactsha256'):
        value = re.sub(r'[A-Za-z0-9_+/=-]{32,}', lambda match: alias(match.group()), value)
    # Credential IDs which are opaque non-UUID IDs also require stable references.
    if field.lower() in ('credential_id', 'auth_id', 'account_id', 'client_key_id') and value:
        value = alias(value)
    return value


def restore(value):
    with lock:
        inverse = {v: k for k, v in aliases.items()}
    return UUID.sub(lambda match: inverse.get(match.group(), match.group()), value)


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'

    def log_message(self, *args):
        pass

    def route(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        path = restore(urllib.parse.unquote(self.path))
        if body:
            body = restore(body.decode()).encode()
        if path in ('/api/auth/status', '/api/auth/refresh'):
            try:
                ops.current()  # Validate the deployment Key against the real server.
            except Exception:
                self.send_error(502, 'admin_key_validation_failed')
                return
            # Only this browser auth-guard response is simulated; not cookie login.
            # CPR's frontend rejects API envelopes whose numeric code is not 200.
            data = json.dumps({'code': 200, 'message': 'success', 'data': {'authenticated': True,
                'session': {'role': 'admin', 'expiresAt': '2099-01-01T00:00:00Z'}}}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        headers = {'x-api-key': admin_key, 'Accept-Encoding': 'identity'}
        for name in ['Accept', 'Content-Type']:
            if self.headers.get(name):
                headers[name] = self.headers[name]
        if self.headers.get('Origin'):
            headers['Origin'] = upstream
        request = urllib.request.Request(upstream + path, data=body if body else None, method=self.command, headers=headers)
        try:
            response = opener.open(request, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        except Exception:
            self.send_error(502, 'validation_proxy_failure')
            return
        with response:
            data = response.read()
            content_type = response.headers.get('Content-Type', 'application/octet-stream')
            if 'application/json' in content_type:
                try:
                    data = json.dumps(sanitise(json.loads(data))).encode()
                except Exception:
                    data = b'{"error":"redaction_failed"}'
                    response.code = 502
            self.send_response(response.code)
            for key, value in response.headers.items():
                if key.lower() in ['content-type', 'cache-control', 'content-security-policy', 'x-content-type-options']:
                    self.send_header(key, value)
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    do_GET = route
    do_POST = route
    do_PUT = route
    do_PATCH = route
    do_DELETE = route


if __name__ == '__main__':
    os.umask(0o077)
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 18761), Handler)
    def shutdown(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, shutdown)
    print(json.dumps({'ready': True, 'real_admin_key_valid': True, 'loopback_only': True,
        'local_auth_guard_bootstrap': True, 'cookie_login_verified': False}), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        server.server_close()
