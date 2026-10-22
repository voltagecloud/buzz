"""Exercise developer enrollment and five isolated runtime bootstraps with local Secrets Manager."""
import json
import subprocess
import sys
import uuid

OWNER = 'a' * 64
PROFILE = {
    'version': 1, 'developer': 'alice', 'owner_pubkey': OWNER,
    'git': {'name': 'Alice Fixture', 'email': 'alice@example.invalid', 'signing_key': 'A' * 40},
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
value = json.loads(run('aws', 'secretsmanager', 'create-secret', '--name',
                      'voltage/staging/agents/alice/profile'))
path = Path('/tmp/profile.json')
path.write_text(json.dumps(profile))
path.chmod(0o600)
tools = Path('/tmp/account-tools')
tools.mkdir()
for name, body in {'gh':'printf "alice\\n"', 'op':'printf "{}\\n"'}.items():
    tool = tools/name
    tool.write_text('#!/bin/sh\n'+body+'\n')
    tool.chmod(0o700)
env = {**os.environ, 'PATH':str(tools)+':'+os.environ['PATH']}
receipt = json.loads(run('buzz-agent-profile', 'enroll', '--arn', value['ARN'], '--file', str(path), env=env))
require(receipt['developer'] == 'alice', 'wrong enrollment receipt')
before = run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', value['ARN'])
# An enrollment error must not change the current profile.
(tools/'gh').write_text('#!/bin/sh\nprintf "someone-else\\n"\n')
failed = subprocess.run(['buzz-agent-profile', 'enroll', '--arn', value['ARN'], '--file', str(path)],
                        env=env, capture_output=True, text=True, timeout=60)
require(failed.returncode != 0 and 'synthetic-profile-' not in failed.stdout+failed.stderr, 'wrong actor was enrolled or leaked')
after = run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', value['ARN'])
require(json.loads(before)['VersionId'] == json.loads(after)['VersionId'], 'failed enrollment changed the profile')
# Claude Code would prefer the API key, so a profile cannot carry both Claude credentials.
(tools/'gh').write_text('#!/bin/sh\nprintf "alice\\n"\n')
both = dict(profile, models={'anthropic_api_key':'synthetic-profile-anthropic', 'claude_oauth_token':'synthetic-profile-claude-oauth'})
path.write_text(json.dumps(both))
failed = subprocess.run(['buzz-agent-profile', 'enroll', '--arn', value['ARN'], '--file', str(path)],
                        env=env, capture_output=True, text=True, timeout=60)
require(failed.returncode != 0 and 'synthetic-profile-' not in failed.stdout+failed.stderr, 'mixed Claude credentials were enrolled or leaked')
after = run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', value['ARN'])
require(json.loads(before)['VersionId'] == json.loads(after)['VersionId'], 'refused enrollment changed the profile')
# Codex would read the access token first, so a profile cannot carry both Codex credentials.
for models in ({'openai_api_key':'synthetic-profile-openai', 'codex_access_token':'at-synthetic-profile-codex'},
               {'codex_access_token':'synthetic-profile-codex'}):
    path.write_text(json.dumps(dict(profile, models=models)))
    failed = subprocess.run(['buzz-agent-profile', 'enroll', '--arn', value['ARN'], '--file', str(path)],
                            env=env, capture_output=True, text=True, timeout=60)
    require(failed.returncode != 0 and 'synthetic-profile-' not in failed.stdout+failed.stderr, 'invalid Codex credentials were enrolled or leaked')
after = run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', value['ARN'])
require(json.loads(before)['VersionId'] == json.loads(after)['VersionId'], 'refused enrollment changed the profile')
print(json.dumps({'arn':value['ARN'], 'version':receipt['version_id']}))
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
def rotate(models):
    stored = json.loads(run('aws', 'secretsmanager', 'get-secret-value', '--secret-id', env['BUZZ_DEVELOPER_PROFILE_ARN']))
    profile = json.loads(stored['SecretString'])
    profile['models'] = models
    path = Path('/tmp/rotated-profile.json')
    path.write_text(json.dumps(profile))
    path.chmod(0o600)
    return json.loads(run('aws', 'secretsmanager', 'put-secret-value', '--secret-id', stored['ARN'], '--secret-string', 'file://'+str(path)))
if mode in ('manual-gate', 'gate-api-key'):
    # A subscription token may share the profile with the Codex gate; billed API keys may not.
    updated = rotate({'claude_oauth_token':'synthetic-profile-claude-oauth'}) if mode == 'manual-gate' else None
    env.update(BUZZ_SETUP_MODE='codex-device-v1', BUZZ_SETUP_AGENT_PUBKEY='c'*64,
        BUZZ_SETUP_POD_UID='11111111-2222-3333-4444-555555555555', BUZZ_SETUP_IMAGE_DIGEST='sha256:'+'d'*64,
        BUZZ_SETUP_NONCE='e'*32, BUZZ_SETUP_DEADLINE=str(int(time.time())+120),
        BUZZ_MANAGED_AGENT_START_NONCE='f'*32, BUZZ_ACP_AGENT_COMMAND='codex-acp',
        BUZZ_ACP_MCP_COMMAND='buzz-dev-mcp', BUZZ_ACP_PERMISSION_MODE='agent-full-access',
        CODEX_HOME='/home/agent/.codex', CODEX_CONFIG='{"cli_auth_credentials_store":"file"}', NO_BROWSER='1')
    process = subprocess.Popen(['sprig-entrypoint'], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        if mode == 'gate-api-key':
            require(process.wait(timeout=60) != 0, 'account gate accepted profile API keys')
            require(not Path('/home/agent/.agent-setup').exists(), 'refused profile reached the account gate')
            print('PASS: refused gate-api-key')
            raise SystemExit(0)
        for attempt in range(100):
            if Path('/home/agent/.agent-setup/pending.json').exists(): break
            require(process.poll() is None, 'profile did not reach the account gate')
            time.sleep(0.1)
        else: raise RuntimeError('profile did not reach the account gate')
        receipt = json.loads(Path('/home/agent/.config/buzz-agent/profile.json').read_text())
        require(receipt['version_id'] == updated['VersionId'], 'new session did not load the rotated profile')
        require(not Path('/home/agent/.codex/auth.json').exists(), 'account mode silently became API-key mode')
    finally:
        if process.poll() is None: process.terminate()
        output, error = process.communicate(timeout=10)
        require('synthetic-profile-' not in output+error, 'account gate leaked a credential')
    print('PASS: rotated shared profile with a Claude subscription token reaches manual account-login gate')
    raise SystemExit(0)
expected = {'OPENAI_API_KEY':'synthetic-profile-openai', 'ANTHROPIC_API_KEY':'synthetic-profile-anthropic',
            'CLAUDE_CODE_OAUTH_TOKEN':None, 'SCCACHE_WEBDAV_PASSWORD':'synthetic-profile-depot', 'BUZZ_GIT_IDENTITY':'user'}
if mode == 'claude-oauth':
    rotate({'openai_api_key':'synthetic-profile-openai', 'claude_oauth_token':'synthetic-profile-claude-oauth'})
    expected.update(ANTHROPIC_API_KEY=None, CLAUDE_CODE_OAUTH_TOKEN='synthetic-profile-claude-oauth')
if mode == 'subscriptions':
    # Both subscriptions in one profile, for Codex and Claude Code in parallel.
    rotate({'codex_access_token':'at-synthetic-profile-codex', 'claude_oauth_token':'synthetic-profile-claude-oauth'})
    expected.update(OPENAI_API_KEY=None, ANTHROPIC_API_KEY=None, CODEX_ACCESS_TOKEN='at-synthetic-profile-codex',
                    CLAUDE_CODE_OAUTH_TOKEN='synthetic-profile-claude-oauth')
if mode == 'no-depot':
    # An unreadable Depot credential turns the remote cache off; the session still starts.
    expected.update(SCCACHE_WEBDAV_PASSWORD=None)
if mode == 'inherited-codex': env['CODEX_ACCESS_TOKEN'] = 'at-synthetic-unselected-account'
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
        print('PASS: one developer enrollment; wrong GitHub actor cannot replace it', flush=True)
        modes = ['valid']*5 + ['wrong-owner', 'wrong-developer', 'mixed-model', 'inherited-claude',
                               'inherited-codex', 'symlink', 'missing', 'gate-api-key', 'claude-oauth',
                               'subscriptions', 'no-depot', 'manual-gate']
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
