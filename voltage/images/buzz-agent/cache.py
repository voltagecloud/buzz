#!/usr/bin/env python3
"""Run developer tools with the owner's Depot cache configuration."""
import json
import os
from pathlib import Path
import re
import sys

DIRECTORY = Path('/home/agent/.config/buzz-agent')


def environment():
    env = os.environ.copy()
    configuration = DIRECTORY / 'depot.json'
    token_file = DIRECTORY / 'depot-token'
    if not configuration.exists() and not token_file.exists():
        return env
    if not configuration.is_file() or not token_file.is_file():
        raise ValueError('Depot enrollment is incomplete')
    organization = json.loads(configuration.read_text())['organization']
    token = token_file.read_text().strip()
    if not re.fullmatch(r'[a-zA-Z0-9_-]{1,64}', organization) or not token or any(c.isspace() for c in token):
        raise ValueError('Invalid Depot enrollment')
    env.update(DEPOT_TOKEN=token,
               SCCACHE_WEBDAV_ENDPOINT='https://cache.depot.dev',
               SCCACHE_WEBDAV_USERNAME=organization, SCCACHE_WEBDAV_PASSWORD=token,
               TURBO_API='https://cache.depot.dev', TURBO_TEAM=organization, TURBO_TOKEN=token,
               GOCACHEPROG=f'depot gocache --organization {organization}')
    env.pop('SCCACHE_WEBDAV_TOKEN', None)
    env.pop('SCCACHE_GHA_ENABLED', None)
    env.setdefault('RUSTC_WRAPPER', '/usr/local/bin/sccache')
    env.setdefault('CARGO_INCREMENTAL', '0')
    return env


def main():
    if len(sys.argv) < 2:
        raise ValueError('usage: buzz-agent-cache COMMAND [ARGUMENT ...]')
    command = sys.argv[1:]
    os.execvpe(command[0], command, environment())


if __name__ == '__main__':
    try:
        main()
    except (ValueError, KeyError, OSError):
        print('Cache setup or command failed; credentials suppressed', file=sys.stderr)
        sys.exit(1)
