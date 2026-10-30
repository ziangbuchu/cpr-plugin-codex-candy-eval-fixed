"""Deployment must fail closed rather than kill a user's in-flight suite."""
import ast
from pathlib import Path
import unittest


class ReleaseIdleGuard(unittest.TestCase):
    def check_state(self, state):
        source = Path(__file__).parents[1] / 'tools/remote-plugin-update.py'
        tree = ast.parse(source.read_text(), filename=str(source))
        function = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'require_idle')
        namespace = {'api': lambda path: state}
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(source), 'exec'), namespace)
        return namespace['require_idle']({'id': 'test', 'artifactSha256': 'test', 'revision': 1})

    def test_idle_is_allowed(self):
        self.check_state({'auths': [{'running': None, 'pelican_running': False}]})

    def test_each_running_suite_blocks_switch(self):
        for key, value in [('running', {'done': 0}), ('pelican_running', True),
                           ('fingerprint_running', {'phase': 'running'}),
                           ('modeltrace_running', {'phase': 'running'})]:
            with self.subTest(key=key), self.assertRaisesRegex(RuntimeError, 'tests_in_progress'):
                self.check_state({'auths': [{key: value}]})

    def test_unknown_state_fails_closed(self):
        for state in [None, {}, {'auths': None}, {'auths': [None]}]:
            with self.subTest(state=state), self.assertRaisesRegex(RuntimeError, 'schema_unrecognized'):
                self.check_state(state)


if __name__ == '__main__':
    unittest.main()
