"""buzz-subagent reaches an API server whose certificate has no key identifiers.

EKS serves kube-apiserver certificates without an Authority Key Identifier,
which Python 3.13's default VERIFY_X509_STRICT rejects. Runs with python3 and
the openssl CLI: the host makes a CA and leaf shaped like EKS's, then the
image's own Python serves HTTPS and calls the packaged client's api().

    python3 test-subagent-tls.py [IMAGE]    # without IMAGE, runs locally
"""
import http.server
import importlib.util
import json
import os
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import urllib.error
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load_client(path):
    spec = importlib.util.spec_from_loader('subagent', loader=None)
    module = importlib.util.module_from_spec(spec)
    exec(compile(Path(path).read_text(), str(path), 'exec'), module.__dict__)
    return module


def openssl(*args, cwd):
    subprocess.run(['openssl', *args], check=True, capture_output=True, cwd=cwd)


def authority(directory, name):
    """A CA and a 127.0.0.1 leaf without key identifiers, as EKS issues them."""
    directory.mkdir()
    (directory / 'ca.cnf').write_text(
        '[req]\ndistinguished_name=dn\n[dn]\n[ca]\nbasicConstraints=critical,CA:TRUE\n'
        'keyUsage=critical,keyCertSign\nsubjectKeyIdentifier=none\nauthorityKeyIdentifier=none\n')
    (directory / 'leaf.cnf').write_text(
        'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n'
        'subjectKeyIdentifier=none\nauthorityKeyIdentifier=none\n')
    openssl('req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
            '-keyout', 'ca.key', '-out', 'ca.crt', '-days', '1', '-subj', f'/CN={name}',
            '-config', 'ca.cnf', '-extensions', 'ca', cwd=directory)
    openssl('req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
            '-keyout', 'leaf.key', '-out', 'leaf.csr', '-subj', '/CN=kube-apiserver', cwd=directory)
    openssl('x509', '-req', '-in', 'leaf.csr', '-CA', 'ca.crt', '-CAkey', 'ca.key', '-CAcreateserial',
            '-out', 'leaf.crt', '-days', '1', '-extfile', 'leaf.cnf', cwd=directory)
    leaf = subprocess.run(['openssl', 'x509', '-in', 'leaf.crt', '-noout', '-text'],
                          check=True, capture_output=True, text=True, cwd=directory).stdout
    assert 'Key Identifier' not in leaf, 'the fixture must lack key identifiers'
    return directory


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({'kind': 'PodList', 'items': []}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def checks(root, client):
    subagent = load_client(client)
    cluster, stranger = root / 'cluster', root / 'stranger'
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    serving = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    serving.load_cert_chain(cluster / 'leaf.crt', cluster / 'leaf.key')
    server.socket = serving.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    credentials = Path(tempfile.mkdtemp())
    (credentials / 'token').write_text('synthetic-token\n')
    subagent.CREDENTIALS = credentials

    def call(ca, host='127.0.0.1'):
        (credentials / 'ca.crt').write_bytes((ca / 'ca.crt').read_bytes())
        os.environ['BUZZ_SUBAGENT_API'] = f'https://{host}:{server.server_address[1]}'
        return subagent.api('GET', 'api/v1/namespaces/n/pods')

    def refused(ca, host, reason):
        try:
            call(ca, host)
        except urllib.error.URLError as error:
            assert reason in str(error), error
        else:
            raise AssertionError(f'{host} via {ca.name} was accepted')

    # Strict verification, Python 3.13's default, refuses this certificate.
    strict = ssl.create_default_context(cafile=str(cluster / 'ca.crt'))
    strict.verify_flags |= ssl.VERIFY_X509_STRICT
    try:
        with strict.wrap_socket(socket.create_connection(server.server_address),
                                server_hostname='127.0.0.1'):
            raise AssertionError('strict verification accepted a certificate without an AKI')
    except ssl.SSLCertVerificationError as error:
        assert 'Authority Key Identifier' in str(error), error

    # The client reaches the API server through its cluster CA.
    assert call(cluster) == {'kind': 'PodList', 'items': []}

    # Verification stays on: a different CA with the same name is refused,
    # and so is a name the certificate does not cover (its SAN is 127.0.0.1).
    refused(stranger, '127.0.0.1', 'CERTIFICATE_VERIFY_FAILED')
    refused(cluster, 'localhost', 'Hostname mismatch')
    server.shutdown()
    print(f'PASS: buzz-subagent ({sys.version.split()[0]}) verifies an EKS-style API server '
          'certificate without an AKI, and still refuses an untrusted CA or another hostname')


if __name__ == '__main__':
    if sys.argv[1:2] == ['--checks']:
        checks(Path(sys.argv[2]), sys.argv[3])
        raise SystemExit(0)
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        authority(root / 'cluster', 'kubernetes')
        authority(root / 'stranger', 'kubernetes')
        # Synthetic keys: readable by the image's user, whatever the umask.
        subprocess.run(['chmod', '-R', 'a+rX', str(root)], check=True)
        if len(sys.argv) > 1:
            command = ['docker', 'run', '--rm', '--network', 'none',
                       '-v', f'{root}:/fixtures:ro', '-v', f'{HERE}:/test:ro',
                       '--entrypoint', 'python3', sys.argv[1], '/test/test-subagent-tls.py',
                       '--checks', '/fixtures', '/usr/local/bin/buzz-subagent']
        else:
            command = [sys.executable, __file__, '--checks', str(root), str(HERE / 'subagent.py')]
        raise SystemExit(subprocess.run(command, timeout=120).returncode)
