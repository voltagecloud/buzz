"""Exercise the packaged restore-once startup step against a local S3 and Secrets Manager emulator."""
import json
import subprocess
import sys
import uuid

OWNER = 'a' * 64
PROFILE = {
    'version': 1, 'developer': 'fixture', 'owner_pubkey': OWNER,
    'git': {'name': 'Restore Fixture', 'email': 'fixture@example.invalid', 'ssh_signing_key': '-----BEGIN OPENSSH PRIVATE KEY-----\nc3ludGhldGljLWZpeHR1cmUta2V5\n-----END OPENSSH PRIVATE KEY-----\n'},
    'github': {'login': 'fixture', 'token': 'synthetic-profile-github'},
    'via': {'profile': '[providers.fixture]\ntype="fixture"\n[services.fixture]\nprovider="fixture"\n',
            'op_token': 'synthetic-profile-op'},
    'models': {},
}

COMMON = r'''
import json, os, subprocess, time
from pathlib import Path
def require(condition, message):
    if not condition: raise RuntimeError(message)
def run(*args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=120, **kwargs)
    require(result.returncode == 0, 'fixture command failed: '+args[0]+'; '+result.stderr[-2000:])
    return result.stdout
'''

SEED = COMMON + r'''
for attempt in range(30):
    ready = subprocess.run(['aws', 's3api', 'list-buckets'], capture_output=True, timeout=5)
    if ready.returncode == 0: break
    time.sleep(0.2)
else: raise RuntimeError('AWS emulator did not start')
run('aws', 's3api', 'create-bucket', '--bucket', os.environ['BUZZ_CHECKPOINT_BUCKET'],
    '--create-bucket-configuration', 'LocationConstraint=us-west-2')
profile = Path('/tmp/profile.json')
profile.write_text(os.environ['FIXTURE_PROFILE'])
profile.chmod(0o600)
secret = json.loads(run('aws', 'secretsmanager', 'create-secret', '--name', 'voltage/staging/agents/fixture/profile',
                        '--secret-string', 'file://'+str(profile)))
keys = {}
for developer in ('fixture', 'other-developer'):
    workspace = Path('/tmp/source-'+developer)
    repo = workspace/'example'
    repo.mkdir(parents=True)
    run('git', '-C', str(repo), 'init')
    run('git', '-C', str(repo), 'config', 'user.name', 'Restore Fixture')
    run('git', '-C', str(repo), 'config', 'user.email', 'fixture@example.invalid')
    run('git', '-C', str(repo), 'remote', 'add', 'origin', 'https://github.com/example/fixture.git')
    (repo/'source.txt').write_text('initial\n')
    run('git', '-C', str(repo), 'add', '.')
    run('git', '-C', str(repo), '-c', 'commit.gpgsign=false', 'commit', '-m', 'test: initialize fixture')
    (repo/'source.txt').write_text('edited\n')
    env = dict(os.environ, BUZZ_CHECKPOINT_PREFIX=developer+'/')
    keys[developer] = json.loads(run('buzz-agent-checkpoint', 'save', '--workspace', str(workspace), env=env))['key']
print(json.dumps({'arn': secret['ARN'], 'key': keys['fixture'], 'foreign': keys['other-developer']}))
'''

BOOT = COMMON + r'''
mode = os.environ['FIXTURE_MODE']
fixture = json.loads(os.environ['FIXTURE_KEYS'])
home = Path('/home/agent')
marker = home/'.config/buzz-agent/checkpoint-restored.json'
probe = Path('/tmp/restore-adapter')
probe.write_text(r"""#!/usr/bin/env python3
import json, sys
from pathlib import Path
source = Path('/home/agent/example/source.txt')
Path('/tmp/restore-adapter.json').write_text(json.dumps({'source': source.read_text() if source.exists() else None}))
for line in sys.stdin:
    value = json.loads(line)
    if value.get('method') == 'initialize':
        print(json.dumps({'jsonrpc':'2.0','id':value['id'],'result':{'protocolVersion':2,'agentCapabilities':{},'authMethods':[]}}), flush=True)
""")
probe.chmod(0o700)
UNREACHABLE = 'http://127.0.0.1:9'

def start(key, profile=False, **extra):
    Path('/tmp/restore-adapter.json').unlink(missing_ok=True)
    env = {k: v for k, v in os.environ.items() if not k.startswith(('FIXTURE_', 'BUZZ_DEVELOPER_'))}
    env.update(BUZZ_CHECKPOINT_RESTORE_KEY=key, **extra)
    if profile:
        env.update(BUZZ_DEVELOPER_ID='fixture', BUZZ_DEVELOPER_PROFILE_ARN=fixture['arn'], BUZZ_ACP_AGENT_OWNER='a'*64)
    result = subprocess.run(['sprig-entrypoint', 'auth-methods', '--agent-command', str(probe), '--agent-args', '', '--json'],
                            env=env, capture_output=True, text=True, timeout=300)
    require('synthetic-profile-' not in result.stdout+result.stderr, 'credential reached output')
    ran = Path('/tmp/restore-adapter.json')
    return result.returncode, json.loads(ran.read_text())['source'] if ran.exists() else 'not-started'

def untouched():
    require(not (home/'example').exists(), 'refused restore changed the workspace')
    require(not marker.exists(), 'refused restore wrote the marker')
    require(not list(home.glob('.checkpoint-restore-*')), 'refused restore left staging content')

def restore_once(key, **extra):
    return subprocess.run(['buzz-agent-checkpoint', 'restore-once', '--key', key],
                          env=dict(os.environ, **extra), capture_output=True, text=True, timeout=300)

if mode == 'profile':
    code, source = start(fixture['key'], profile=True)
    require(code == 0, 'profile startup with a restore key failed')
    require(source == 'edited\n', 'harness started before the checkpoint was restored')
    require(json.loads(marker.read_text()) == {'version': 1, 'key': fixture['key']}, 'restore marker missing')
    require(marker.stat().st_mode & 0o777 == 0o600, 'restore marker permissions too broad')
    require(not list(home.glob('.checkpoint-restore-*')), 'restore left staging content')
    print('PASS: developer-profile startup restores the requested checkpoint before the harness')
elif mode == 'profile-missing':
    missing = fixture['key'].rsplit('/', 1)[0]+'/00000000-0000-4000-8000-000000000000.tar.gz'
    code, source = start(missing, profile=True)
    require(code != 0 and source == 'not-started', 'failed restore did not abort startup')
    untouched()
    print('PASS: restore failure aborts developer-profile startup without starting the harness')
elif mode == 'plain':
    code, source = start(fixture['key'])
    require(code == 0 and source == 'edited\n', 'startup did not restore the checkpoint')
    (home/'example/source.txt').write_text('newer\n')
    # A restart must not contact S3 or replace newer work.
    code, source = start(fixture['key'], AWS_ENDPOINT_URL=UNREACHABLE)
    require(code == 0 and source == 'newer\n', 'second start restored over newer edits')
    result = restore_once(fixture['foreign'].replace('other-developer/', 'fixture/', 1), AWS_ENDPOINT_URL=UNREACHABLE)
    require(result.returncode != 0 and (home/'example/source.txt').read_text() == 'newer\n',
            'marker accepted a different checkpoint')
    print('PASS: restore runs once; a restart with the marker skips it and keeps newer edits')
elif mode == 'empty':
    code, source = start('', AWS_ENDPOINT_URL=UNREACHABLE)
    require(code == 0 and source is None, 'empty restore key changed startup')
    untouched()
    result = restore_once('', AWS_ENDPOINT_URL=UNREACHABLE)
    require(result.returncode == 0 and json.loads(result.stdout)['restored'] is None, 'empty key did not skip')
    untouched()
    print('PASS: empty restore key skips restore without contacting S3')
elif mode == 'foreign':
    code, source = start(fixture['foreign'])
    require(code != 0 and source == 'not-started', 'foreign checkpoint started the harness')
    untouched()
    for key in (fixture['foreign'], 'fixture/../'+fixture['foreign'], '/'+fixture['key']):
        result = restore_once(key)
        require(result.returncode != 0, 'out-of-prefix checkpoint key was accepted')
    untouched()
    print('PASS: checkpoint keys outside the developer prefix are refused and abort startup')
else:
    raise RuntimeError('unknown mode')
'''


def main():
    image = sys.argv[1]
    network = 'restore-test-' + uuid.uuid4().hex[:8]
    server = network + '-aws'
    try:
        subprocess.run(['docker', 'network', 'create', '--internal', network], check=True, capture_output=True)
        subprocess.run(['docker', 'run', '-d', '--rm', '--name', server, '--network', network,
                        'motoserver/moto:5.1.14@sha256:800719627036dc38dc699e9f314f624f2c98ac3d8bb754db078cb2698f08ee77'],
                       check=True, capture_output=True)
        base = ['docker', 'run', '--rm', '-i', '--network', network, '--read-only', '--cap-drop', 'ALL',
                '--security-opt', 'no-new-privileges', '--cpus', '2', '--memory', '1g',
                '--tmpfs', '/home/agent:uid=10001,gid=10001,mode=0700,size=128m',
                '--tmpfs', '/tmp:uid=10001,gid=10001,mode=0700,size=256m,exec',
                '-e', 'AWS_ACCESS_KEY_ID=testing', '-e', 'AWS_SECRET_ACCESS_KEY=testing',
                '-e', 'AWS_DEFAULT_REGION=us-west-2', '-e', 'AWS_EC2_METADATA_DISABLED=true',
                '-e', 'AWS_ENDPOINT_URL=http://'+server+':5000',
                '-e', 'BUZZ_CHECKPOINT_BUCKET=agent-checkpoint-fixture',
                '-e', 'BUZZ_CHECKPOINT_PREFIX=fixture/', '--entrypoint', 'python3']
        result = subprocess.run([*base, '-e', 'BUZZ_MANAGED_AGENT_START_NONCE=old-session-0001',
                                 '-e', 'FIXTURE_PROFILE='+json.dumps(PROFILE), image, '-'],
                                input=SEED, text=True, capture_output=True, timeout=300)
        if result.returncode: raise RuntimeError('Seed fixture failed: '+result.stderr[-2000:])
        keys = result.stdout.strip()
        for mode in ('profile', 'profile-missing', 'plain', 'empty', 'foreign'):
            # A recovered session is a new generation that restores the previous one.
            result = subprocess.run([*base, '-e', 'BUZZ_MANAGED_AGENT_START_NONCE=new-session-0001',
                                     '-e', 'FIXTURE_KEYS='+keys, '-e', 'FIXTURE_MODE='+mode, image, '-'],
                                    input=BOOT, capture_output=True, text=True, timeout=600)
            if result.returncode: raise RuntimeError('Restore fixture failed: '+mode+'; '+result.stderr[-2000:])
            print(result.stdout, end='', flush=True)
    finally:
        subprocess.run(['docker', 'stop', '--time', '1', server], capture_output=True)
        subprocess.run(['docker', 'network', 'rm', network], capture_output=True)


if __name__ == '__main__':
    main()
