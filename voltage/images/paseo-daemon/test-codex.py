"""Exercise the packaged Codex sandbox and app-server with synthetic credentials."""
import json
import os
from pathlib import Path
import selectors
import signal
import socket
import subprocess
import time

home = Path.home()
status = dict(line.split(':', 1) for line in Path('/proc/self/status').read_text().splitlines() if ':' in line)
assert os.geteuid() == 1000 and int(status['CapEff'].strip(), 16) == 0, 'outer container must remain non-root and capability-free'
assert status['NoNewPrivs'].strip() == '1', 'privilege escalation must remain disabled'
readable = home / 'code' / 'codex-smoke.txt'
readable.parent.mkdir(exist_ok=True)
readable.write_text('readable-code-fixture')
credential = home / '.paseo' / 'codex-smoke-secret'
credential.write_text('synthetic-secret-must-not-be-readable')


def sandbox(arguments):
    return subprocess.run(['codex', 'sandbox', '--include-managed-config', '-P', ':read-only', '--', *arguments],
                          capture_output=True, timeout=30)


allowed = sandbox(['cat', str(readable)])
assert allowed.stdout == b'readable-code-fixture', allowed.stderr[-1500:].decode(errors='replace')
denied = sandbox(['cat', str(credential)])
assert denied.returncode != 0 and b'synthetic-secret-must-not-be-readable' not in denied.stdout
blocked = home / 'code' / 'codex-smoke-write'
assert sandbox(['sh', '-c', 'echo forbidden > "$1"', 'sh', str(blocked)]).returncode != 0
assert not blocked.exists()
with socket.socket() as listener:
    listener.bind(('127.0.0.1', 0))
    listener.listen(8)
    port = listener.getsockname()[1]
    connect = ['python3', '-c', f'import socket; socket.create_connection(("127.0.0.1", {port}), timeout=1)']
    assert subprocess.run(connect, capture_output=True, timeout=5).returncode == 0
    assert sandbox(connect).returncode != 0
readable.unlink()
credential.unlink()

with subprocess.Popen(['codex', 'app-server'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                      stderr=subprocess.DEVNULL, start_new_session=True) as child:
    try:
        buffer = bytearray()
        def call(identifier, method, params):
            child.stdin.write((json.dumps({'id': identifier, 'method': method, 'params': params}) + '\n').encode())
            child.stdin.flush()
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    while b'\n' in buffer:
                        line, _, remainder = buffer.partition(b'\n')
                        buffer[:] = remainder
                        reply = json.loads(line)
                        if reply.get('id') == identifier:
                            return reply
                    if not selector.select(1):
                        continue
                    chunk = os.read(child.stdout.fileno(), 65536)
                    assert chunk, 'Codex app-server exited'
                    buffer.extend(chunk)
                    assert len(buffer) <= 1024 * 1024, 'oversized Codex response'
            raise RuntimeError('Codex RPC timed out')

        assert 'error' not in call(1, 'initialize', {
            'clientInfo': {'name': 'paseo', 'version': '0.15.3'}, 'capabilities': {'experimentalApi': True}})
        child.stdin.write(b'{"method":"initialized"}\n')
        child.stdin.flush()
        # Read the provider's real startup configuration, including its policy.
        config = json.loads((Path(os.environ['PASEO_HOME']) / 'config.json').read_text())
        policy = config['agents']['providers']['codex']['options']
        approval = policy['approval_policy']['granular']
        assert approval['sandbox_approval'] is False
        assert approval['request_permissions'] is False
        started = call(2, 'thread/start', {'cwd': str(home / 'assistant'), 'config': policy})
        assert 'error' not in started, 'native read-only thread creation failed'
        assert started['result']['sandbox'] == {'type': 'readOnly', 'networkAccess': False}
        forbidden = call(3, 'thread/start', {'cwd': str(home / 'assistant'), 'config': {
            'approval_policy': 'never', 'sandbox_mode': 'danger-full-access'}})
        if 'error' not in forbidden:
            # Codex can clamp unsupported requests to the managed policy.
            assert forbidden['result']['approvalPolicy'] == started['result']['approvalPolicy']
            assert forbidden['result']['sandbox'] == {'type': 'readOnly', 'networkAccess': False}
        reviewed = call(4, 'thread/start', {'cwd': str(home / 'assistant'), 'config': policy,
                                            'approvalsReviewer': 'auto_review', 'ephemeral': True})
        assert 'error' not in reviewed, 'native Auto-review thread creation failed'
        assert reviewed['result']['approvalsReviewer'] == 'auto_review'
        assert reviewed['result']['approvalPolicy'] == started['result']['approvalPolicy']
        assert reviewed['result']['sandbox'] == started['result']['sandbox']
        expanded = call(5, 'thread/start', {
            'cwd': str(home / 'assistant'),
            'config': {**policy, 'approval_policy': {'granular': {
                **approval, 'sandbox_approval': True, 'request_permissions': True,
            }}},
            'approvalsReviewer': 'auto_review', 'ephemeral': True,
        })
        if 'error' not in expanded:
            assert expanded['result']['approvalPolicy'] == started['result']['approvalPolicy']
    finally:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=5)
print('PASS: native Codex starts read-only; code reads work; credentials, writes, network, and full-access mode are denied')
