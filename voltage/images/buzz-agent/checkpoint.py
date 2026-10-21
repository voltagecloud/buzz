#!/usr/bin/env python3
"""Save and explicitly restore private development workspaces."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import uuid
from urllib.parse import urlsplit, urlunsplit

MAX_BYTES = 256 * 1024 * 1024
MAX_FILES = 50000
EXCLUDED = {'.git', '.aws', '.ssh', '.gnupg', '.config', '.codex', '.claude',
            '.cache', '.local', '.npm', '.cargo', '.rustup', '.venv', 'venv',
            'node_modules', 'target', '__pycache__', 'dist', 'build', '.terraform'}
PRIVATE_NAMES = {'credentials', 'credentials.json', 'auth.json', 'id_rsa', 'id_ed25519',
                 '.npmrc', '.netrc', '.pypirc', '.git-credentials'}


def run(*args, cwd=None, timeout=60, env=None):
    result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, timeout=timeout)
    if result.returncode:
        raise ValueError('Checkpoint command failed; command output suppressed')
    return result.stdout


def git(repo, *args):
    return run('git', '-c', 'core.hooksPath=/dev/null', '-C', str(repo), *args)


def safe_path(value):
    path = PurePosixPath(value)
    if path.is_absolute() or '..' in path.parts or not path.parts:
        raise ValueError('Checkpoint contains an unsafe path')
    return path


def excluded(path):
    return any(part in EXCLUDED or part in PRIVATE_NAMES or part.startswith('.env')
               or part.endswith(('.pem', '.key', '.p12', '.pfx', '.tfstate', '.tfstate.backup'))
               for part in path.parts)


def repositories(workspace):
    found = []
    for root, directories, files in os.walk(workspace, followlinks=False):
        if '.git' in directories or '.git' in files:
            found.append(Path(root))
        directories[:] = [d for d in directories if d not in EXCLUDED
                          and not Path(root, d).is_symlink()]
    return sorted(found)


def public_origin(repo):
    value = git(repo, 'config', '--get', 'remote.origin.url').decode().strip()
    if value.startswith('git@github.com:'):
        value = 'https://github.com/' + value.split(':', 1)[1]
    parsed = urlsplit(value)
    if parsed.scheme not in {'https', 'ssh'} or not parsed.hostname:
        raise ValueError('Checkpoint requires an HTTPS or SSH repository origin')
    host = f'[{parsed.hostname}]' if ':' in parsed.hostname else parsed.hostname
    if parsed.port is not None:
        host += f':{parsed.port}'
    if parsed.scheme == 'ssh' and parsed.username:
        host = f'{parsed.username}@{host}'
    return urlunsplit((parsed.scheme, host, parsed.path, '', ''))


def scan(directory, *, history=False):
    # Immutable image configuration prevents workspace allowlists from bypassing
    # the scan. Scanner output can contain secrets even on errors, so suppress it.
    env = {k: v for k, v in os.environ.items() if not k.startswith('GITLEAKS_')}
    command = ['gitleaks', 'git' if history else 'dir', str(directory),
               '--config', '/opt/agent-tools/checkpoint-gitleaks.toml',
               '--gitleaks-ignore-path', '/dev/null', '--ignore-gitleaks-allow',
               '--no-banner', '--redact', '--timeout', '60']
    if history:
        command += ['--log-opts=--all HEAD']
    run(*command, timeout=65, env=env)


def pack(workspace, output):
    workspace = workspace.resolve(strict=True)
    manifest = {'version': 1, 'repositories': []}
    total = 0
    count = 0
    with tempfile.TemporaryDirectory(prefix='agent-checkpoint-') as temp:
        stage = Path(temp)
        for index, repo in enumerate(repositories(workspace)):
            relative = repo.relative_to(workspace).as_posix()
            if relative == '.':
                raise ValueError('Use a workspace directory containing repositories')
            record = {'path': relative, 'origin': public_origin(repo),
                      'head': git(repo, 'rev-parse', 'HEAD').decode().strip(),
                      'branch': git(repo, 'branch', '--show-current').decode().strip(),
                      'excluded': [], 'files': []}
            destination = stage / 'repositories' / str(index)
            (destination / 'files').mkdir(parents=True)
            scan(repo, history=True)
            git(repo, 'bundle', 'create', str(destination / 'history.bundle'), '--all', 'HEAD')
            total += (destination / 'history.bundle').stat().st_size
            names = git(repo, 'ls-files', '-z', '--cached', '--others', '--exclude-standard')
            for raw in sorted(set(names.split(b'\0')) - {b''}):
                path = safe_path(os.fsdecode(raw))
                if excluded(path):
                    record['excluded'].append(str(path))
                    continue
                source = repo / path
                if not source.exists() and not source.is_symlink():
                    continue  # A tracked deletion is represented by absence.
                if source.is_dir():
                    continue  # Submodule repositories are discovered separately.
                info = source.lstat()
                target = destination / 'files' / path
                target.parent.mkdir(parents=True, exist_ok=True)
                if stat.S_ISLNK(info.st_mode):
                    link = os.readlink(source)
                    resolved = (source.parent / link).resolve()
                    if not resolved.is_relative_to(repo) or Path(link).is_absolute():
                        raise ValueError('Checkpoint refused an external repository symlink')
                    target.symlink_to(link)
                elif stat.S_ISREG(info.st_mode):
                    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
                    with os.fdopen(fd, 'rb') as stream:
                        content = stream.read(MAX_BYTES + 1)
                    total += len(content)
                    if total > MAX_BYTES:
                        raise ValueError('Checkpoint exceeds the 256 MiB source budget')
                    target.write_bytes(content)
                    target.chmod(0o755 if info.st_mode & 0o111 else 0o644)
                else:
                    raise ValueError('Checkpoint refused a special file')
                count += 1
                if count > MAX_FILES:
                    raise ValueError('Checkpoint exceeds the file-count budget')
                record['files'].append(str(path))
            manifest['repositories'].append(record)
        if total > MAX_BYTES:
            raise ValueError('Checkpoint history exceeds the 256 MiB source budget')
        (stage / 'manifest.json').write_text(json.dumps(manifest))
        scan(stage)
        with tarfile.open(output, 'w:gz', dereference=False) as archive:
            archive.add(stage, arcname='snapshot')
    os.chmod(output, 0o600)
    return manifest


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').digest()


def restore(archive_path, destination):
    if destination.exists():
        raise ValueError('Restore requires a new destination; existing work is never overwritten')
    with tempfile.TemporaryDirectory(prefix='agent-restore-') as temp:
        with tarfile.open(archive_path, 'r:gz') as archive:
            members = archive.getmembers()
            if len(members) > MAX_FILES * 3 or sum(m.size for m in members) > MAX_BYTES * 2:
                raise ValueError('Checkpoint exceeds restore budget')
            for member in members:
                safe_path(member.name)
                if member.islnk() or not (member.isfile() or member.isdir() or member.issym()):
                    raise ValueError('Checkpoint contains an unsupported archive entry')
            archive.extractall(temp, filter='data')
        stage = Path(temp) / 'snapshot'
        manifest = json.loads((stage / 'manifest.json').read_text())
        if manifest.get('version') != 1:
            raise ValueError('Unsupported checkpoint version')
        scan(stage)
        destination.mkdir(mode=0o700, parents=True)
        for index, record in enumerate(manifest['repositories']):
            relative = safe_path(record['path'])
            if excluded(relative):
                raise ValueError('Checkpoint repository path is reserved')
            source = stage / 'repositories' / str(index)
            repo = destination / relative
            run('git', '-c', 'core.hooksPath=/dev/null', 'clone', '--no-checkout',
                str(source / 'history.bundle'), str(repo))
            if not re.fullmatch('[0-9a-f]{40,64}', record['head']):
                raise ValueError('Invalid checkpoint revision')
            git(repo, 'checkout', '--detach', record['head'])
            if record['branch']:
                git(repo, 'check-ref-format', '--branch', record['branch'])
                git(repo, 'checkout', '-B', record['branch'])
            git(repo, 'remote', 'set-url', 'origin', record['origin'])
            # Absence from the snapshot means deletion, except for excluded paths:
            # they were never saved, so they keep their committed content.
            for raw in git(repo, 'ls-files', '-z').split(b'\0'):
                if raw:
                    path = safe_path(os.fsdecode(raw))
                    target = repo / path
                    if not excluded(path) and (target.is_file() or target.is_symlink()):
                        target.unlink()
            shutil.copytree(source / 'files', repo, dirs_exist_ok=True, symlinks=True)
        return manifest


def storage():
    bucket = os.environ.get('BUZZ_CHECKPOINT_BUCKET', '')
    prefix = os.environ.get('BUZZ_CHECKPOINT_PREFIX', '')
    session = os.environ.get('BUZZ_MANAGED_AGENT_START_NONCE', '')
    if not re.fullmatch('[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]', bucket):
        raise ValueError('Checkpoint bucket is missing or invalid')
    if not re.fullmatch('[a-z0-9][a-z0-9-]{0,31}/', prefix):
        raise ValueError('Checkpoint developer prefix is missing or invalid')
    if not re.fullmatch('[a-z0-9-]{8,64}', session):
        raise ValueError('Checkpoint session binding is missing or invalid')
    return bucket, prefix + session + '/'


def save(workspace):
    bucket, prefix = storage()
    with tempfile.TemporaryDirectory(prefix='agent-upload-') as temp:
        artifact = Path(temp) / 'checkpoint.tar.gz'
        manifest = pack(workspace, artifact)
        checksum = base64.b64encode(digest(artifact)).decode()
        key = prefix + str(uuid.uuid4()) + '.tar.gz'
        run('aws', 's3api', 'put-object', '--bucket', bucket, '--key', key,
            '--body', str(artifact), '--if-none-match', '*', '--server-side-encryption', 'AES256',
            '--checksum-algorithm', 'SHA256', '--checksum-sha256', checksum)
        head = json.loads(run('aws', 's3api', 'head-object', '--bucket', bucket,
                              '--key', key, '--checksum-mode', 'ENABLED'))
        if head.get('ChecksumSHA256') != checksum or head.get('ContentLength') != artifact.stat().st_size:
            raise ValueError('Checkpoint upload could not be verified')
        receipt = {'version': 1, 'key': key, 'sha256': digest(artifact).hex(),
                   'repositories': len(manifest['repositories']), 'bytes': artifact.stat().st_size}
        private = workspace / '.config/buzz-agent'
        private.mkdir(parents=True, exist_ok=True, mode=0o700)
        receipt_path = private / 'checkpoint.json'
        fd = os.open(receipt_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, 'w') as stream:
            json.dump(receipt, stream)
        return receipt


def recover(key, destination):
    bucket, _ = storage()
    if not key.startswith(os.environ['BUZZ_CHECKPOINT_PREFIX']) or not re.fullmatch(
            '[a-z0-9-]+/[a-z0-9-]{8,64}/[a-f0-9-]{36}\\.tar\\.gz', key):
        raise ValueError('Checkpoint does not belong to the configured developer prefix')
    head = json.loads(run('aws', 's3api', 'head-object', '--bucket', bucket,
                          '--key', key, '--checksum-mode', 'ENABLED'))
    if not 0 < head.get('ContentLength', 0) <= MAX_BYTES * 2 or not head.get('ChecksumSHA256'):
        raise ValueError('Checkpoint size or checksum is missing')
    with tempfile.TemporaryDirectory(prefix='agent-download-') as temp:
        artifact = Path(temp) / 'checkpoint.tar.gz'
        # No overall deadline: a large archive may download slowly but steadily.
        # The CLI's socket timeouts fail the download only when it stalls.
        run('aws', 's3api', 'get-object', '--cli-connect-timeout', '10', '--cli-read-timeout', '60',
            '--bucket', bucket, '--key', key, str(artifact), timeout=None)
        if artifact.stat().st_size != head['ContentLength'] or base64.b64encode(digest(artifact)).decode() != head['ChecksumSHA256']:
            raise ValueError('Downloaded checkpoint checksum differs')
        return restore(artifact, destination)


def restore_once(key, workspace):
    """Recover the launch checkpoint into the live workspace once.

    The marker lives in the excluded private directory, so it survives a
    container restart but never enters a later checkpoint.
    """
    if not key:
        return {'version': 1, 'restored': None}
    workspace = workspace.resolve(strict=True)
    private = workspace / '.config/buzz-agent'
    marker = private / 'checkpoint-restored.json'
    if marker.exists() or marker.is_symlink():
        fd = os.open(marker, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(fd) as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError('Restore marker is not a regular file')
            recorded = json.loads(stream.read(4096))
        if recorded.get('key') != key:
            raise ValueError('Workspace was already restored from a different checkpoint')
        return {'version': 1, 'restored': key, 'skipped': True}
    # restore() requires a new destination; stage beside the workspace on the
    # same filesystem and move whole top-level entries without overwriting.
    staging = workspace / ('.checkpoint-restore-' + uuid.uuid4().hex)
    try:
        manifest = recover(key, staging)
        entries = sorted(staging.iterdir())
        if any((workspace / entry.name).exists() or (workspace / entry.name).is_symlink()
               for entry in entries):
            raise ValueError('Restore would overwrite existing workspace content')
        for entry in entries:
            entry.rename(workspace / entry.name)
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    private.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(marker, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as stream:
        json.dump({'version': 1, 'key': key}, stream)
    # The manifest lists file names; report only counts.
    return {'version': 1, 'restored': key, 'repositories': len(manifest['repositories'])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['save', 'pack', 'restore', 'recover', 'restore-once'])
    parser.add_argument('--workspace', type=Path, default=Path('/home/agent'))
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--destination', type=Path)
    parser.add_argument('--key')
    args = parser.parse_args()
    if args.action == 'save':
        result = save(args.workspace)
    elif args.action == 'pack' and args.archive:
        result = pack(args.workspace, args.archive)
    elif args.action == 'restore' and args.archive and args.destination:
        result = restore(args.archive, args.destination)
    elif args.action == 'recover' and args.key and args.destination:
        result = recover(args.key, args.destination)
    elif args.action == 'restore-once' and args.key is not None:
        result = restore_once(args.key, args.workspace)
    else:
        raise ValueError('pack requires --archive; restore also requires --destination')
    print(json.dumps(result))


if __name__ == '__main__':
    os.umask(0o077)
    try:
        main()
    except (ValueError, OSError, subprocess.TimeoutExpired, tarfile.TarError):
        print('Checkpoint failed; retain the workspace and inspect it before cleanup', file=sys.stderr)
        sys.exit(1)
