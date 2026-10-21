"""Synthetic setup-gate checks; no login, account state, model or relay access."""
import json
import subprocess
import sys
import time
import unittest
import uuid

IMAGE = sys.argv.pop(1) if len(sys.argv) > 1 and not sys.argv[1].startswith('-') else 'buzz-agent:local'
PUBKEY = 'a' * 64
POD_UID = '11111111-2222-3333-4444-555555555555'
DIGEST = 'sha256:' + 'b' * 64
NONCE = 'c' * 32
GENERATION = '1234abcd'
ACK = {'version': 1, 'agent_pubkey': PUBKEY, 'pod_uid': POD_UID,
       'generation': GENERATION, 'image_digest': DIGEST}


def docker(*args, **kwargs):
    return subprocess.run(['docker', *args], text=True, capture_output=True, timeout=15, **kwargs)


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.names = []

    def tearDown(self):
        for name in self.names:
            docker('rm', '-f', name)

    def start(self, overrides=None, deadline=None, shell=None):
        name = 'dev2900-setup-' + uuid.uuid4().hex[:12]
        self.names.append(name)
        env = {'BUZZ_SETUP_MODE': 'codex-device-v1',
               'BUZZ_SETUP_AGENT_PUBKEY': PUBKEY, 'BUZZ_SETUP_POD_UID': POD_UID,
               'BUZZ_SETUP_IMAGE_DIGEST': DIGEST, 'BUZZ_SETUP_NONCE': NONCE,
               'BUZZ_SETUP_DEADLINE': str(deadline or int(time.time()) + 120),
               'BUZZ_MANAGED_AGENT_START_NONCE': GENERATION,
               'BUZZ_ACP_AGENT_COMMAND': 'codex-acp', 'BUZZ_ACP_MCP_COMMAND': 'buzz-dev-mcp',
               'BUZZ_ACP_PERMISSION_MODE': 'agent-full-access',
               'CODEX_HOME': '/home/agent/.codex', 'NO_BROWSER': '1',
               'CODEX_CONFIG': '{"cli_auth_credentials_store":"file"}',
               'BUZZ_PRIVATE_KEY': '0' * 63 + '1', 'BUZZ_RELAY_URL': 'wss://relay.invalid'}
        env.update(overrides or {})
        args = ['run', '-d', '--name', name, '--network', 'none', '--cap-drop', 'ALL',
                '--security-opt', 'no-new-privileges',
                '--tmpfs', '/home/agent:uid=10001,gid=10001,mode=0700']
        for key, value in env.items():
            if value is not None:
                args += ['-e', key + '=' + value]
        if shell:
            args += ['--entrypoint', '/bin/bash', IMAGE, '-ceu', shell]
        else:
            args += [IMAGE]
        result = docker(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        return name

    def pending(self, name):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            result = docker('exec', name, 'test', '-f', '/home/agent/.agent-setup/pending.json')
            if result.returncode == 0:
                return
            time.sleep(0.05)
        self.fail('gate did not become pending')

    def complete(self, name, value=ACK):
        text = value if isinstance(value, str) else json.dumps(value)
        return docker('exec', '-i', name, '/usr/local/bin/buzz-setup-complete', input=text)

    def wait_exit(self, name, expected):
        result = docker('wait', name)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(expected))
        self.assertEqual(docker('inspect', name, '--format', '{{.State.Running}}').stdout.strip(), 'false')

    def write_ack(self, name, text, suffix=''):
        result = docker('exec', '-i', name, '/bin/bash', '-ceu',
                        'umask 077; cat > /home/agent/.agent-setup/test-injected' + suffix +
                        '; mv /home/agent/.agent-setup/test-injected /home/agent/.agent-setup/complete.json', input=text)
        self.assertEqual(result.returncode, 0, result.stderr)

    def full_ack(self, name):
        result = docker('exec', name, 'cat', '/home/agent/.agent-setup/pending.json')
        self.assertEqual(result.returncode, 0)
        return json.loads(result.stdout)

    def test_sandbox_generation_is_accepted(self):
        generation = 'a' * 32
        name = self.start(overrides={'BUZZ_MANAGED_AGENT_START_NONCE': generation})
        self.pending(name)
        result = self.complete(name, dict(ACK, generation=generation))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_success_private_startup_config_and_one_use(self):
        name = self.start()
        self.pending(name)
        result = docker('exec', name, '/bin/bash', '-ceu',
                        'test "$(stat -c %a /home/agent/.codex)" = 700; '
                        'test "$(stat -c %a /home/agent/.codex/config.toml)" = 600; '
                        'test "$(stat -c %a /home/agent/.agent-setup)" = 700; '
                        'test "$(stat -c %a /home/agent/.agent-setup/pending.json)" = 600; '
                        'cat /home/agent/.codex/config.toml')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'cli_auth_credentials_store = "file"\n')
        listing = docker('top', name, '-eo', 'pid,args')
        self.assertEqual(listing.returncode, 0, listing.stderr)
        processes = listing.stdout
        for executable in ['buzz-acp', 'codex-acp', 'app-server']:
            self.assertNotIn(executable, processes)
        result = self.complete(name)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(NONCE, result.stdout + result.stderr)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if docker('exec', name, 'test', '-f', '/home/agent/.agent-setup/consumed').returncode == 0:
                break
            time.sleep(0.05)
        else:
            self.fail('acknowledgement was not consumed')
        listing = docker('top', name, '-eo', 'pid,args')
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertIn('buzz-acp', listing.stdout)
        self.assertNotEqual(self.complete(name).returncode, 0)
        for path in ['complete.json', 'pending.json', 'accepted']:
            self.assertNotEqual(docker('exec', name, 'test', '-e', '/home/agent/.agent-setup/' + path).returncode, 0)
        logs = docker('logs', name).stdout
        self.assertEqual(logs.count('agent setup complete; starting harness'), 1)
        self.assertNotIn(NONCE, logs)
        self.assertEqual(docker('kill', '--signal', 'TERM', name).returncode, 0)
        self.wait_exit(name, 0)

    def test_helper_rejects_wrong_bindings_unknown_and_duplicate_fields(self):
        name = self.start()
        self.pending(name)
        bad = [dict(ACK, version=2), dict(ACK, pod_uid='other'), dict(ACK, generation='deadbeef'),
               dict(ACK, image_digest='sha256:' + 'd' * 64), dict(ACK, agent_pubkey='d' * 64),
               dict(ACK, extra='refused'), dict(ACK, setup_nonce=NONCE),
               json.dumps(ACK)[:-1] + ', "version": 1}', '[]', 'x' * 4097]
        for value in bad:
            with self.subTest(value_type=type(value).__name__):
                result = self.complete(name, value)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn(NONCE, result.stdout + result.stderr)
        self.assertEqual(self.complete(name).returncode, 0)

    def test_gate_rejects_forged_seven_field_ack(self):
        name = self.start()
        self.pending(name)
        value = self.full_ack(name)
        value['setup_nonce'] = 'd' * 32
        self.write_ack(name, json.dumps(value))
        self.wait_exit(name, 1)

    def test_gate_rejects_public_ack_with_unsafe_mode(self):
        name = self.start()
        self.pending(name)
        value = self.full_ack(name)
        self.write_ack(name, json.dumps(value), '; chmod 644 /home/agent/.agent-setup/test-injected')
        self.wait_exit(name, 1)

    def test_gate_rejects_symlink_ack(self):
        name = self.start()
        self.pending(name)
        result = docker('exec', name, 'ln', '-s', '/home/agent/.agent-setup/pending.json',
                        '/home/agent/.agent-setup/complete.json')
        self.assertEqual(result.returncode, 0)
        self.wait_exit(name, 1)

    def test_gate_rejects_unwritten_fifo_ack_and_stops_enrollment(self):
        name = self.start(deadline=int(time.time()) + 5)
        self.pending(name)
        self.assertEqual(docker('exec', '-d', name, '/bin/bash', '-c', 'sleep 300 & wait').returncode, 0)
        started = time.monotonic()
        result = docker('exec', name, 'mkfifo', '-m', '600', '/home/agent/.agent-setup/complete.json')
        self.assertEqual(result.returncode, 0, result.stderr)
        # Timeout shorter than the setup deadline proves immediate type refusal.
        result = subprocess.run(['docker', 'wait', name], text=True, capture_output=True, timeout=3)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), '1')
        self.assertLess(time.monotonic() - started, 3)
        self.assertNotEqual(docker('exec', name, 'true').returncode, 0)
        logs = docker('logs', name)
        self.assertIn('agent setup refused', logs.stderr)
        self.assertNotIn('starting harness', logs.stdout)

    def test_helper_rejects_unwritten_fifo_private_inputs(self):
        for path in ['.agent-setup/pending.json', '.codex/config.toml']:
            with self.subTest(path=path):
                name = self.start()
                self.pending(name)
                target = '/home/agent/' + path
                result = docker('exec', name, '/bin/bash', '-ceu',
                                'rm "' + target + '"; mkfifo -m 600 "' + target + '"')
                self.assertEqual(result.returncode, 0, result.stderr)
                result = subprocess.run(['docker', 'exec', '-i', name, '/usr/local/bin/buzz-setup-complete'],
                                        input=json.dumps(ACK), text=True, capture_output=True, timeout=3)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('agent setup refused', result.stderr)
                self.assertEqual(docker('kill', '--signal', 'TERM', name).returncode, 0)
                self.wait_exit(name, 143)

    def test_gate_rejects_modified_startup_config(self):
        name = self.start()
        self.pending(name)
        self.assertEqual(docker('exec', name, '/bin/bash', '-ceu',
                               'printf \'cli_auth_credentials_store = "ephemeral"\\n\' > /home/agent/.codex/config.toml').returncode, 0)
        self.assertNotEqual(self.complete(name).returncode, 0)
        self.write_ack(name, json.dumps(self.full_ack(name)))
        self.wait_exit(name, 1)

    def test_completion_rechecks_codex_directory_symlink(self):
        name = self.start()
        self.pending(name)
        value = self.full_ack(name)
        self.assertEqual(docker('exec', name, '/bin/bash', '-ceu',
                               'mv /home/agent/.codex /home/agent/.codex-moved; '
                               'ln -s /home/agent/.codex-moved /home/agent/.codex').returncode, 0)
        self.assertNotEqual(self.complete(name).returncode, 0)
        self.write_ack(name, json.dumps(value))
        self.wait_exit(name, 1)

    def test_invalid_descriptor_and_overrides_refuse(self):
        for values in [{'CODEX_PATH': '/bin/true'}, {'CODEX_CONFIG': '{}'},
                       {'CODEX_HOME': ''}, {'NO_BROWSER': ''},
                       {'OPENAI_API_KEY': 'fictional'}, {'DEFAULT_AUTH_REQUEST': '{}'},
                       {'BUZZ_ACP_AGENT_COMMAND': 'buzz-agent'}, {'BUZZ_ACP_AGENT_ARGS': '--version'},
                       {'BUZZ_SETUP_EXTRA': 'unknown'}, {'BUZZ_SETUP_POD_UID': 'missing'}]:
            with self.subTest(field=next(iter(values))):
                self.wait_exit(self.start(overrides=values), 1)

    def test_expired_and_overlong_deadlines_refuse(self):
        self.wait_exit(self.start(deadline=int(time.time()) - 1), 1)
        self.wait_exit(self.start(deadline=int(time.time()) + 950), 1)

    def test_container_execution_mode_required_before_pending(self):
        for values in [{'BUZZ_ACP_PERMISSION_MODE': None},
                       {'BUZZ_ACP_PERMISSION_MODE': ''},
                       {'BUZZ_ACP_PERMISSION_MODE': 'default'},
                       {'BUZZ_ACP_PERMISSION_MODE': 'bypass-permissions'}]:
            with self.subTest(values=values):
                name = self.start(overrides=values)
                self.wait_exit(name, 1)
                logs = docker('logs', name)
                self.assertIn('agent setup refused', logs.stderr)
                self.assertNotIn('agent setup pending', logs.stdout)
                self.assertNotIn('starting harness', logs.stdout)

    def test_timeout_and_term_stop_exec_created_enrollment_processes(self):
        for signal in [None, 'TERM']:
            with self.subTest(signal=signal):
                name = self.start(deadline=int(time.time()) + (3 if signal is None else 120))
                self.pending(name)
                self.assertEqual(docker('exec', '-d', name, '/bin/bash', '-c', 'sleep 300 & wait').returncode, 0)
                started = time.monotonic()
                if signal:
                    self.assertEqual(docker('kill', '--signal', signal, name).returncode, 0)
                self.wait_exit(name, 143 if signal else 1)
                self.assertLess(time.monotonic() - started, 5)
                self.assertNotEqual(docker('exec', name, 'true').returncode, 0)

    def test_helper_requires_pending_and_gate_reentry_refuses(self):
        name = self.start(shell='exec sleep 120')
        self.assertNotEqual(self.complete(name).returncode, 0)
        active = self.start()
        self.pending(active)
        result = docker('exec', active, '/usr/local/bin/buzz-agent-setup', 'gate')
        self.assertNotEqual(result.returncode, 0)


if __name__ == '__main__':
    unittest.main(verbosity=2)
