#!/usr/bin/env python3
"""Configure one developer's container account without printing credentials."""
import argparse
import getpass
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

WORKSPACE_HOME = Path('/home/agent')
DIRECTORY = WORKSPACE_HOME / '.config/buzz-agent'


def run(*command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True, timeout=60, **kwargs)
    if result.returncode:
        raise ValueError('account command failed; account input and output suppressed')
    return result.stdout.strip()


def save(name, value):
    DIRECTORY.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(DIRECTORY, 0o700)
    target = DIRECTORY / name
    # Refuse links left by an earlier process; never follow a credential path.
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        os.fchmod(stream.fileno(), 0o600)
        stream.write(value)


def secret(prompt):
    if not sys.stdin.isatty() or not sys.stderr.isatty():
        raise ValueError('credential input requires the developer interactive terminal')
    value = getpass.getpass(prompt)
    if not value or len(value) > 8192 or any(c.isspace() for c in value):
        raise ValueError('invalid credential input')
    return value


def initialize():
    if os.environ.get('BUZZ_GIT_IDENTITY') != 'user':
        return
    fields = {'user.name': 'BUZZ_GIT_NAME', 'user.email': 'BUZZ_GIT_EMAIL',
              'user.signingkey': 'BUZZ_GIT_SIGNING_KEY'}
    if any(not os.environ.get(key) for key in fields.values()):
        raise ValueError('developer Git identity is incomplete')
    for option, key in fields.items():
        run('git', 'config', '--global', option, os.environ[key])
    # Profile Sandboxes sign with an agent-only SSH key file; a manual setup uses a forwarded GPG signer.
    signing = os.environ.get('BUZZ_GIT_SIGNING_FORMAT', 'openpgp')
    if signing not in ('openpgp', 'ssh'):
        raise ValueError('unsupported Git signing format')
    options = {'commit.gpgsign': 'true', 'tag.gpgsign': 'true', 'gpg.format': signing,
               'credential.https://github.com.helper': '!gh auth git-credential'}
    if signing == 'openpgp':
        options['gpg.program'] = '/usr/local/bin/buzz-gpg'
    for option, value in options.items():
        run('git', 'config', '--global', option, value)
    os.chmod(WORKSPACE_HOME / '.gitconfig', 0o600)
    (WORKSPACE_HOME / '.gnupg').mkdir(mode=0o700, exist_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['init', 'github', 'via', 'via-profile', 'depot', 'status'])
    parser.add_argument('--github-user')
    parser.add_argument('--organization')
    args = parser.parse_args()
    if args.action == 'init':
        initialize()
    elif args.action == 'github':
        if not args.github_user:
            raise ValueError('expected --github-user is required')
        token = secret('GitHub token for selected development repositories (hidden): ')
        env = {**os.environ, 'GH_TOKEN': token}
        actor = run('/usr/bin/gh', 'api', 'user', '--jq', '.login', env=env)
        if actor.casefold() != args.github_user.casefold():
            raise ValueError('GitHub token belongs to a different developer')
        save('github-token', token)
        print('GitHub account configured:', actor)
    elif args.action == 'via':
        token = secret('Development vault service-account token (hidden): ')
        # Validate before storing; no vault contents or returned values are printed.
        run('/usr/local/bin/op', 'whoami', '--format=json', env={
            **{k: v for k, v in os.environ.items() if not k.startswith('OP_')},
            'OP_SERVICE_ACCOUNT_TOKEN': token})
        save('op-token', token)
        print('Development vault account configured')
    elif args.action == 'via-profile':
        value = sys.stdin.read(65537)
        if len(value) > 65536:
            raise ValueError('via profile is too large')
        profile = tomllib.loads(value)
        if not profile.get('services') or not profile.get('providers'):
            raise ValueError('via profile requires providers and services')
        save('via.toml', value)
        run('/opt/agent-tools/via', '--config', str(DIRECTORY / 'via.toml'), 'capabilities', '--json')
        print('Development via profile installed')
    elif args.action == 'depot':
        if not args.organization or not re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', args.organization):
            raise ValueError('a Depot --organization ID is required')
        token = secret('Depot user or organization token (hidden): ')
        save('depot-token', token)
        save('depot.json', json.dumps({'organization': args.organization}))
        print('Depot credential stored; verify access with cold and warm builds')
    else:
        print(json.dumps({'git_identity': run('git', 'config', '--global', 'user.email'),
                          'signing_required': run('git', 'config', '--global', 'commit.gpgsign') == 'true',
                          'signer_connected': (WORKSPACE_HOME / '.gnupg/S.gpg-agent').is_socket()
                          or (DIRECTORY / 'ssh-signing-key').is_file(),
                          'github_configured': (DIRECTORY / 'github-token').is_file(),
                          'depot_configured': all((DIRECTORY / name).is_file() for name in ['depot-token', 'depot.json']),
                          'via_configured': all((DIRECTORY / name).is_file() for name in ['op-token', 'via.toml'])}))


if __name__ == '__main__':
    try:
        os.umask(0o077)
        main()
    except (ValueError, OSError, subprocess.TimeoutExpired) as error:
        print(str(error) if isinstance(error, ValueError) else 'Account setup failed', file=sys.stderr)
        sys.exit(1)
