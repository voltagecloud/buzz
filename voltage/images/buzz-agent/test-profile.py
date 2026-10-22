"""Exercise developer enrollment and five isolated runtime bootstraps with local Secrets Manager."""
import json
import subprocess
import sys
import uuid

OWNER = 'a' * 64
PROFILE = {
    'version': 1, 'developer': 'alice', 'owner_pubkey': OWNER,
    'git': {'name': 'Alice Fixture', 'email': 'alice@example.invalid', 'ssh_signing_key': '-----BEGIN OPENSSH PRIVATE KEY-----\nc3ludGhldGljLWZpeHR1cmUta2V5\n-----END OPENSSH PRIVATE KEY-----\n'},
    'github': {'login': 'alice', 'token': 'synthetic-profile-github'},
    'via': {'profile': '[providers.fixture]\ntype="fixture"\n[services.fixture]\nprovider="fixture"\n'
                       '[services.depot]\nprovider="fixture"\n'
                       'secrets.organization="op://Fixture/Depot/organization"\n'
                       'secrets.token="op://Fixture/Depot/credential"\n',
            'op_token': 'synthetic-profile-op'},
    'models': {'openai_api_key': 'synthetic-profile-openai', 'anthropic_api_key': 'synthetic-profile-anthropic'},
}

COMMON = r'''
import json, os, subprocess, time
from pathlib import Path
def require(condition, message):
    if not condition: raise RuntimeError(message)
def run(*args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=60, **kwargs)
    require(result.returncode == 0, 'fixture command failed: '+args[0]+'; '+result.stderr[-2000:])
    return result.stdout
'''

SEED = COMMON + r'''
profile = json.loads(os.environ['FIXTURE_PROFILE'])
for attempt in range(30):
    ready = subprocess.run(['aws', 'secretsmanager', 'list-secrets'], capture_output=True, timeout=5)
    if ready.returncode == 0: break
    time.sleep(0.2)
else: raise RuntimeError('Secrets Manager fixture did not start')
# The Sandbox manager enrolls profiles (its tests cover refusals); seed one as it stores it.
path = Path('/tmp/profile.json')
path.write_text(json.dumps(profile))
path.chmod(0o600)
value = json.loads(run('aws', 'secretsmanager', 'create-secret', '--name',
                      'voltage/staging/agents/alice/profile', '--secret-string', 'file://'+str(path)))
print(json.dumps({'arn':value['ARN'], 'version':value['VersionId']}))
'''

BOOT = COMMON + r'''
mode = os.environ['FIXTURE_MODE']
env = dict(os.environ)
# The shared Depot credential comes from the vault through the via profile's depot service.
tools = Path('/tmp/boot-tools')
tools.mkdir()
(tools/'op').write_text('#!/bin/sh\n[ "$OP_SERVICE_ACCOUNT_TOKEN" = synthetic-profile-op ] || exit 1\n'
    + ('exit 1\n' if mode == 'no-depot' else '')
    + 'case "$3" in */organization) printf fixture-org;; */credential) printf synthetic-profile-depot;; *) exit 1;; esac\n')
(tools/'op').chmod(0o700)
env['PATH'] = str(tools)+':'+env['PATH']
def rotate(models, git=None):
    stored = json.loads(run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', env['BUZZ_DEVELOPER_PROFILE_ARN']))
    profile = json.loads(stored['SecretString'])
    profile['models'] = models
    if git is not None:
        profile['git'] = git
    path = Path('/tmp/rotated-profile.json')
    path.write_text(json.dumps(profile))
    path.chmod(0o600)
    return json.loads(run('aws', 'secretsmanager', 'put-secret-value', '--secret-id', stored['ARN'], '--secret-string', 'file://'+str(path)))
expected = {'OPENAI_API_KEY':'synthetic-profile-openai', 'ANTHROPIC_API_KEY':'synthetic-profile-anthropic',
            'CLAUDE_CODE_OAUTH_TOKEN':None, 'OPENROUTER_API_KEY':None, 'BUZZ_AGENT_PROVIDER':None,
            'SCCACHE_WEBDAV_PASSWORD':'synthetic-profile-depot', 'BUZZ_GIT_IDENTITY':'user'}
if mode == 'claude-oauth':
    rotate({'openai_api_key':'synthetic-profile-openai', 'claude_oauth_token':'synthetic-profile-claude-oauth'})
    expected.update(ANTHROPIC_API_KEY=None, CLAUDE_CODE_OAUTH_TOKEN='synthetic-profile-claude-oauth')
if mode == 'subscriptions':
    # Both subscriptions in one profile, for Codex and Claude Code in parallel, and
    # OpenRouter for the Buzz agent runtime.
    rotate({'codex_access_token':'at-synthetic-profile-codex', 'claude_oauth_token':'synthetic-profile-claude-oauth',
            'openrouter_api_key':'synthetic-profile-openrouter'})
    expected.update(OPENAI_API_KEY=None, ANTHROPIC_API_KEY=None, CODEX_ACCESS_TOKEN='at-synthetic-profile-codex',
                    CLAUDE_CODE_OAUTH_TOKEN='synthetic-profile-claude-oauth',
                    OPENROUTER_API_KEY='synthetic-profile-openrouter', BUZZ_AGENT_PROVIDER='openrouter')
if mode == 'no-depot':
    # An unreadable Depot credential turns the remote cache off; the session still starts.
    # Earlier modes rotated the shared fixture profile, so restore its original models.
    rotate({'openai_api_key':'synthetic-profile-openai', 'anthropic_api_key':'synthetic-profile-anthropic'})
    expected.update(SCCACHE_WEBDAV_PASSWORD=None)
if mode == 'inherited-codex': env['CODEX_ACCESS_TOKEN'] = 'at-synthetic-unselected-account'
# A stored profile the manager would not enroll is refused at startup too.
if mode == 'mixed-codex':
    rotate({'openai_api_key':'synthetic-profile-openai', 'codex_access_token':'at-synthetic-profile-codex'})
if mode == 'gpg-fingerprint':
    rotate({}, {'name': 'Alice Fixture', 'email': 'alice@example.invalid', 'signing_key': 'A' * 40})
if mode == 'inherited-openrouter': env['OPENROUTER_API_KEY'] = 'synthetic-unselected-account'
if mode == 'wrong-owner': env['BUZZ_ACP_AGENT_OWNER'] = 'b'*64
if mode == 'wrong-developer': env['BUZZ_DEVELOPER_ID'] = 'bob'
if mode == 'mixed-model': env['OPENAI_API_KEY'] = 'synthetic-unselected-account'
if mode == 'inherited-claude': env['CLAUDE_CODE_OAUTH_TOKEN'] = 'synthetic-unselected-account'
if mode == 'symlink':
    Path('/home/agent/.config').symlink_to('/tmp')
if mode == 'missing': env['BUZZ_DEVELOPER_PROFILE_ARN'] = env['BUZZ_DEVELOPER_PROFILE_ARN'].replace('/profile-', '/profile-missing-')
probe = Path('/tmp/profile-adapter')
probe.write_text(r"""#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
home = Path('/home/agent/.config/buzz-agent')
if home.joinpath('github-token').read_text() != 'synthetic-profile-github': raise RuntimeError('GitHub token missing')
for key, expected in EXPECTED.items():
    if os.environ.get(key) != expected: raise RuntimeError('runtime environment differs from profile')
if subprocess.check_output(['git','config','--global','user.email'], text=True).strip() != 'alice@example.invalid':
    raise RuntimeError('Git identity missing')
Path('/tmp/profile-adapter.ran').touch()
for line in sys.stdin:
    value = json.loads(line)
    if value.get('method') == 'initialize':
        print(json.dumps({'jsonrpc':'2.0','id':value['id'],'result':{'protocolVersion':2,'agentCapabilities':{},'authMethods':[]}}), flush=True)
""".replace('EXPECTED', repr(expected)))
probe.chmod(0o700)
result = subprocess.run(['sprig-entrypoint', 'auth-methods', '--agent-command', str(probe), '--agent-args', '', '--json'],
                        env=env, capture_output=True, text=True, timeout=45)
require('synthetic-profile-' not in result.stdout+result.stderr, 'credential reached output')
if mode not in ('valid', 'claude-oauth', 'subscriptions', 'no-depot'):
    require(result.returncode != 0 and not Path('/tmp/profile-adapter.ran').exists(), 'invalid profile started a runtime')
    print('PASS: refused '+mode)
else:
    require(result.returncode == 0 and Path('/tmp/profile-adapter.ran').exists(), 'runtime bootstrap failed')
    directory = Path('/home/agent/.config/buzz-agent')
    if mode == 'valid':
        require(json.loads((directory/'profile.json').read_text())['version_id'] == os.environ['FIXTURE_VERSION'], 'wrong profile revision')
    for path in directory.iterdir():
        require(path.stat().st_mode & 0o777 == 0o600, 'credential file permissions too broad')
    require(not list(Path('/home/agent/.gnupg').glob('private-keys-v1.d/*.key')), 'signing key copied to workspace')
    def contains(path, marker):
        # Codex creates helper symlinks to its binary outside the workspace.
        # Scan workspace files without following those links or loading whole files.
        if path.is_symlink() or not path.is_file():
            return False
        tail = b''
        with path.open('rb') as stream:
            while chunk := stream.read(65536):
                if marker in tail + chunk:
                    return True
                tail = chunk[-len(marker) + 1:]
        return False
    for marker in (b'synthetic-profile-claude-oauth', b'at-synthetic-profile-codex'):
        require(not any(contains(p, marker) for p in Path('/home/agent').rglob('*')),
                'subscription token written to the workspace')
    require((mode == 'no-depot') != (directory/'depot-token').exists(), 'Depot credential not loaded from the vault')
    print('PASS: fresh Sandbox bootstrap through packaged entrypoint and ACP adapter ('+mode+')')
'''


def main():
    image = sys.argv[1]
    network = 'profile-test-' + uuid.uuid4().hex[:8]
    server = network + '-secrets'
    try:
        subprocess.run(['docker', 'network', 'create', '--internal', network], check=True, capture_output=True)
        subprocess.run(['docker', 'run', '-d', '--rm', '--name', server, '--network', network,
                        'motoserver/moto:5.1.14@sha256:800719627036dc38dc699e9f314f624f2c98ac3d8bb754db078cb2698f08ee77'],
                       check=True, capture_output=True)
        base = ['docker', 'run', '--rm', '-i', '--network', network, '--read-only', '--cap-drop', 'ALL',
                '--security-opt', 'no-new-privileges', '--cpus', '2', '--memory', '1g',
                '--tmpfs', '/home/agent:uid=10001,gid=10001,mode=0700,size=128m',
                '--tmpfs', '/tmp:uid=10001,gid=10001,mode=0700,size=128m,exec',
                '-e', 'AWS_ACCESS_KEY_ID=testing', '-e', 'AWS_SECRET_ACCESS_KEY=testing',
                '-e', 'AWS_DEFAULT_REGION=us-west-2', '-e', 'AWS_EC2_METADATA_DISABLED=true',
                '-e', 'AWS_ENDPOINT_URL=http://'+server+':5000', '--entrypoint', 'python3']
        result = subprocess.run([*base, '-e', 'FIXTURE_PROFILE='+json.dumps(PROFILE), image, '-'],
                                input=SEED, text=True, capture_output=True, timeout=180)
        if result.returncode: raise RuntimeError('Enrollment fixture failed: '+result.stderr[-2000:])
        receipt = json.loads(result.stdout)
        print('PASS: seeded one enrolled developer profile', flush=True)
        modes = ['valid']*5 + ['wrong-owner', 'wrong-developer', 'mixed-model', 'inherited-claude',
                               'inherited-codex', 'inherited-openrouter', 'symlink', 'missing', 'claude-oauth',
                               'subscriptions', 'no-depot', 'mixed-codex', 'gpg-fingerprint']
        for mode in modes:
            result = subprocess.run([*base, '-e', 'BUZZ_DEVELOPER_PROFILE_ARN='+receipt['arn'],
                '-e', 'BUZZ_DEVELOPER_ID=alice', '-e', 'BUZZ_ACP_AGENT_OWNER='+OWNER,
                '-e', 'FIXTURE_VERSION='+receipt['version'], '-e', 'FIXTURE_MODE='+mode, image, '-'],
                input=BOOT, capture_output=True, text=True, timeout=120)
            if result.returncode: raise RuntimeError('Bootstrap fixture failed: '+mode+'; '+result.stderr[-2000:])
            print(result.stdout, end='', flush=True)
    finally:
        subprocess.run(['docker', 'stop', '--time', '1', server], capture_output=True)
        subprocess.run(['docker', 'network', 'rm', network], capture_output=True)


if __name__ == '__main__':
    main()
