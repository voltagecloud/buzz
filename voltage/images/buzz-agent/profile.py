#!/usr/bin/env python3
"""Enroll one developer profile and load it into that developer's Sandboxes."""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import tomllib

MAX_BYTES = 65536
DIRECTORY = Path('/home/agent/.config/buzz-agent')
FIELDS = {'version', 'developer', 'owner_pubkey', 'git', 'github', 'via', 'models'}
MODEL_FIELDS = {'openai_api_key', 'codex_access_token', 'anthropic_api_key', 'claude_oauth_token',
                'openrouter_api_key'}
# The Codex device-login gate refuses these: billed API keys and a Codex access token.
API_KEY_FIELDS = {'openai_api_key', 'codex_access_token', 'anthropic_api_key', 'openrouter_api_key'}
# Inherited values would override or mix with the profile's selected accounts.
INHERITED_MODEL_VARIABLES = ('OPENAI_API_KEY', 'CODEX_API_KEY', 'CODEX_ACCESS_TOKEN', 'ANTHROPIC_API_KEY',
                             'ANTHROPIC_AUTH_TOKEN', 'CLAUDE_CODE_OAUTH_TOKEN', 'OPENROUTER_API_KEY')
# The shared Depot cache credential: the via profile's `depot` service, read from the vault.
DEPOT_SERVICE = 'depot'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fields(value, expected):
    require(isinstance(value, dict) and set(value) == expected, 'Profile fields are incomplete or unsupported')


def token(value):
    require(isinstance(value, str) and 0 < len(value) <= 8192
            and not any(c.isspace() for c in value), 'Invalid credential')


def validate(profile):
    fields(profile, FIELDS)
    require(type(profile['version']) is int and profile['version'] == 1, 'Unsupported profile version')
    require(isinstance(profile['developer'], str)
            and re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,30}[a-z0-9])?', profile['developer']), 'Invalid developer ID')
    require(isinstance(profile['owner_pubkey'], str)
            and re.fullmatch(r'[0-9a-f]{64}', profile['owner_pubkey']), 'Invalid Buzz owner')
    fields(profile['git'], {'name', 'email', 'signing_key'})
    for value in profile['git'].values():
        require(isinstance(value, str) and 0 < len(value) <= 256
                and not any(ord(c) < 32 for c in value), 'Invalid Git identity')
    require(re.fullmatch(r'[0-9A-Fa-f]{40,64}', profile['git']['signing_key']), 'Use a full OpenPGP signing fingerprint')
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


def private_file(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor) as stream:
        info = os.fstat(stream.fileno())
        require(stat.S_ISREG(info.st_mode) and not info.st_mode & 0o077
                and info.st_uid == os.getuid(), 'Profile input must be an owner-only regular file')
        value = stream.read(MAX_BYTES + 1)
    require(len(value.encode()) <= MAX_BYTES, 'Profile input is too large')
    return value


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


def enroll(args):
    profile = json.loads(private_file(args.file))
    validate(profile)
    secret_region(args.arn, profile['developer'])
    # Check the actual GitHub actor before any upload. The token is never an argument.
    env = {**os.environ, 'GH_TOKEN': profile['github']['token'], 'GH_HOST': 'github.com'}
    login = command('gh', 'api', '--hostname', 'github.com', 'user', '--jq', '.login', env=env).strip()
    require(login.casefold() == profile['github']['login'].casefold(), 'GitHub credential belongs to a different developer')
    env = {k: v for k, v in os.environ.items() if not k.startswith('OP_')}
    env['OP_SERVICE_ACCOUNT_TOKEN'] = profile['via']['op_token']
    command('op', 'whoami', '--format=json', env=env)
    # AWS reads the private file directly. Secret bytes never enter argv or stdout.
    # Rewrite validated data to avoid a caller modifying the source between validation and upload.
    with tempfile.TemporaryDirectory(prefix='buzz-profile-') as directory:
        path = Path(directory) / 'profile.json'
        path.write_text(json.dumps(profile))
        path.chmod(0o600)
        result = aws(args.arn, profile['developer'], 'put-secret-value', '--secret-string', 'file://' + str(path))
    print(json.dumps({'developer': profile['developer'], 'version_id': result['VersionId']}))


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
    result = aws(arn, developer, 'get-secret-value')
    raw = result['SecretString']
    require(len(raw.encode()) <= MAX_BYTES, 'Profile is too large')
    profile = json.loads(raw)
    validate(profile)
    require(profile['developer'] == developer
            and profile['owner_pubkey'] == os.environ.get('BUZZ_ACP_AGENT_OWNER'), 'Developer profile owner does not match the verified launch owner')
    require(result['ARN'] == arn, 'Secret response does not match the requested profile')
    setup_mode = bool(os.environ.get('BUZZ_SETUP_MODE'))
    require(not setup_mode or not API_KEY_FIELDS & set(profile['models']),
            'Interactive account login cannot be combined with model API keys or a Codex access token')
    env = os.environ.copy()
    for key in INHERITED_MODEL_VARIABLES:
        require(not env.get(key), 'Model credentials must come from the selected developer profile')
    env.update(BUZZ_GIT_IDENTITY='user', BUZZ_GIT_NAME=profile['git']['name'],
               BUZZ_GIT_EMAIL=profile['git']['email'], BUZZ_GIT_SIGNING_KEY=profile['git']['signing_key'],
               GIT_AUTHOR_NAME=profile['git']['name'], GIT_AUTHOR_EMAIL=profile['git']['email'],
               GIT_COMMITTER_NAME=profile['git']['name'], GIT_COMMITTER_EMAIL=profile['git']['email'])
    files = {'github-token': profile['github']['token'], 'op-token': profile['via']['op_token'],
             'via.toml': profile['via']['profile'],
             'profile.json': json.dumps({'developer': developer, 'version_id': result['VersionId']})}
    cache = depot(profile)
    if cache:
        files.update({'depot-token': cache[1], 'depot.json': json.dumps({'organization': cache[0]})})
    for name, value in files.items():
        write_file(DIRECTORY / name, value)
    if 'openai_api_key' in profile['models']:
        env['OPENAI_API_KEY'] = profile['models']['openai_api_key']
        env['CODEX_HOME'] = '/home/agent/.codex'
        env['CODEX_CONFIG'] = '{"cli_auth_credentials_store":"file"}'
        write_file(Path(env['CODEX_HOME']) / 'config.toml', 'cli_auth_credentials_store = "file"\n')
        command('codex', 'login', '--with-api-key', input=profile['models']['openai_api_key']+'\n', env=env)
    if 'codex_access_token' in profile['models']:
        # A workspace access token: no refresh token, so concurrent Sandboxes share it safely.
        env['CODEX_ACCESS_TOKEN'] = profile['models']['codex_access_token']
    if 'anthropic_api_key' in profile['models']:
        env['ANTHROPIC_API_KEY'] = profile['models']['anthropic_api_key']
    if 'openrouter_api_key' in profile['models']:
        # The Buzz agent runtime routes any OpenRouter model; an agent's own
        # BUZZ_AGENT_PROVIDER still wins.
        env['OPENROUTER_API_KEY'] = profile['models']['openrouter_api_key']
        env.setdefault('BUZZ_AGENT_PROVIDER', 'openrouter')
    # The Codex setup session cannot use the Claude token, and its gate refuses CLAUDE_* variables.
    if 'claude_oauth_token' in profile['models'] and not setup_mode:
        env['CLAUDE_CODE_OAUTH_TOKEN'] = profile['models']['claude_oauth_token']
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
        raise ValueError('Checkpoint restore failed')


SUBAGENT_CREDENTIALS = Path('/var/run/secrets/buzz-subagents')
SUBAGENT_GUIDE = Path('/etc/buzz-agent/subagents.md')


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
    parser = argparse.ArgumentParser(description=__doc__)
    actions = parser.add_subparsers(dest='action', required=True)
    enroll_parser = actions.add_parser('enroll', help='Validate and publish a private profile once per developer')
    enroll_parser.add_argument('--arn', required=True)
    enroll_parser.add_argument('--file', required=True, type=Path)
    run_parser = actions.add_parser('run', help='Load the verified owner profile, then start the harness')
    run_parser.add_argument('arguments', nargs=argparse.REMAINDER)
    actions.add_parser('work', help='Load the verified owner profile, then run one headless worker task')
    args = parser.parse_args()
    if args.action == 'enroll':
        enroll(args)
    elif args.action == 'work':
        env = bootstrap()
        worker = ['/usr/local/bin/buzz-agent-cache', '/usr/local/bin/buzz-agent-worker']
        os.execvpe(worker[0], worker, env)
    else:
        env = bootstrap()
        restore_checkpoint()
        install_subagent_guide(env)
        arguments = args.arguments[1:] if args.arguments[:1] == ['--'] else args.arguments
        if env.get('BUZZ_SETUP_MODE'):
            require(not arguments, 'Interactive setup does not accept harness arguments')
            harness = ['/usr/local/bin/buzz-agent-setup', 'gate']
        else:
            harness = ['/usr/local/bin/buzz-agent-cache', '/usr/local/bin/buzz-acp', *arguments]
        os.execvpe(harness[0], harness, env)


if __name__ == '__main__':
    try:
        os.umask(0o077)
        main()
    except (ValueError, KeyError, TypeError, OSError, subprocess.TimeoutExpired):
        print('Developer profile refused; credentials and command output suppressed', file=sys.stderr)
        sys.exit(1)
