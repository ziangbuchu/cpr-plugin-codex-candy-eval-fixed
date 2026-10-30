"""Isolated HTTP contract tests; no production imports, keys or cookie login."""
import ast
import http.server
import json
from pathlib import Path
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import Mock
import urllib.error
import urllib.request


class BrowserProxyAuthContract(unittest.TestCase):
    def setUp(self):
        source = Path(__file__).parents[1] / 'tools' / 'remote-browser-proxy.py'
        tree = ast.parse(source.read_text(), filename=str(source))
        handler_node = next(node for node in tree.body
                            if isinstance(node, ast.ClassDef) and node.name == 'Handler')
        # Execute the actual Handler only, excluding production credential lookup.
        self.validate = Mock()
        namespace = {'http': SimpleNamespace(server=http.server), 'json': json,
                     'urllib': SimpleNamespace(parse=__import__('urllib.parse', fromlist=['parse'])),
                     'restore': lambda value: value,
                     'ops': SimpleNamespace(current=self.validate)}
        exec(compile(ast.Module(body=[handler_node], type_ignores=[]), str(source), 'exec'),
             namespace)
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), namespace['Handler'])
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def test_status_matches_frontend_success_envelope(self):
        with urllib.request.urlopen(self.base + '/api/auth/status', timeout=3) as response:
            data = json.load(response)
            self.assertEqual(response.status, 200)
            self.assertEqual(response.headers['Cache-Control'], 'no-store')
        self.assertEqual(data['code'], 200)
        self.assertTrue(data['data']['authenticated'])
        self.validate.assert_called_once_with()

    def test_refresh_matches_frontend_success_envelope(self):
        request = urllib.request.Request(self.base + '/api/auth/refresh', method='POST')
        with urllib.request.urlopen(request, timeout=3) as response:
            self.assertEqual(json.load(response)['code'], 200)
        self.validate.assert_called_once_with()

    def test_validation_failure_is_closed(self):
        self.validate.side_effect = RuntimeError('test_validation_failure')
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.base + '/api/auth/status', timeout=3)
        self.assertEqual(caught.exception.code, 502)
        caught.exception.close()
        self.validate.assert_called_once_with()


class BrowserProxyRedactionContract(unittest.TestCase):
    def setUp(self):
        source = Path(__file__).parents[1] / 'tools' / 'remote-browser-proxy.py'
        tree = ast.parse(source.read_text(), filename=str(source))
        constants = {'aliases', 'lock', 'UUID', 'EMAIL', 'DOMAIN', 'IPV4', 'IPV6',
                     'SECRET_FIELD', 'IDENTITY_FIELD'}
        nodes = [node for node in tree.body if
                 (isinstance(node, ast.FunctionDef) and node.name in ('alias', 'sanitise', 'restore')) or
                 (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id in constants for t in node.targets))]
        self.namespace = {'re': __import__('re'), 'threading': threading,
                          'ops': SimpleNamespace(PLUGIN='nmbb.codex-candy-eval')}
        exec(compile(ast.Module(body=nodes, type_ignores=[]), str(source), 'exec'), self.namespace)

    def test_identity_alias_is_uuid_shaped_stable_and_restorable(self):
        sanitise, restore = self.namespace['sanitise'], self.namespace['restore']
        result = sanitise('test-only-account', 'id')
        self.assertRegex(result, r'^00000000-0000-4000-8000-[0-9a-f]{12}$')
        self.assertEqual(result, sanitise('test-only-account', 'id'))
        self.assertEqual(result, sanitise(result, 'id'))
        self.assertEqual('test-only-account', restore(result))

    def test_public_resource_and_page_paths_are_not_mistaken_for_domains(self):
        sanitise = self.namespace['sanitise']
        resource = 'web/vendor/test-font-0123456789ab.woff2'
        self.assertEqual(resource, sanitise(resource, 'path'))
        page = {'id': 'codex-candy-eval', 'title': 'Test page', 'entry': 'web/ui.html'}
        self.assertEqual(page, sanitise(page))

    def test_credentials_queries_and_network_identifiers_remain_redacted(self):
        result = self.namespace['sanitise']({'token': 'test-only-secret', 'email': 'test@example.invalid',
            'url': 'https://example.invalid/path?unknown=test-only-value',
            'address': '192.0.2.1', 'ipv6': '2001:db8::1'})
        self.assertEqual(result['token'], '<redacted-secret>')
        self.assertEqual(result['email'], '<redacted-identity>')
        self.assertNotIn('?', result['url'])
        self.assertNotIn('test-only-value', result['url'])
        self.assertEqual(result['address'], '<redacted-address>')
        self.assertEqual(result['ipv6'], '<redacted-address>')


if __name__ == '__main__':
    unittest.main()
