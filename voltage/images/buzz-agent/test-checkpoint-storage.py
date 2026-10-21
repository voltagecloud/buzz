"""Exercise the packaged AWS CLI checkpoint round trip against a local S3 emulator."""
import subprocess
import sys
import uuid

SCRIPT = r'''
import json, os, signal, subprocess, time
from pathlib import Path

def run(*command):
    result=subprocess.run(command,capture_output=True,timeout=90)
    if result.returncode:
        raise AssertionError('fixture command failed: '+command[0]+', output suppressed')
    return result.stdout

for attempt in range(30):
    ready=subprocess.run(['aws','s3api','list-buckets'],capture_output=True,timeout=5)
    if ready.returncode == 0: break
    time.sleep(0.2)
else: raise AssertionError('S3 emulator did not start')
run('aws','s3api','create-bucket','--bucket',os.environ['BUZZ_CHECKPOINT_BUCKET'],
    '--create-bucket-configuration','LocationConstraint=us-west-2')
repo=Path('/home/agent/example')
repo.mkdir()
run('git','-C',str(repo),'init')
run('git','-C',str(repo),'config','user.name','Checkpoint Fixture')
run('git','-C',str(repo),'config','user.email','fixture@example.invalid')
run('git','-C',str(repo),'remote','add','origin','https://github.com/example/fixture.git')
(repo/'source.txt').write_text('initial\n')
run('git','-C',str(repo),'add','.')
run('git','-C',str(repo),'-c','commit.gpgsign=false','commit','-m','test: initialize fixture')
(repo/'source.txt').write_text('edited\n')
receipt=json.loads(run('buzz-agent-checkpoint','save'))
assert receipt['repositories']==1
assert Path('/home/agent/.config/buzz-agent/checkpoint.json').stat().st_mode & 0o777 == 0o600
os.environ['BUZZ_MANAGED_AGENT_START_NONCE']='different-session'
run('buzz-agent-checkpoint','recover','--key',receipt['key'],'--destination','/home/agent/recovered')
assert Path('/home/agent/recovered/example/source.txt').read_text()=='edited\n'
foreign=receipt['key'].replace('fixture/','other-developer/',1)
result=subprocess.run(['buzz-agent-checkpoint','recover','--key',foreign,'--destination','/home/agent/foreign'],capture_output=True)
assert result.returncode != 0 and not Path('/home/agent/foreign').exists()
print('PASS: packaged AWS CLI upload, server checksum verification, and explicit recovery into a new session; foreign developer prefix refused locally')

# Run the packaged harness, not just its helper. The adapter keeps editing until
# the harness kills it; the checkpoint must contain its final edit after drain.
adapter = Path('/tmp/checkpoint-adapter')
adapter.write_text(r"""#!/usr/bin/env python3
import json, sys, threading, time
from pathlib import Path
def edit():
    number = 0
    source = Path('/home/agent/example/source.txt')
    while True:
        number += 1
        source.with_suffix('.next').write_text(str(number)+'\n')
        source.with_suffix('.next').replace(source)
        time.sleep(0.01)
threading.Thread(target=edit, daemon=True).start()
for line in sys.stdin:
    message = json.loads(line)
    if message.get('method') == 'initialize':
        result = {'protocolVersion':2,'agentCapabilities':{},'authMethods':[]}
        Path('/tmp/checkpoint-adapter.ready').touch()
    elif message.get('method') == 'session/new':
        result = {'sessionId':'checkpoint-fixture'}
    else:
        result = {}
    if 'id' in message:
        print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
""")
adapter.chmod(0o700)

def stop_harness(bucket, expected):
    ready = Path('/tmp/checkpoint-adapter.ready')
    ready.unlink(missing_ok=True)
    receipt_path = Path('/home/agent/.config/buzz-agent/checkpoint.json')
    receipt_path.unlink(missing_ok=True)
    env = dict(os.environ, BUZZ_PRIVATE_KEY='0'*63+'1', BUZZ_RELAY_URL='wss://relay.invalid',
               BUZZ_ACP_AGENT_COMMAND=str(adapter), BUZZ_ACP_AGENT_ARGS='',
               BUZZ_ACP_LAZY_POOL='false', BUZZ_ACP_CHECKPOINT_ON_STOP='true',
               BUZZ_ACP_ACTIVITY_LOG='true', BUZZ_CHECKPOINT_BUCKET=bucket)
    with open('/tmp/checkpoint-harness.log', 'w+') as log:
        process = subprocess.Popen(['/usr/local/bin/sprig-entrypoint'], env=env,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        try:
            deadline = time.monotonic()+20
            while not ready.exists() and time.monotonic() < deadline:
                if process.poll() is not None: break
                time.sleep(0.05)
            if not ready.exists(): raise AssertionError('checkpoint adapter did not initialize')
            time.sleep(0.1)
            process.send_signal(signal.SIGTERM)
            code = process.wait(timeout=150)
            if (code == 0) != expected: raise AssertionError('checkpoint shutdown returned the wrong status')
            log.seek(0)
            events = []
            for line in log:
                try:
                    fields = json.loads(line).get('fields', {})
                    if fields.get('schema') == 'buzz.activity.v1':
                        events.append(json.loads(fields['record'])['event'])
                except json.JSONDecodeError:
                    pass
            if expected:
                assert events.index('checkpoint_started') < events.index('checkpoint_saved') < events.index('harness_stopped')
                return json.loads(receipt_path.read_text())
            assert 'checkpoint_failed' in events and 'checkpoint_saved' not in events
            assert not receipt_path.exists()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

receipt = stop_harness(os.environ['BUZZ_CHECKPOINT_BUCKET'], True)
final_edit = (repo/'source.txt').read_text()
time.sleep(0.1)
assert final_edit == (repo/'source.txt').read_text(), 'adapter survived harness shutdown'
run('buzz-agent-checkpoint','recover','--key',receipt['key'],'--destination','/home/agent/after-stop')
assert Path('/home/agent/after-stop/example/source.txt').read_text() == final_edit
stop_harness('nonexistent-checkpoint-fixture', False)
print('PASS: SIGTERM drains a writing adapter before verified storage; recovery preserves its final edit; upload failure exits nonzero and emits checkpoint_failed')
'''

name = 'checkpoint-test-' + uuid.uuid4().hex[:8]
server = name + '-s3'
try:
    subprocess.run(['docker', 'network', 'create', '--internal', name], check=True, capture_output=True)
    subprocess.run(['docker', 'run', '-d', '--rm', '--name', server, '--network', name,
                    'motoserver/moto:5.1.14@sha256:800719627036dc38dc699e9f314f624f2c98ac3d8bb754db078cb2698f08ee77'],
                   check=True, capture_output=True)
    result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', name,
                             '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                             '-e', 'AWS_ACCESS_KEY_ID=testing', '-e', 'AWS_SECRET_ACCESS_KEY=testing',
                             '-e', 'AWS_DEFAULT_REGION=us-west-2', '-e', 'AWS_EC2_METADATA_DISABLED=true',
                             '-e', 'AWS_ENDPOINT_URL=http://' + server + ':5000',
                             '-e', 'BUZZ_CHECKPOINT_BUCKET=agent-checkpoint-fixture',
                             '-e', 'BUZZ_CHECKPOINT_PREFIX=fixture/',
                             '-e', 'BUZZ_MANAGED_AGENT_START_NONCE=fixture-session',
                             '--entrypoint', 'python3', sys.argv[1], '-'],
                            input=SCRIPT, text=True, capture_output=True, timeout=420)
    print(result.stdout, end='')
    print(result.stderr, end='', file=sys.stderr)
    sys.exit(result.returncode)
finally:
    subprocess.run(['docker', 'rm', '-f', server], capture_output=True)
    subprocess.run(['docker', 'network', 'rm', name], capture_output=True)
