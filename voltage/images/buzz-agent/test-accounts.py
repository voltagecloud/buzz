"""Exercise the packaged user Git configuration with a disposable agent signing key."""
import subprocess
import sys

SCRIPT = r'''
import json, os, subprocess, tempfile
from pathlib import Path

def run(*args,input=None,env=None):
    result=subprocess.run(args,input=input,env=env,capture_output=True,timeout=45)
    if result.returncode:
        raise AssertionError('fixture command failed: '+args[0]+', output suppressed')
    return result.stdout

home=Path('/home/agent')
with tempfile.TemporaryDirectory() as temp:
    # A profile Sandbox signs with the developer's agent-only SSH key file.
    key=Path(temp)/'agent-signing-key'
    run('ssh-keygen','-q','-t','ed25519','-N','','-C','fixture agents','-f',str(key))
    key.chmod(0o600)
    env={**os.environ,'BUZZ_GIT_IDENTITY':'user','BUZZ_GIT_NAME':'Signing Fixture',
         'BUZZ_GIT_EMAIL':'fixture@example.invalid','BUZZ_GIT_SIGNING_KEY':str(key)}
    run('buzz-agent-account','init',env=env)
    repo=home/'fixture-repo'
    repo.mkdir()
    run('git','-C',str(repo),'init')
    (repo/'source.txt').write_text('signed fixture\n')
    run('git','-C',str(repo),'add','source.txt')
    run('git','-C',str(repo),'commit','-m','test: sign with the agent key')
    signers=Path(temp)/'allowed_signers'
    signers.write_text('fixture@example.invalid '+key.with_suffix('.pub').read_text())
    run('git','-C',str(repo),'-c','gpg.ssh.allowedSignersFile='+str(signers),'verify-commit','HEAD')
    identity=run('git','-C',str(repo),'log','-1','--format=%an|%ae|%cn|%ce').decode().strip()
    assert identity=='Signing Fixture|fixture@example.invalid|Signing Fixture|fixture@example.invalid'
    status=json.loads(run('buzz-agent-account','status',env=env))
    assert status['signing_required'] is True
    # Without the key, Git refuses to commit rather than commit unsigned.
    key.unlink()
    (repo/'source.txt').write_text('key removed\n')
    run('git','-C',str(repo),'add','source.txt')
    failed=subprocess.run(['git','-C',str(repo),'commit','-m','test: require the signing key'],capture_output=True,timeout=15)
    assert failed.returncode != 0, 'a missing signing key produced an unsigned commit'
    print('PASS: author/committer identity and SSH signature verified; a missing key refuses commits')
'''

result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                         '--entrypoint', 'python3', sys.argv[1], '-'],
                        input=SCRIPT, text=True, capture_output=True, timeout=180)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
