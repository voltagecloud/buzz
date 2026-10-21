"""Verify packaged cache clients and remote sccache reuse without real credentials."""
import subprocess
import sys

SCRIPT = r'''
import base64, http.server, json, os, pathlib, shutil, subprocess, threading

def run(*args, env=None):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=90)
    if result.returncode:
        raise RuntimeError('cache fixture failed: '+args[0]+'; command output suppressed')
    return result.stdout

account = pathlib.Path('/home/agent/.config/buzz-agent')
account.mkdir(mode=0o700, parents=True)
token = 'synthetic-depot-token'
(account/'depot-token').write_text(token)
(account/'depot.json').write_text(json.dumps({'organization':'fixture-org'}))
for path in account.iterdir(): path.chmod(0o600)
check = """
import os, subprocess
assert os.environ['SCCACHE_WEBDAV_ENDPOINT']=='https://cache.depot.dev'
assert os.environ['SCCACHE_WEBDAV_USERNAME']=='fixture-org'
assert os.environ['SCCACHE_WEBDAV_PASSWORD']=='synthetic-depot-token'
assert os.environ['TURBO_TEAM']=='fixture-org'
assert os.environ['TURBO_TOKEN']=='synthetic-depot-token'
assert os.environ['DEPOT_TOKEN']=='synthetic-depot-token'
assert os.environ['GOCACHEPROG']=='depot gocache --organization fixture-org'
assert os.environ['RUSTC_WRAPPER']=='/usr/local/bin/sccache'
subprocess.run(['depot','--version'], check=True, capture_output=True)
subprocess.run(['depot','gocache','--help'], check=True, capture_output=True)
version=subprocess.check_output(['go','version'],text=True).split()[2][2:].split('.')
assert tuple(map(int, version[:2])) >= (1,24)
"""
run('buzz-agent-cache', 'python3', '-c', check)

cache = {}
hits = []
class WebDAV(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def authorized(self):
        expected = 'Basic '+base64.b64encode(b'fixture-org:synthetic-depot-token').decode()
        if self.headers.get('Authorization') != expected:
            self.send_response(401); self.end_headers(); return False
        return True
    def do_GET(self):
        if not self.authorized(): return
        value = cache.get(self.path)
        self.send_response(200 if value is not None else 404)
        self.send_header('Content-Length', str(len(value or b'')))
        self.end_headers()
        if value is not None:
            hits.append(self.path)
            self.wfile.write(value)
    def do_PUT(self):
        if not self.authorized(): return
        cache[self.path] = self.rfile.read(int(self.headers.get('Content-Length',0)))
        self.send_response(201); self.send_header('Content-Length','0'); self.end_headers()
    def do_MKCOL(self):
        if not self.authorized(): return
        self.send_response(201); self.send_header('Content-Length','0'); self.end_headers()
    def do_HEAD(self):
        if not self.authorized(): return
        self.send_response(200 if self.path.endswith('/') or self.path in cache else 404)
        self.send_header('Content-Length',str(len(cache.get(self.path,b''))))
        self.end_headers()
    def do_PROPFIND(self):
        if not self.authorized(): return
        body = (f'<?xml version="1.0"?><d:multistatus xmlns:d="DAV:"><d:response>'
                f'<d:href>{self.path}</d:href><d:propstat><d:prop>'
                '<d:resourcetype><d:collection/></d:resourcetype>'
                '<d:getlastmodified>Thu, 08 Oct 2026 12:00:00 GMT</d:getlastmodified>'
                '<d:getcontentlength>0</d:getcontentlength></d:prop>'
                '<d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>').encode()
        self.send_response(207); self.send_header('Content-Type','application/xml')
        self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), WebDAV)
threading.Thread(target=server.serve_forever, daemon=True).start()
env = {**os.environ, 'SCCACHE_WEBDAV_ENDPOINT':f'http://127.0.0.1:{server.server_port}',
       'SCCACHE_WEBDAV_USERNAME':'fixture-org','SCCACHE_WEBDAV_PASSWORD':token,
       'CARGO_INCREMENTAL':'0','RUSTC_WRAPPER':'/usr/local/bin/sccache',
       'SCCACHE_DIR':'/tmp/cache-fixture'}
repo = pathlib.Path('/home/agent/cache-fixture')
repo.mkdir(); (repo/'src').mkdir()
(repo/'Cargo.toml').write_text('[package]\nname="cache-fixture"\nversion="0.1.0"\nedition="2021"\n')
(repo/'src/lib.rs').write_text('pub fn answer() -> u64 { 42 }\n')
run('sccache','--start-server',env=env)
run('cargo','build','--offline','--manifest-path',str(repo/'Cargo.toml'),env=env)
run('sccache','--stop-server',env=env)
if not cache: raise RuntimeError('cold build did not populate remote WebDAV cache')
shutil.rmtree(repo/'target')
shutil.rmtree('/tmp/cache-fixture',ignore_errors=True)
before = len(hits)
run('sccache','--start-server',env=env)
run('cargo','build','--offline','--manifest-path',str(repo/'Cargo.toml'),env=env)
run('sccache','--stop-server',env=env)
if len(hits) <= before: raise RuntimeError('warm build did not read remote WebDAV cache')
server.shutdown()
print('PASS: Depot/Turbo/Go runtime configuration; pinned clients; cold Rust upload and fresh-build remote hit against an authenticated WebDAV fixture')
'''

result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                         '--entrypoint', 'python3', sys.argv[1], '-'],
                        input=SCRIPT, text=True, capture_output=True, timeout=300)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
