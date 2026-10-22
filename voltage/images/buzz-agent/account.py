#!/usr/bin/env python3
"""Configure the developer's Git identity in a Sandbox without printing credentials."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

WORKSPACE_HOME = Path('/home/agent')
DIRECTORY = WORKSPACE_HOME / '.config/buzz-agent'


def run(*command, **kwargs):
    result = subprocess.run(command, capture_output=True, text=True, timeout=60, **kwargs)
    if result.returncode:
        raise ValueError('account command failed; account input and output suppressed')
    return result.stdout.strip()


def initialize():
    if os.environ.get('BUZZ_GIT_IDENTITY') != 'user':
        return
    fields = {'user.name': 'BUZZ_GIT_NAME', 'user.email': 'BUZZ_GIT_EMAIL',
              'user.signingkey': 'BUZZ_GIT_SIGNING_KEY'}
    if any(not os.environ.get(key) for key in fields.values()):
        raise ValueError('developer Git identity is incomplete')
    for option, key in fields.items():
        run('git', 'config', '--global', option, os.environ[key])
    # Every commit and tag is signed with the developer's agent-only SSH key file.
    for option, value in {'commit.gpgsign': 'true', 'tag.gpgsign': 'true', 'gpg.format': 'ssh',
                          'credential.https://github.com.helper': '!gh auth git-credential'}.items():
        run('git', 'config', '--global', option, value)
    os.chmod(WORKSPACE_HOME / '.gitconfig', 0o600)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['init', 'status'])
    args = parser.parse_args()
    if args.action == 'init':
        initialize()
    else:
        print(json.dumps({'git_identity': run('git', 'config', '--global', 'user.email'),
                          'signing_required': run('git', 'config', '--global', 'commit.gpgsign') == 'true',
                          'signing_key': (DIRECTORY / 'ssh-signing-key').is_file(),
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
