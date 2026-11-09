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
(repo/'history.txt').write_text('shallow history\n')
run('git','-C',str(repo),'add','history.txt')
run('git','-C',str(repo),'-c','commit.gpgsign=false','commit','-m','test: create shallow boundary')
(repo/'.git/shallow').write_bytes(run('git','-C',str(repo),'rev-parse','HEAD'))
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
            details = {}
            for line in log:
                try:
                    fields = json.loads(line).get('fields', {})
                    if fields.get('schema') == 'buzz.activity.v1':
                        record = json.loads(fields['record'])
                        events.append(record['event'])
                        details[record['event']] = record.get('details', {})
                except json.JSONDecodeError:
                    pass
            if expected:
                assert events.index('checkpoint_started') < events.index('checkpoint_saved') < events.index('harness_stopped')
                return json.loads(receipt_path.read_text())
            assert 'checkpoint_failed' in events and 'checkpoint_saved' not in events
            assert not receipt_path.exists()
            return events, details
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
events, _ = stop_harness('nonexistent-checkpoint-fixture', False)
# The quarantine uploads to the same missing bucket, so it fails too.
assert events.index('checkpoint_failed') < events.index('checkpoint_quarantine_failed')
print('PASS: SIGTERM drains a writing adapter before verified storage; recovery preserves its final edit; upload failure exits nonzero and emits checkpoint_failed and checkpoint_quarantine_failed')

# A final save the secret scan refuses quarantines the workspace instead of
# leaving it only on the Pod's node. The credential is refused first while every
# repository would otherwise save, so the refusal is the scan's (exit 11).
leak = 'github_token = "ghp_'+'aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL3oR6'+'"\n'
(repo/'leak.txt').write_text(leak)
(repo/'.env').write_text('FIXTURE_TOKEN=never-packed\n')
refused = subprocess.run(['buzz-agent-checkpoint','save'],capture_output=True)
assert refused.returncode == 11, refused.returncode
# A repository without an origin, which a verified save also refuses, is
# bundled whole. Credential paths stay out.
scratch = Path('/home/agent/scratch')
scratch.mkdir()
run('git','-C',str(scratch),'init')
run('git','-C',str(scratch),'config','user.name','Checkpoint Fixture')
run('git','-C',str(scratch),'config','user.email','fixture@example.invalid')
(scratch/'notes.txt').write_text('local only\n')
run('git','-C',str(scratch),'add','.')
run('git','-C',str(scratch),'-c','commit.gpgsign=false','commit','-m','test: local-only fixture')
# A replace ref packs into the unvalidated bundle, and restore refuses it: the
# quarantine restore keeps this repository's files instead of failing whole.
replaced = Path('/home/agent/replaced')
replaced.mkdir()
run('git','-C',str(replaced),'init','-b','work')
run('git','-C',str(replaced),'config','user.name','Checkpoint Fixture')
run('git','-C',str(replaced),'config','user.email','fixture@example.invalid')
(replaced/'kept.txt').write_text('committed\n')
run('git','-C',str(replaced),'add','.')
run('git','-C',str(replaced),'-c','commit.gpgsign=false','commit','-m','test: replaced fixture')
substitute = run('git','-C',str(replaced),'commit-tree','HEAD^{tree}','-m','test: substitute').decode().strip()
run('git','-C',str(replaced),'replace','HEAD',substitute)
(replaced/'kept.txt').write_text('uncommitted\n')
(replaced/'new.txt').write_text('untracked\n')
# An orphan checkout leaves HEAD without a commit; the stash made before it
# lives only in the bundle beside the committed branch.
fresh = Path('/home/agent/fresh')
fresh.mkdir()
run('git','-C',str(fresh),'init','-b','base')
run('git','-C',str(fresh),'config','user.name','Checkpoint Fixture')
run('git','-C',str(fresh),'config','user.email','fixture@example.invalid')
(fresh/'seed.txt').write_text('committed\n')
run('git','-C',str(fresh),'add','.')
run('git','-C',str(fresh),'-c','commit.gpgsign=false','commit','-m','test: orphan base')
(fresh/'seed.txt').write_text('stashed\n')
run('git','-C',str(fresh),'stash','push')
run('git','-C',str(fresh),'checkout','--orphan','main')
(fresh/'draft.txt').write_text('never committed\n')
events, details = stop_harness(os.environ['BUZZ_CHECKPOINT_BUCKET'], False)
assert events.index('checkpoint_failed') < events.index('checkpoint_quarantined') < events.index('harness_stopped')
key = details['checkpoint_quarantined']['key']
session = os.environ['BUZZ_CHECKPOINT_PREFIX'] + os.environ['BUZZ_MANAGED_AGENT_START_NONCE'] + '/'
assert key.startswith(session + 'quarantine/') and key.count('/') == 3, key
assert not Path('/home/agent/.config/buzz-agent/checkpoint.json').exists(), 'a quarantine is never a verified receipt'
run('buzz-agent-checkpoint','recover','--key',key,'--destination','/home/agent/from-quarantine')
recovered = Path('/home/agent/from-quarantine')
assert (recovered/'example/leak.txt').read_text() == leak
assert not (recovered/'example/.env').exists()
assert (recovered/'scratch/notes.txt').read_text() == 'local only\n'
leftover = subprocess.run(['git','-C',str(recovered/'scratch'),'config','--get','remote.origin.url'],capture_output=True)
assert leftover.returncode != 0, 'the temporary bundle URL was left behind'
assert (recovered/'fresh/draft.txt').read_text() == 'never committed\n'
assert run('git','-C',str(recovered/'fresh'),'symbolic-ref','HEAD').strip() == b'refs/heads/main'
assert run('git','-C',str(recovered/'fresh'),'stash','list').count(b'\n') == 1
assert run('git','-C',str(recovered/'fresh'),'show','stash@{0}:seed.txt') == b'stashed\n'
assert run('git','-C',str(recovered/'scratch'),'log','--format=%s').strip() == b'test: local-only fixture'
assert (recovered/'replaced/kept.txt').read_text() == 'uncommitted\n'
assert (recovered/'replaced/new.txt').read_text() == 'untracked\n'
assert run('git','-C',str(recovered/'replaced'),'symbolic-ref','HEAD').strip() == b'refs/heads/work'
assert subprocess.run(['git','-C',str(recovered/'replaced'),'rev-parse','--verify','HEAD'],capture_output=True).returncode != 0
assert run('git','-C',str(recovered/'replaced'),'bundle','list-heads',str(recovered/'replaced/.git/quarantined-history.bundle'))
print('PASS: a refused final save quarantines the workspace outside the session prefix; explicit recovery restores the unscanned work, a repository without an origin, one without a commit and its stash, and the files of one whose history restore refuses, never .env')

# Only the key selects the unscanned restore: an archive stored under an
# ordinary session key takes the verified path, which refuses it and leaves
# nothing. The quarantine above fails on its unborn repository (exit 14); an
# unscanned archive of only the credential repository fails the restore-time
# scan (exit 11).
import base64, hashlib, importlib.machinery, importlib.util, shutil, uuid
def recover_as_ordinary(archive, destination):
    checksum = base64.b64encode(hashlib.sha256(archive.read_bytes()).digest()).decode()
    ordinary = session + str(uuid.uuid4()) + '.tar.gz'
    run('aws','s3api','put-object','--bucket',os.environ['BUZZ_CHECKPOINT_BUCKET'],'--key',ordinary,
        '--body',str(archive),'--checksum-algorithm','SHA256','--checksum-sha256',checksum)
    refused = subprocess.run(['buzz-agent-checkpoint','recover','--key',ordinary,'--destination',destination],
                             capture_output=True, text=True)
    assert not Path(destination).exists()
    return refused.returncode
archive = Path('/tmp/quarantine.tar.gz')
run('aws','s3api','get-object','--bucket',os.environ['BUZZ_CHECKPOINT_BUCKET'],'--key',key,str(archive))
assert recover_as_ordinary(archive, '/home/agent/ordinary') == 14
loader = importlib.machinery.SourceFileLoader('checkpoint', '/usr/local/bin/buzz-agent-checkpoint')
checkpoint = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(checkpoint)
shutil.copytree(repo, '/tmp/leak-workspace/example', symlinks=True)
checkpoint.pack(Path('/tmp/leak-workspace'), Path('/tmp/leak.tar.gz'), scanned=False)
assert recover_as_ordinary(Path('/tmp/leak.tar.gz'), '/home/agent/ordinary-leak') == 11
print('PASS: an ordinary checkpoint key takes the verified restore, which refuses an unborn repository and, by its scan, an unscanned credential')
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
