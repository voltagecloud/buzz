"""Exercise the packaged user Git configuration with a disposable test signer."""
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
    signer=Path(temp)/'signer'
    signer.mkdir(mode=0o700)
    run('gpg','--homedir',str(signer),'--batch','--pinentry-mode','loopback',
        '--passphrase','','--quick-generate-key','Signing Fixture <fixture@example.invalid>',
        'ed25519','sign','1d')
    public=run('gpg','--homedir',str(signer),'--armor','--export')
    listing=run('gpg','--homedir',str(signer),'--with-colons','--list-keys').decode()
    fingerprint=next(line.split(':')[9] for line in listing.splitlines() if line.startswith('fpr:'))
    env={**os.environ,'BUZZ_GIT_IDENTITY':'user','BUZZ_GIT_NAME':'Signing Fixture',
         'BUZZ_GIT_EMAIL':'fixture@example.invalid','BUZZ_GIT_SIGNING_KEY':fingerprint}
    run('buzz-agent-account','init',env=env)
    client=home/'.gnupg'
    run('gpg','--homedir',str(client),'--no-autostart','--import',input=public)
    extra=run('gpgconf','--homedir',str(signer),'--list-dirs','agent-extra-socket').decode().strip()
    (client/'S.gpg-agent').symlink_to(extra)
    repo=home/'fixture-repo'
    repo.mkdir()
    run('git','-C',str(repo),'init')
    (repo/'source.txt').write_text('signed fixture\n')
    run('git','-C',str(repo),'add','source.txt')
    run('git','-C',str(repo),'commit','-m','test: verify remote signer')
    run('git','-C',str(repo),'verify-commit','HEAD')
    identity=run('git','-C',str(repo),'log','-1','--format=%an|%ae|%cn|%ce|%GF').decode().strip()
    assert identity=='Signing Fixture|fixture@example.invalid|Signing Fixture|fixture@example.invalid|'+fingerprint
    assert not list((client/'private-keys-v1.d').glob('*.key'))
    (client/'S.gpg-agent').unlink()
    (repo/'source.txt').write_text('signer disconnected\n')
    run('git','-C',str(repo),'add','source.txt')
    failed=subprocess.run(['git','-C',str(repo),'commit','-m','test: require connected signer'],capture_output=True,timeout=15)
    assert failed.returncode != 0, 'missing signer produced an unsigned commit'
    print('PASS: author/committer identity and signature verified through an extra socket; workspace has no private key; signer loss refuses commits')
    run('gpgconf','--homedir',str(signer),'--kill','gpg-agent')
    # A profile Sandbox signs with the developer's agent-only SSH key file instead.
    key=Path(temp)/'agent-signing-key'
    run('ssh-keygen','-q','-t','ed25519','-N','','-C','fixture agents','-f',str(key))
    key.chmod(0o600)
    env={**os.environ,'BUZZ_GIT_IDENTITY':'user','BUZZ_GIT_NAME':'Signing Fixture',
         'BUZZ_GIT_EMAIL':'fixture@example.invalid','BUZZ_GIT_SIGNING_KEY':str(key),
         'BUZZ_GIT_SIGNING_FORMAT':'ssh'}
    run('buzz-agent-account','init',env=env)
    (repo/'source.txt').write_text('ssh signed fixture\n')
    run('git','-C',str(repo),'add','source.txt')
    run('git','-C',str(repo),'commit','-m','test: sign with the agent key')
    signers=Path(temp)/'allowed_signers'
    signers.write_text('fixture@example.invalid '+key.with_suffix('.pub').read_text())
    run('git','-C',str(repo),'-c','gpg.ssh.allowedSignersFile='+str(signers),'verify-commit','HEAD')
    status=json.loads(run('buzz-agent-account','status',env=env))
    assert status['signing_required'] is True
    print('PASS: commits are signed with the agent SSH key and verify against its public key')
'''

result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                         '--entrypoint', 'python3', sys.argv[1], '-'],
                        input=SCRIPT, text=True, capture_output=True, timeout=180)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
