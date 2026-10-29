#!/usr/bin/env python3
"""Load the developer profile the Sandbox manager enrolled into that developer's Sandboxes."""
import argparse
import base64
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import stat
import ssl
import subprocess
import sys
import tomllib
from urllib import request as http
from urllib.parse import urlsplit

MAX_BYTES = 65536
DIRECTORY = Path('/home/agent/.config/buzz-agent')
FIELDS = {'version', 'developer', 'owner_pubkey', 'git', 'github', 'via', 'models'}
MODEL_FIELDS = {'openai_api_key', 'codex_access_token', 'anthropic_api_key', 'claude_oauth_token',
                'openrouter_api_key'}
# Inherited values would override or mix with the profile's selected accounts.
INHERITED_MODEL_VARIABLES = ('OPENAI_API_KEY', 'CODEX_API_KEY', 'CODEX_ACCESS_TOKEN', 'ANTHROPIC_API_KEY',
                             'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN', 'OPENROUTER_API_KEY',
                             'CODEX_REFRESH_TOKEN_URL_OVERRIDE', 'CODEX_CA_CERTIFICATE')
# The shared Depot cache credential: the via profile's `depot` service, read from the vault.
DEPOT_SERVICE = 'depot'
# The developer's agent-only commit signing key, registered on their GitHub account at enrollment.
SSH_SIGNING_KEY = re.compile(r'-----BEGIN OPENSSH PRIVATE KEY-----\n[A-Za-z0-9+/=\n]{1,4000}-----END OPENSSH PRIVATE KEY-----\n?')


class Refused(ValueError):
    """A refusal whose message is a fixed string, never a value, so startup may print it."""


def require(condition, message):
    if not condition:
        raise Refused(message)


# The startup step in progress, named in the refusal so a failed Sandbox says where it stopped.
STEP = ['starting']


def step(name):
    STEP[0] = name


def fields(value, expected):
    require(isinstance(value, dict) and set(value) == expected, 'Profile fields are incomplete or unsupported')


def token(value):
    require(isinstance(value, str) and 0 < len(value) <= 8192
            and not any(c.isspace() for c in value), 'Invalid credential')


def validate(profile):
    require(isinstance(profile, dict) and FIELDS <= set(profile)
            and set(profile) <= FIELDS | {'codex_login'}, 'Profile fields are incomplete or unsupported')
    require(type(profile['version']) is int and profile['version'] == 1, 'Unsupported profile version')
    require(isinstance(profile['developer'], str)
            and re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?', profile['developer']), 'Invalid developer ID')
    require(isinstance(profile['owner_pubkey'], str)
            and re.fullmatch(r'[0-9a-f]{64}', profile['owner_pubkey']), 'Invalid Buzz owner')
    fields(profile['git'], {'name', 'email', 'ssh_signing_key'})
    for value in (profile['git']['name'], profile['git']['email']):
        require(isinstance(value, str) and 0 < len(value) <= 256
                and not any(ord(c) < 32 for c in value), 'Invalid Git identity')
    require(isinstance(profile['git']['ssh_signing_key'], str)
            and SSH_SIGNING_KEY.fullmatch(profile['git']['ssh_signing_key']), 'Invalid SSH signing key')
    fields(profile['github'], {'login', 'token'})
    require(isinstance(profile['github']['login'], str)
            and re.fullmatch(r'[a-zA-Z0-9][a-zA-Z0-9-]{0,38}', profile['github']['login']), 'Invalid GitHub login')
    token(profile['github']['token'])
    fields(profile['via'], {'profile', 'op_token'})
    token(profile['via']['op_token'])
    require(isinstance(profile['via']['profile'], str), 'Invalid via profile')
    config = tomllib.loads(profile['via']['profile'])
    require(config.get('services') and config.get('providers'), 'via profile requires providers and services')
    require(isinstance(profile['models'], dict)
            and set(profile['models']) <= MODEL_FIELDS, 'Unsupported model credential type')
    if 'codex_login' in profile:
        validate_codex_login(profile['codex_login'], profile['developer'])
        require(not {'openai_api_key', 'codex_access_token'} & set(profile['models']),
                'Choose a ChatGPT login, an OpenAI API key, or a Codex access token')
    # Codex reads the access token before its saved API-key login, so allow only one.
    require(not {'openai_api_key', 'codex_access_token'} <= set(profile['models']),
            'Choose either an OpenAI API key or a Codex access token')
    # Claude Code prefers an API key over the subscription token, so allow only one.
    require(not {'anthropic_api_key', 'claude_oauth_token'} <= set(profile['models']),
            'Choose either a Claude API key or a Claude subscription token')
    for value in profile['models'].values():
        token(value)
    require('codex_access_token' not in profile['models']
            or profile['models']['codex_access_token'].startswith('at-'), 'Use a Codex access token (at-...)')
    require(len(json.dumps(profile).encode()) <= MAX_BYTES, 'Profile exceeds Secrets Manager size limit')


def validate_codex_login(login, developer):
    fields(login, {'url', 'ca_certificate', 'refresh_token'})
    require(isinstance(login['url'], str), 'Invalid ChatGPT broker URL')
    url = urlsplit(login['url'])
    require(url.scheme == 'https'
            and url.hostname == 'codex-refresh.buzz-sandbox-manager.svc.cluster.local'
            and url.port == 8443 and url.path == '/oauth/token'
            and url.username is None and url.password is None
            and not url.query and not url.fragment, 'Invalid ChatGPT broker URL')
    certificate = login['ca_certificate']
    require(isinstance(certificate, str) and len(certificate) <= 8192
            and certificate.startswith('-----BEGIN CERTIFICATE-----\n')
            and certificate.rstrip().endswith('-----END CERTIFICATE-----'), 'Invalid ChatGPT broker certificate')
    token(login['refresh_token'])
    require(re.fullmatch(re.escape(developer) + r'\.[a-f0-9]{32}\.seed', login['refresh_token']),
            'Invalid ChatGPT broker credential')


class NoRedirect(http.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def configure_codex_login(login, env):
    """Validate the broker grant before every new Sandbox or worker starts."""
    step('loading the shared ChatGPT login')
    try:
        context = ssl.create_default_context(cadata=login['ca_certificate'])
        # Ignore ambient HTTP proxies for this private, authenticated endpoint.
        opener = http.build_opener(http.ProxyHandler({}), http.HTTPSHandler(context=context), NoRedirect())
        body = json.dumps({'client_id': 'app_EMoamEEZ73f0CkXaXp7hrann',
                           'grant_type': 'refresh_token',
                           'refresh_token': login['refresh_token']}).encode()
        req = http.Request(login['url'], data=body, headers={'Content-Type': 'application/json'})
        with opener.open(req, timeout=60) as response:
            raw = response.read(MAX_BYTES + 1)
        require(len(raw) <= MAX_BYTES, 'Invalid ChatGPT broker response')
        refreshed = json.loads(raw)
        for key in ('access_token', 'id_token', 'refresh_token', 'account_id'):
            token(refreshed[key])
        require(refreshed['refresh_token'].rsplit('.', 1)[0]
                == login['refresh_token'].rsplit('.', 1)[0], 'Invalid ChatGPT broker response')
        claims = json.loads(base64.urlsafe_b64decode(refreshed['id_token'].split('.')[1] + '==='))
        require(claims['https://api.openai.com/auth']['chatgpt_account_id'] == refreshed['account_id'],
                'Invalid ChatGPT broker response')
    except (KeyError, ValueError, TypeError, OSError, IndexError, ssl.SSLError):
        raise Refused('Shared ChatGPT login unavailable; enroll again if it was disconnected or interrupted') from None
    directory = Path('/home/agent/.codex')
    write_file(directory / 'config.toml', 'cli_auth_credentials_store = "file"\n')
    write_file(directory / 'auth.json', json.dumps({
        'auth_mode': 'chatgpt', 'OPENAI_API_KEY': None,
        'tokens': {key: refreshed[key] for key in ('id_token', 'access_token', 'refresh_token', 'account_id')},
        'last_refresh': datetime.now(timezone.utc).isoformat().replace('+00:00', 'Z'),
    }))
    # Keep public roots as well as the private broker CA: Codex uses this
    # bundle for its upstream HTTPS connections too.
    system_ca = ssl.get_default_verify_paths().cafile
    roots = Path(system_ca).read_text() if system_ca else ''
    certificate_file = DIRECTORY / 'codex-ca.pem'
    write_file(certificate_file, roots + '\n' + login['ca_certificate'])
    env.update(CODEX_HOME=str(directory), CODEX_CONFIG='{"cli_auth_credentials_store":"file"}',
               CODEX_REFRESH_TOKEN_URL_OVERRIDE=login['url'], CODEX_CA_CERTIFICATE=str(certificate_file))


def secret_region(arn, developer):
    match = re.fullmatch(r'arn:aws:secretsmanager:([a-z0-9-]+):[0-9]{12}:secret:voltage/staging/agents/'
                         + re.escape(developer) + r'/profile-[a-zA-Z0-9]{6}', arn)
    require(match, 'Profile ARN does not belong to this developer')
    return match[1]


def command(*args, env=None, input=None):
    result = subprocess.run(args, input=input, env=env, capture_output=True, text=True, timeout=60)
    require(result.returncode == 0, 'Profile operation failed; command output suppressed')
    return result.stdout


def aws(arn, developer, operation, *args):
    return json.loads(command('aws', '--region', secret_region(arn, developer),
                              '--no-cli-pager', 'secretsmanager', operation,
                              '--secret-id', arn, '--output', 'json', *args))


def write_file(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    # Every parent must be an owner-controlled directory, not a symlink into a repository.
    parent = path.parent
    while parent != Path('/home/agent'):
        info = parent.lstat()
        require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
                and not info.st_mode & 0o077, 'Unsafe credential directory')
        parent = parent.parent
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write(value)


def depot(profile):
    """The Depot organization and token the via profile's `depot` service names, or None.

    The vault holds one Depot credential for every developer's agents. A missing,
    malformed, or unreadable service only turns the remote build cache off."""
    services = tomllib.loads(profile['via']['profile'])['services']
    if not isinstance(services, dict) or DEPOT_SERVICE not in services:
        print('Depot cache unavailable: the via profile has no depot service; building without it', file=sys.stderr)
        return None
    try:
        secrets = services[DEPOT_SERVICE]['secrets']
        references = [secrets['organization'], secrets['token']]
        require(all(isinstance(value, str) and value.startswith('op://') for value in references),
                'Depot references must be op:// references')
        env = {k: v for k, v in os.environ.items() if not k.startswith('OP_')}
        env['OP_SERVICE_ACCOUNT_TOKEN'] = profile['via']['op_token']
        organization, value = (command('op', 'read', '--no-newline', reference, env=env).strip()
                               for reference in references)
        require(re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', organization), 'Invalid Depot organization')
        token(value)
    # command() reports a failed read as ValueError; OSError covers an op that cannot run.
    except (ValueError, KeyError, TypeError, AttributeError, OSError, subprocess.TimeoutExpired):
        print('Depot cache unavailable: the via profile depot service could not be read; building without it',
              file=sys.stderr)
        return None
    return organization, value


def bootstrap():
    developer = os.environ.get('BUZZ_DEVELOPER_ID', '')
    arn = os.environ.get('BUZZ_DEVELOPER_PROFILE_ARN', '')
    step('reading the profile secret')
    result = aws(arn, developer, 'get-secret-value')
    raw = result['SecretString']
    require(len(raw.encode()) <= MAX_BYTES, 'Profile is too large')
    step('validating the profile')
    profile = json.loads(raw)
    validate(profile)
    require(profile['developer'] == developer
            and profile['owner_pubkey'] == os.environ.get('BUZZ_ACP_AGENT_OWNER'), 'Developer profile owner does not match the verified launch owner')
    require(result['ARN'] == arn, 'Secret response does not match the requested profile')
    env = os.environ.copy()
    for key in INHERITED_MODEL_VARIABLES:
        require(not env.get(key), 'Model credentials must come from the selected developer profile')
    # Commits are signed with the agent-only SSH key; the key file stays out of checkpoints.
    signing_key = DIRECTORY / 'ssh-signing-key'
    env.update(BUZZ_GIT_IDENTITY='user', BUZZ_GIT_NAME=profile['git']['name'],
               BUZZ_GIT_EMAIL=profile['git']['email'], BUZZ_GIT_SIGNING_KEY=str(signing_key),
               GIT_AUTHOR_NAME=profile['git']['name'], GIT_AUTHOR_EMAIL=profile['git']['email'],
               GIT_COMMITTER_NAME=profile['git']['name'], GIT_COMMITTER_EMAIL=profile['git']['email'])
    files = {'github-token': profile['github']['token'], 'op-token': profile['via']['op_token'],
             'via.toml': profile['via']['profile'],
             'ssh-signing-key': profile['git']['ssh_signing_key'].rstrip('\n') + '\n',
             'profile.json': json.dumps({'developer': developer, 'version_id': result['VersionId']})}
    step('reading the Depot cache credential')
    cache = depot(profile)
    if cache:
        files.update({'depot-token': cache[1], 'depot.json': json.dumps({'organization': cache[0]})})
    step('writing credential files')
    for name, value in files.items():
        write_file(DIRECTORY / name, value)
    if 'openai_api_key' in profile['models']:
        step('signing in to Codex')
        env['OPENAI_API_KEY'] = profile['models']['openai_api_key']
        env['CODEX_HOME'] = '/home/agent/.codex'
        env['CODEX_CONFIG'] = '{"cli_auth_credentials_store":"file"}'
        write_file(Path(env['CODEX_HOME']) / 'config.toml', 'cli_auth_credentials_store = "file"\n')
        command('codex', 'login', '--with-api-key', input=profile['models']['openai_api_key']+'\n', env=env)
    if 'codex_access_token' in profile['models']:
        # A workspace access token: no refresh token, so concurrent Sandboxes share it safely.
        env['CODEX_ACCESS_TOKEN'] = profile['models']['codex_access_token']
    if 'codex_login' in profile:
        configure_codex_login(profile['codex_login'], env)
    if 'anthropic_api_key' in profile['models']:
        env['ANTHROPIC_API_KEY'] = profile['models']['anthropic_api_key']
    if 'openrouter_api_key' in profile['models']:
        # The Buzz agent runtime routes any OpenRouter model; an agent's own
        # BUZZ_AGENT_PROVIDER still wins.
        env['OPENROUTER_API_KEY'] = profile['models']['openrouter_api_key']
        env.setdefault('BUZZ_AGENT_PROVIDER', 'openrouter')
    if 'claude_oauth_token' in profile['models']:
        env['CLAUDE_CODE_OAUTH_TOKEN'] = profile['models']['claude_oauth_token']
    step('configuring Git commit signing')
    command('/usr/local/bin/buzz-agent-account', 'init', env=env)
    return env


def restore_checkpoint():
    # Uses the launch environment (IRSA), not the profile's model credentials.
    key = os.environ.get('BUZZ_CHECKPOINT_RESTORE_KEY', '')
    if not key:
        return
    result = subprocess.run(['/usr/local/bin/buzz-agent-checkpoint', 'restore-once', '--key', key,
                             '--workspace', '/home/agent'],
                            stdin=subprocess.DEVNULL, stdout=sys.stderr)
    if result.returncode:
        print('Checkpoint restore failed; refusing to start with an empty workspace', file=sys.stderr)
        raise Refused('Checkpoint restore failed')


SUBAGENT_CREDENTIALS = Path('/var/run/secrets/buzz-subagents')
SUBAGENT_GUIDE = Path('/etc/buzz-agent/subagents.md')
CLUSTER_CREDENTIALS = Path('/var/run/secrets/buzz-cluster')
CLUSTER_GUIDE = Path('/etc/buzz-agent/cluster.md')


def install_cluster_access(env):
    """Configure kubectl with the mounted, rotating token; never copy its value."""
    credentials = next((path for path in (CLUSTER_CREDENTIALS, SUBAGENT_CREDENTIALS)
                        if (path / 'token').is_file()), None)
    if credentials is None:
        return
    require(all((credentials / name).is_file() for name in ('ca.crt', 'namespace')),
            'Staging cluster credentials are incomplete')
    namespace = (credentials / 'namespace').read_text().strip()
    require(namespace == 'buzz-agent-staging-' + env['BUZZ_DEVELOPER_ID'],
            'Staging cluster credentials do not match the developer namespace')
    config = {
        'apiVersion': 'v1', 'kind': 'Config', 'current-context': 'voltage-staging',
        'clusters': [{'name': 'voltage-staging', 'cluster': {
            'server': 'https://kubernetes.default.svc',
            'certificate-authority': str(credentials / 'ca.crt')}}],
        'users': [{'name': 'agent-observer', 'user': {'tokenFile': str(credentials / 'token')}}],
        'contexts': [{'name': 'voltage-staging', 'context': {
            'cluster': 'voltage-staging', 'user': 'agent-observer', 'namespace': namespace}}],
    }
    path = DIRECTORY / 'staging-kubeconfig.json'
    write_file(path, json.dumps(config))
    env['KUBECONFIG'] = str(path)
    guide = CLUSTER_GUIDE.read_text()
    for target in (Path('/home/agent/.claude/CLAUDE.md'), Path(env.get('CODEX_HOME', '/home/agent/.codex')) / 'AGENTS.md'):
        target.parent.mkdir(parents=True, exist_ok=True)
        with open(target, 'a') as handle:
            handle.write(guide)


def install_subagent_guide(env):
    """Tell Claude Code and Codex how to delegate when this session may spawn workers."""
    if not (SUBAGENT_CREDENTIALS / 'token').exists():
        return
    guide = SUBAGENT_GUIDE.read_text()
    for target in (Path('/home/agent/.claude/CLAUDE.md'), Path(env.get('CODEX_HOME', '/home/agent/.codex')) / 'AGENTS.md'):
        target.parent.mkdir(parents=True, exist_ok=True)
        with open(target, 'a') as handle:
            handle.write(guide)


def main():
    """Load the owner profile and cluster access before starting a session or worker."""
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest='action', required=True)
    run_parser = actions.add_parser('run', help='Load the verified owner profile, then start the harness')
    run_parser.add_argument('arguments', nargs=argparse.REMAINDER)
    actions.add_parser('work', help='Load the verified owner profile, then run one headless worker task')
    args = parser.parse_args()
    if args.action == 'work':
        env = bootstrap()
        step('configuring staging cluster diagnostics')
        install_cluster_access(env)
        step('starting the worker')
        worker = ['/usr/local/bin/buzz-agent-cache', '/usr/local/bin/buzz-agent-worker']
        os.execvpe(worker[0], worker, env)
    else:
        env = bootstrap()
        step('restoring the checkpoint')
        restore_checkpoint()
        step('configuring staging cluster diagnostics')
        install_cluster_access(env)
        step('installing the sub-agent guide')
        install_subagent_guide(env)
        step('starting the harness')
        arguments = args.arguments[1:] if args.arguments[:1] == ['--'] else args.arguments
        harness = ['/usr/local/bin/buzz-agent-cache', '/usr/local/bin/buzz-acp', *arguments]
        os.execvpe(harness[0], harness, env)


if __name__ == '__main__':
    try:
        os.umask(0o077)
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired) as error:
        # Only a fixed refusal message or the error's type: other errors can quote a value.
        reason = str(error) if type(error) is Refused else type(error).__name__
        print('Developer profile refused while '+STEP[0]+': '+reason
              +'; credentials and command output suppressed', file=sys.stderr)
        sys.exit(1)
