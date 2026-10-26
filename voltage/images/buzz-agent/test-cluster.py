"""Exercise staging kubeconfig generation with real kubectl and a local TLS API."""
import subprocess
import sys

SCRIPT = r'''
import http.server, json, os, runpy, ssl, subprocess, tempfile, threading
from pathlib import Path

profile = runpy.run_path('/usr/local/bin/buzz-agent-profile', run_name='probe')
configure = profile['install_cluster_access']
settings = configure.__globals__
root = Path(tempfile.mkdtemp(prefix='cluster-test-', dir='/home/agent'))
os.umask(0o077)
key, cert = root / 'key.pem', root / 'ca.crt'
subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost',
                '-keyout', str(key), '-out', str(cert)], check=True, capture_output=True)
expected = ['synthetic-first-token']
seen = []
class API(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        seen.append((self.path, self.headers.get('Authorization')))
        if self.headers.get('Authorization') != 'Bearer ' + expected[0]:
            self.send_error(401); return
        body = json.dumps({'kind': 'PodList', 'items': []}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers(); self.wfile.write(body)
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), API)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server.socket = context.wrap_socket(server.socket, server_side=True)
threading.Thread(target=server.serve_forever, daemon=True).start()

def require(condition, message):
    if not condition: raise RuntimeError(message)

def setup(mode):
    home = root / mode
    home.mkdir(mode=0o700)
    settings['DIRECTORY'] = home / 'config'
    settings['CLUSTER_CREDENTIALS'] = home / 'worker'
    settings['SUBAGENT_CREDENTIALS'] = home / 'primary'
    env = {**os.environ, 'BUZZ_DEVELOPER_ID': 'fixture', 'CODEX_HOME': str(home / 'codex')}
    env.pop('KUBECONFIG', None)
    return home, env

home, env = setup('no-mount')
configure(env)
require('KUBECONFIG' not in env and not settings['DIRECTORY'].exists(), 'unconfigured image gained cluster access')
for mode, selected in [('primary', 'SUBAGENT_CREDENTIALS'), ('worker', 'CLUSTER_CREDENTIALS')]:
    home, env = setup(mode)
    creds = settings[selected]
    creds.mkdir(mode=0o700)
    (creds / 'token').write_text(expected[0])
    (creds / 'ca.crt').write_bytes(cert.read_bytes())
    (creds / 'namespace').write_text('buzz-agent-staging-fixture\n')
    configure(env)
    config_path = Path(env['KUBECONFIG'])
    raw = config_path.read_text()
    config = json.loads(raw)
    require(config['current-context'] == 'voltage-staging', 'wrong default cluster context')
    require(config['clusters'][0]['cluster']['server'] == 'https://kubernetes.default.svc', 'wrong cluster server')
    require(config['users'][0]['user'] == {'tokenFile': str(creds / 'token')}, 'token was copied instead of referenced')
    require(expected[0] not in raw and config_path.stat().st_mode & 0o777 == 0o600, 'unsafe kubeconfig')
    require('Staging cluster diagnostics' in (home / 'codex/AGENTS.md').read_text(), 'runtime guide missing')
    command = ['kubectl', '--context=voltage-staging', '--server=https://127.0.0.1:' + str(server.server_port),
               '--tls-server-name=localhost', '--request-timeout=5s', 'get', '--raw=/api/v1/pods']
    first = subprocess.run(command, env=env, text=True, capture_output=True)
    require(first.returncode == 0 and json.loads(first.stdout)['kind'] == 'PodList', 'authenticated read failed: ' + first.stderr)
    expected[0] = 'synthetic-rotated-' + mode
    (creds / 'token').write_text(expected[0])
    second = subprocess.run(command, env=env, text=True, capture_output=True)
    require(second.returncode == 0 and seen[-1][1] == 'Bearer ' + expected[0], 'rotated token was not reread')
    wrong_peer = command.copy()
    wrong_peer[3] = '--tls-server-name=untrusted.example.invalid'
    refused = subprocess.run(wrong_peer, env=env, text=True, capture_output=True)
    require(refused.returncode != 0, 'TLS peer verification was bypassed')
    require('synthetic-' not in first.stdout + first.stderr + second.stdout + second.stderr + refused.stderr,
            'credential reached command output')
    print('PASS: ' + mode + ' TLS reads and token rotation')
home, env = setup('wrong-namespace')
creds = settings['CLUSTER_CREDENTIALS']; creds.mkdir()
for name, value in [('token', 'synthetic-other'), ('ca.crt', cert.read_text()), ('namespace', 'production')]:
    (creds / name).write_text(value)
try:
    configure(env)
except profile['Refused']:
    pass
else:
    raise RuntimeError('accepted another namespace as staging')
server.shutdown()
print('PASS: unavailable credentials and namespace mismatch')
'''

if __name__ == '__main__':
    result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                             '--entrypoint', 'python3', sys.argv[1] if len(sys.argv) > 1 else 'buzz-agent:local', '-'],
                            input=SCRIPT, text=True, timeout=90)
    raise SystemExit(result.returncode)
