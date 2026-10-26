"""Exercise agent bootstrap against a local TLS broker with synthetic tokens only."""
import base64
from copy import deepcopy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from queue import Queue, Empty
from pathlib import Path
import ssl
import shutil
import signal
import sys
import subprocess
import tempfile
from threading import Thread
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('agent_profile', Path(__file__).with_name('profile.py'))
profile = importlib.util.module_from_spec(spec)
spec.loader.exec_module(profile)


def jwt(claims):
    return 'e30.' + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=') + '.fixture'


class BootstrapTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        directory = Path(cls.directory.name)
        cert, key = directory / 'cert.pem', directory / 'key.pem'
        ca, ca_key, csr = directory / 'ca.pem', directory / 'ca-key.pem', directory / 'request.pem'
        extensions = directory / 'extensions'
        extensions.write_text('subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\n'
                              'keyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n'
                              'authorityKeyIdentifier=keyid,issuer\nsubjectKeyIdentifier=hash\n')
        for args in [
            ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(ca_key),
             '-out', str(ca), '-days', '1', '-subj', '/CN=Fixture CA', '-addext', 'basicConstraints=critical,CA:TRUE',
             '-addext', 'keyUsage=critical,keyCertSign,cRLSign'],
            ['req', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(key), '-out', str(csr), '-subj', '/CN=localhost'],
            ['x509', '-req', '-in', str(csr), '-CA', str(ca), '-CAkey', str(ca_key), '-CAcreateserial',
             '-out', str(cert), '-days', '1', '-extfile', str(extensions)],
        ]:
            subprocess.run(['openssl', *args], check=True, capture_output=True)
        cls.ca = ca.read_text()
        cls.cookie = 'alice.' + 'a' * 32 + '.seed'

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['content-length'])))
                cls.requests.append(body)
                self.send_response(cls.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(cls.response)))
                if cls.status == 302:
                    self.send_header('Location', 'https://attacker.invalid')
                self.end_headers()
                self.wfile.write(cls.response)

        cls.server = ThreadingHTTPServer(('localhost', 0), Handler)
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert, key)
        cls.server.socket = context.wrap_socket(cls.server.socket, server_side=True)
        cls.thread = Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()
        cls.directory.cleanup()

    def setUp(self):
        cls = type(self)
        cls.requests = []
        cls.status = 200
        self.bundle = {
            'access_token': jwt({'exp': 2_000_000_000}),
            'id_token': jwt({'https://api.openai.com/auth': {'chatgpt_account_id': 'workspace'}}),
            'account_id': 'workspace', 'refresh_token': self.cookie.rsplit('.', 1)[0] + '.version-1',
        }
        cls.response = json.dumps(self.bundle).encode()
        self.login = {'url': f'https://localhost:{self.server.server_port}/oauth/token',
                      'ca_certificate': self.ca, 'refresh_token': self.cookie}
        self.files = {}
        self.writer = patch.object(profile, 'write_file', side_effect=lambda path, text: self.files.__setitem__(str(path), text))
        self.writer.start()
        self.addCleanup(self.writer.stop)

    def test_bootstrap_persists_only_broker_credentials_and_checks_tls_even_with_a_proxy(self):
        env = {}
        with patch.dict('os.environ', {'HTTPS_PROXY': 'http://127.0.0.1:1'}):
            profile.configure_codex_login(self.login, env)
        auth = json.loads(self.files['/home/agent/.codex/auth.json'])
        self.assertEqual(auth['tokens'], self.bundle)
        self.assertEqual(self.requests[0]['refresh_token'], self.cookie)
        self.assertEqual(env['CODEX_REFRESH_TOKEN_URL_OVERRIDE'], self.login['url'])
        self.assertIn(self.ca, self.files[env['CODEX_CA_CERTIFICATE']])
        self.assertNotIn('upstream-refresh-secret', json.dumps(self.files))

    def test_a_revoked_grant_stops_boot_before_any_credential_file_is_written(self):
        type(self).status = 401
        type(self).response = b'private-error-must-not-display'
        with self.assertRaises(profile.Refused) as refusal:
            profile.configure_codex_login(self.login, {})
        self.assertNotIn('private-error', str(refusal.exception))
        self.assertEqual(self.files, {})

    def test_redirects_untrusted_tls_bad_workspaces_and_oversized_replies_are_refused(self):
        for scenario in ('redirect', 'untrusted-ca', 'workspace', 'oversized'):
            with self.subTest(scenario=scenario):
                login = deepcopy(self.login)
                type(self).status = 302 if scenario == 'redirect' else 200
                bundle = deepcopy(self.bundle)
                if scenario == 'workspace':
                    bundle['account_id'] = 'other-workspace'
                type(self).response = b'x' * 65537 if scenario == 'oversized' else json.dumps(bundle).encode()
                if scenario == 'untrusted-ca':
                    login['ca_certificate'] = 'invalid-ca-private-fixture'
                with self.assertRaises(profile.Refused):
                    profile.configure_codex_login(login, {})
                self.assertEqual(self.files, {})

    @unittest.skipUnless(os.environ.get('BUZZ_TEST_PACKAGED_CODEX') == '1', 'requires the packaged pinned Codex binary')
    def test_the_pinned_codex_client_refreshes_and_persists_the_broker_credential(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            (home / 'config.toml').write_text('cli_auth_credentials_store = "file"\n')
            initial = {**self.bundle, 'refresh_token': self.cookie}
            (home / 'auth.json').write_text(json.dumps({
                'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None, 'tokens': initial,
                'last_refresh': '2026-10-01T00:00:00Z',
            }))
            (home / 'ca.pem').write_text(self.ca)
            env = {**os.environ, 'CODEX_HOME': directory, 'CODEX_CA_CERTIFICATE': str(home / 'ca.pem'),
                   'CODEX_REFRESH_TOKEN_URL_OVERRIDE': self.login['url']}
            for key in ('OPENAI_API_KEY', 'CODEX_API_KEY', 'CODEX_ACCESS_TOKEN'):
                env.pop(key, None)
            process = subprocess.Popen([shutil.which('codex'), 'app-server'], env=env,
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                       text=True, start_new_session=True)
            frames = Queue()
            reader = Thread(target=lambda: [frames.put(line) for line in process.stdout], daemon=True)
            reader.start()
            try:
                def request(identifier, method, params):
                    process.stdin.write(json.dumps({'id': identifier, 'method': method, 'params': params}) + '\n')
                    process.stdin.flush()
                    for _ in range(100):
                        try:
                            reply = json.loads(frames.get(timeout=10))
                        except Empty:
                            self.fail(f'pinned Codex client did not answer; broker requests: {len(self.requests)}')
                        if reply.get('id') == identifier:
                            self.assertNotIn('error', reply, 'pinned Codex rejected the synthetic refresh workflow')
                            return reply
                    self.fail('too many unrelated Codex frames')
                request(1, 'initialize', {'clientInfo': {'name': 'buzz-broker-test', 'version': '1.0'}})
                process.stdin.write('{"method":"initialized"}\n')
                process.stdin.flush()
                reply = request(2, 'getAuthStatus', {'refreshToken': True, 'includeToken': False})
                self.assertEqual(reply['result']['authMethod'], 'chatgpt')
                request(3, 'getAuthStatus', {'refreshToken': True, 'includeToken': False})
                self.assertGreaterEqual(len(self.requests), 1)
                self.assertEqual(self.requests[0]['refresh_token'], self.cookie)
                self.assertEqual(self.requests[-1]['refresh_token'], self.bundle['refresh_token'])
                saved = json.loads((home / 'auth.json').read_text())
                self.assertEqual(saved['tokens']['refresh_token'], self.bundle['refresh_token'])
            finally:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=10)
                process.stdin.close()
                process.stdout.close()
                reader.join(timeout=5)

    def test_profile_metadata_rejects_external_urls_and_credentials_for_another_developer(self):
        login = {**self.login, 'url': 'https://codex-refresh.buzz-sandbox-manager.svc.cluster.local:8443/oauth/token'}
        profile.validate_codex_login(login, 'alice')
        for key, bad in [('url', 'https://attacker.invalid/oauth/token'),
                         ('refresh_token', 'bob.' + 'a' * 32 + '.seed')]:
            with self.assertRaises(profile.Refused):
                profile.validate_codex_login({**login, key: bad}, 'alice')


if __name__ == '__main__':
    if len(sys.argv) == 2 and not sys.argv[1].startswith('-'):
        directory = Path(__file__).resolve().parent
        result = subprocess.run([
            'docker', 'run', '--rm', '--network', 'none', '--cap-drop', 'ALL',
            '--security-opt', 'no-new-privileges', '--env', 'BUZZ_TEST_PACKAGED_CODEX=1',
            '--mount', f'type=bind,src={directory / "profile.py"},dst=/tmp/profile.py,readonly',
            '--mount', f'type=bind,src={Path(__file__).resolve()},dst=/tmp/test-codex-login.py,readonly',
            '--entrypoint', 'python3', sys.argv[1], '/tmp/test-codex-login.py',
        ], timeout=120)
        sys.exit(result.returncode)
    unittest.main()
