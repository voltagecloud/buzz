"""Measure Depot Rust cache reuse across two disposable agent containers."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import time
import uuid


BUILD = r'''
import json, os, subprocess
from pathlib import Path

repo = Path('/home/agent/cache-proof')
(repo/'src').mkdir(parents=True)
(repo/'Cargo.toml').write_text('[package]\nname="depot-cache-proof"\nversion="0.1.0"\nedition="2024"\n')
(repo/'src/lib.rs').write_text('pub const PROOF: &str = "'+os.environ['CACHE_PROOF_NONCE']+'";\n')
def run(*args):
    result = subprocess.run(args, capture_output=True, text=True, timeout=150)
    if result.returncode:
        raise RuntimeError('Cache proof command failed: '+args[0]+'; output suppressed')
    return result.stdout
run('sccache', '--start-server')
try:
    run('cargo', 'build', '--offline', '--manifest-path', str(repo/'Cargo.toml'))
    data = json.loads(run('sccache', '--show-stats', '--stats-format=json'))['stats']
    def total(value):
        if isinstance(value, dict) and 'counts' in value: value = value['counts']
        if isinstance(value, dict): return sum(total(v) for v in value.values())
        return value if isinstance(value, (int, float)) else 0
    print(json.dumps({key:total(data.get(key, 0)) for key in
        ['cache_hits','cache_misses','cache_write_errors','cache_read_errors','cache_writes']}))
finally:
    run('sccache', '--stop-server')
'''

ENROLL = r'''
import json, os, subprocess, sys
from pathlib import Path
directory = Path('/home/agent/.config/buzz-agent')
directory.mkdir(parents=True, mode=0o700)
for name, value in {
    'depot.json':json.dumps({'organization':os.environ.pop('DEPOT_TEST_ORGANIZATION')}),
    'depot-token':os.environ.pop('DEPOT_TEST_TOKEN'),
}.items():
    path = directory/name
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as stream: stream.write(value)
result = subprocess.run(['buzz-agent-cache', 'python3', '-c', sys.stdin.read()], capture_output=True, text=True, timeout=210)
if result.returncode:
    raise SystemExit('Authenticated cache proof failed; command output suppressed')
print(result.stdout, end='')
'''


def credentials(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_mode & 0o077 or info.st_size > 65536:
        raise ValueError('Use a private, regular sccache environment file')
    result = {}
    for line in path.read_text().splitlines():
        match = re.fullmatch(r'\s*(?:export\s+)?(SCCACHE_WEBDAV_USERNAME|SCCACHE_WEBDAV_PASSWORD)=(.*)', line)
        if match:
            words = shlex.split(match[2], comments=True)
            if len(words) != 1:
                raise ValueError('The environment file must contain literal assignments')
            result[match[1]] = words[0]
    if not result.get('SCCACHE_WEBDAV_USERNAME') or not result.get('SCCACHE_WEBDAV_PASSWORD'):
        raise ValueError('The file must contain Depot organization and token assignments')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image')
    parser.add_argument('--depot-env', type=Path, required=True)
    args = parser.parse_args()
    config = credentials(args.depot_env)
    env = dict(os.environ, DEPOT_TEST_ORGANIZATION=config['SCCACHE_WEBDAV_USERNAME'],
               DEPOT_TEST_TOKEN=config['SCCACHE_WEBDAV_PASSWORD'], CACHE_PROOF_NONCE=uuid.uuid4().hex)
    results = []
    for phase in ['cold', 'fresh-session']:
        started = time.monotonic()
        result = subprocess.run(['docker', 'run', '--rm', '-i', '--read-only',
            '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges', '--cpus', '2', '--memory', '2g',
            '--tmpfs', '/home/agent:uid=10001,gid=10001,mode=0700,size=1g',
            '--tmpfs', '/tmp:uid=10001,gid=10001,mode=0700,size=256m',
            '-e', 'DEPOT_TEST_ORGANIZATION', '-e', 'DEPOT_TEST_TOKEN', '-e', 'CACHE_PROOF_NONCE',
            '--entrypoint', 'python3', args.image, '-c', ENROLL],
            input=BUILD, env=env, capture_output=True, text=True, timeout=240)
        if result.returncode:
            raise RuntimeError(phase+' cache test failed; credential-bearing output suppressed')
        stats = json.loads(result.stdout)
        stats['seconds'] = round(time.monotonic()-started, 2)
        results.append(stats)
        print(json.dumps({'phase':phase, **stats}), flush=True)
    if results[0]['cache_misses'] < 1 or results[0]['cache_writes'] < 1:
        raise RuntimeError('Cold build did not populate the remote cache')
    if results[1]['cache_hits'] < 1:
        raise RuntimeError('Fresh container did not reuse the Depot cache')
    if any(result['cache_read_errors'] or result['cache_write_errors'] for result in results):
        raise RuntimeError('Depot reported a cache read or write failure')
    print('PASS: fresh agent container reused the authenticated Depot Rust cache')


if __name__ == '__main__':
    main()
