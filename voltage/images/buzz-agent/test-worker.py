"""Exercise headless workers and the buzz-subagent claim client inside the image.

Runs against a local bare Git remote (GitHub URLs are rewritten to it through
Git configuration only) and a moto S3 server on an internal Docker network.
"""
import subprocess
import sys
import uuid

SCRIPT = r'''
import http.server, json, os, subprocess, sys, threading, time
from pathlib import Path

ROOT = Path('/tmp/worker-test')
REMOTE = ROOT / 'remote.git'
FOCUS = 'Rename the greeting; SYNTHETIC-FOCUS-MARKER'

def sh(*args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kwargs)

ROOT.mkdir()
seed = ROOT / 'seed'
sh('git', 'init', '-q', '-b', 'main', str(seed))
(seed / 'hello.txt').write_text('hello\n')
git_env = {**os.environ, 'GIT_AUTHOR_NAME': 'Worker Fixture', 'GIT_AUTHOR_EMAIL': 'worker@example.invalid',
           'GIT_COMMITTER_NAME': 'Worker Fixture', 'GIT_COMMITTER_EMAIL': 'worker@example.invalid',
           'GIT_CONFIG_COUNT': '2',
           'GIT_CONFIG_KEY_0': 'url.file://' + str(REMOTE) + '.insteadOf', 'GIT_CONFIG_VALUE_0': 'https://github.com/example/fixture',
           'GIT_CONFIG_KEY_1': 'commit.gpgsign', 'GIT_CONFIG_VALUE_1': 'false'}
sh('git', '-C', str(seed), 'add', '.', env=git_env)
sh('git', '-C', str(seed), 'commit', '-qm', 'seed', env=git_env)
sh('git', 'clone', '-q', '--bare', str(seed), str(REMOTE))
for _ in range(60):
    if subprocess.run(['aws', 's3api', 'list-buckets'], capture_output=True, timeout=5).returncode == 0:
        break
    time.sleep(0.5)
sh('aws', 's3api', 'create-bucket', '--bucket', 'checkpoints', '--create-bucket-configuration', 'LocationConstraint=us-west-2')

bin_dir = ROOT / 'bin'
bin_dir.mkdir()
(bin_dir / 'claude').write_text("""#!/bin/sh
case "$FAKE_MODE" in
  commit) echo hi > hello.txt && git commit -qam 'feat: rename greeting' && echo 'renamed and verified';;
  dirty) echo dirty > hello.txt; echo 'left edits'; exit 3;;
  slow) sleep 30 & echo $! > "$CHILD_PID_FILE"; wait;;
  background) sleep 30 & echo $! > "$CHILD_PID_FILE"; echo 'finished';;
esac
""")
(bin_dir / 'claude').chmod(0o755)

def worker(task, mode, **overrides):
    env = {**git_env, 'PATH': str(bin_dir) + ':' + os.environ['PATH'], 'FAKE_MODE': mode, 'HOME': str(ROOT / ('home-' + task)),
           'BUZZ_WORKER_TASK': task, 'BUZZ_WORKER_FOCUS': FOCUS, 'BUZZ_WORKER_REPO': 'https://github.com/example/fixture',
           'BUZZ_WORKER_BASE': 'main', 'BUZZ_WORKER_RUNTIME': 'claude', 'BUZZ_CHECKPOINT_BUCKET': 'checkpoints',
           'BUZZ_WORKER_RESULT_PREFIX': 'dev/workers/', 'CHILD_PID_FILE': str(ROOT / (task + '.pid')), **overrides}
    Path(env['HOME']).mkdir()
    return subprocess.run(['/usr/local/bin/buzz-agent-worker'], env=env, capture_output=True, text=True, timeout=120,
                          cwd=env['HOME'])

def result(task):
    return json.loads(sh('aws', 's3', 'cp', 's3://checkpoints/dev/workers/' + task + '/result.json', '-').stdout)

def remote_branch(task):
    return subprocess.run(['git', '--git-dir', str(REMOTE), 'rev-list', '--count', 'main..worker/' + task],
                          capture_output=True, text=True)

# A worker that commits pushes its branch and reports success.
done = worker('task-commit', 'commit')
assert done.returncode == 0, done.stderr[-800:]
found = result('task-commit')
assert found['pushed'] and found['commits'] == 1 and found['exit_code'] == 0 and found['branch'] == 'worker/task-commit', found
assert 'renamed and verified' in found['summary']
assert remote_branch('task-commit').stdout.strip() == '1'
# Activity records carry metadata only, never the focus or runtime output.
assert 'worker_completed' in done.stderr and 'SYNTHETIC-FOCUS-MARKER' not in done.stderr and 'renamed and verified' not in done.stderr

# Uncommitted edits are kept and pushed; the runtime's failure status is kept.
dirty = worker('task-dirty', 'dirty')
assert dirty.returncode == 3, dirty.stderr[-800:]
found = result('task-dirty')
assert found['commits'] == 1 and found['pushed'] and found['exit_code'] == 3, found

# A runtime that exceeds its timeout reports 124.
slow = worker('task-slow', 'slow', BUZZ_WORKER_TIMEOUT='2')
assert slow.returncode == 124 and result('task-slow')['exit_code'] == 124

def assert_child_stopped(task):
    pid = int((ROOT / (task + '.pid')).read_text())
    status = Path('/proc') / str(pid) / 'stat'
    # A container without a reaping PID 1 can retain a zombie; it cannot run tools.
    assert not status.exists() or status.read_text().split()[2] in ('Z', 'X'), (task, pid)

assert_child_stopped('task-slow')
# A successful CLI may leave tools behind too. Saving work must wait for their cleanup.
background = worker('task-background', 'background')
assert background.returncode == 0, background.stderr[-800:]
assert_child_stopped('task-background')

# Exercise the production runner under a memory cap smaller than its output.
# This fails if capture_output or an unbounded buffer is reintroduced.
bounded = """import resource, runpy, sys
worker = runpy.run_path('/usr/local/bin/buzz-agent-worker', run_name='probe')
resource.setrlimit(resource.RLIMIT_AS, (64 * 1024 * 1024, 64 * 1024 * 1024))
program = "import os; [os.write(1, b'x' * 8192) for _ in range(16384)]; os.write(1, b'TAIL-MARKER')"
code, output = worker['run_runtime']([sys.executable, '-c', program], '/tmp', 20)
assert code == 0 and len(output) <= 4000 and output.endswith('TAIL-MARKER'), (code, len(output))
"""
sh(sys.executable, '-c', bounded)

# SIGTERM stops descendants before returning a cancellation result.
cancelled = """import pathlib, runpy, sys
worker = runpy.run_path('/usr/local/bin/buzz-agent-worker', run_name='probe')
code, _ = worker['run_runtime'](['sh', '-c', 'sleep 30 & echo $! > "$1"; wait', 'probe', sys.argv[1]], '/tmp', 30)
pathlib.Path(sys.argv[2]).write_text(str(code))
"""
pid_file, code_file = ROOT / 'cancel.pid', ROOT / 'cancel.code'
probe = subprocess.Popen([sys.executable, '-c', cancelled, str(pid_file), str(code_file)])
for _ in range(1000):
    if pid_file.exists(): break
    time.sleep(0.01)
assert pid_file.exists(), 'cancellation probe never started its child'
probe.terminate()
assert probe.wait(timeout=10) == 0 and code_file.read_text() == '143'
assert_child_stopped('cancel')

# Invalid inputs are refused before any clone.
for bad in [{'BUZZ_WORKER_REPO': 'https://evil.example/x/y'}, {'BUZZ_WORKER_TASK': 'Bad_Task'},
            {'BUZZ_WORKER_BASE': '--upload-pack=x'}, {'BUZZ_WORKER_RUNTIME': 'bash'}, {'BUZZ_WORKER_FOCUS': ''},
            {'BUZZ_WORKER_TIMEOUT': '0'}, {'BUZZ_WORKER_TIMEOUT': '-60'}, {'BUZZ_WORKER_TIMEOUT': 'x'},
            {'BUZZ_WORKER_TIMEOUT': '7201'}]:
    refused = worker('task-' + str(abs(hash(str(bad))) % 10**8), 'commit', **bad)
    assert refused.returncode == 2, (bad, refused.stderr[-400:])

# buzz-subagent creates a bounded claim against the worker pool and reads results.
seen = []
class Api(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass
    def reply(self, body):
        data = json.dumps(body).encode()
        self.send_response(200); self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(data))); self.end_headers(); self.wfile.write(data)
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        seen.append((self.path, self.headers['Authorization'], body))
        self.reply({**body, 'status': {'sandbox': {'name': 'sb-1'}}})
    def do_GET(self):
        self.reply({'items': [s[2] for s in seen]})
    def do_DELETE(self):
        self.reply({})
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Api)
threading.Thread(target=server.serve_forever, daemon=True).start()
creds = ROOT / 'creds'
creds.mkdir()
(creds / 'token').write_text('synthetic-token\n')
(creds / 'ca.crt').write_text('')
(creds / 'namespace').write_text('buzz-agent-staging-dev\n')
cli_env = {**os.environ, 'BUZZ_SUBAGENT_CREDENTIALS': str(creds), 'BUZZ_SUBAGENT_API': 'http://127.0.0.1:%d' % server.server_port,
           'BUZZ_MANAGED_AGENT_START_NONCE': 'session-generation', 'BUZZ_DEVELOPER_ID': 'dev', 'BUZZ_CHECKPOINT_BUCKET': 'checkpoints'}
def cli(*args):
    return subprocess.run(['/usr/local/bin/buzz-subagent', *args], env=cli_env, capture_output=True, text=True, timeout=60)
spawned = cli('spawn', '--task', 'task-commit', '--repo', 'https://github.com/example/fixture', '--base', 'main',
              '--focus', FOCUS, '--minutes', '500')
assert spawned.returncode == 0, spawned.stderr
path, auth, claim = seen[0]
assert path == '/apis/extensions.agents.x-k8s.io/v1beta1/namespaces/buzz-agent-staging-dev/sandboxclaims'
assert auth == 'Bearer synthetic-token'
assert claim['metadata']['name'] == 'worker-task-commit' and claim['spec']['warmPoolRef'] == {'name': 'agent-worker'}
env = {e['name']: e['value'] for e in claim['spec']['env']}
assert set(env) == {'BUZZ_WORKER_TASK', 'BUZZ_WORKER_FOCUS', 'BUZZ_WORKER_REPO', 'BUZZ_WORKER_BASE', 'BUZZ_WORKER_RUNTIME', 'BUZZ_WORKER_TIMEOUT'}
assert env['BUZZ_WORKER_TIMEOUT'] == str(120 * 60 - 120)  # lifetime capped at 120 minutes
assert claim['spec']['lifecycle']['ttlSecondsAfterFinished'] == 3600
assert claim['spec']['additionalPodMetadata']['annotations'] == {'buzz.block.xyz/session-id': 'session-generation'}
assert cli('spawn', '--task', 'Bad_Task', '--repo', 'x', '--base', 'main', '--focus', 'x').returncode == 1
for minutes in ('-1', '0', '1', '2'):
    invalid = cli('spawn', '--repo', 'https://github.com/example/fixture', '--base', 'main',
                  '--focus', FOCUS, '--minutes', minutes)
    assert invalid.returncode == 1 and 'at least 3 minutes' in invalid.stderr, (minutes, invalid.stderr)
assert len(seen) == 1, 'invalid lifetime reached the Kubernetes API'
assert json.loads(cli('result', 'task-commit').stdout)['pushed'] is True
assert json.loads(cli('wait', 'task-commit', '--timeout', '5').stdout)['branch'] == 'worker/task-commit'
assert cli('result', 'never-ran').returncode == 1
assert json.loads(cli('cancel', 'task-commit').stdout)['cancelled'] is True

# Permission and transport errors must not masquerade as an unfinished worker.
error_bin = ROOT / 'aws-error'
error_bin.mkdir()
(error_bin / 'aws').write_text('#!/bin/sh\necho "An error occurred (AccessDenied)" >&2\nexit 1\n')
(error_bin / 'aws').chmod(0o755)
cli_env['PATH'] = str(error_bin) + ':' + os.environ['PATH']
denied = cli('wait', 'task-commit', '--timeout', '60')
assert denied.returncode == 1 and 'bucket permissions' in denied.stderr, denied.stderr
print('ok')
'''


def main():
    image = sys.argv[1]
    network = 'worker-test-' + uuid.uuid4().hex[:8]
    server = network + '-aws'
    try:
        subprocess.run(['docker', 'network', 'create', '--internal', network], check=True, capture_output=True)
        subprocess.run(['docker', 'run', '-d', '--rm', '--name', server, '--network', network,
                        'motoserver/moto:5.1.14@sha256:800719627036dc38dc699e9f314f624f2c98ac3d8bb754db078cb2698f08ee77'],
                       check=True, capture_output=True)
        result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', network, '--cap-drop', 'ALL',
                                 '-e', 'AWS_ACCESS_KEY_ID=test', '-e', 'AWS_SECRET_ACCESS_KEY=test',
                                 '-e', 'AWS_DEFAULT_REGION=us-west-2', '-e', 'AWS_ENDPOINT_URL=http://' + server + ':5000',
                                 '--entrypoint', 'python3', image, '-c', SCRIPT],
                                capture_output=True, text=True, timeout=600)
    finally:
        subprocess.run(['docker', 'stop', '--time', '1', server], capture_output=True)
        subprocess.run(['docker', 'network', 'rm', network], capture_output=True)
    if result.returncode or result.stdout.strip() != 'ok':
        print(result.stdout[-2000:], result.stderr[-4000:], file=sys.stderr)
        raise SystemExit('FAIL: headless workers')
    print('PASS: workers push their branch and result; claims are bounded to the worker pool')


if __name__ == '__main__':
    main()
