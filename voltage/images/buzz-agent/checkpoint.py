#!/usr/bin/env python3
"""Save and explicitly restore private development workspaces."""
import argparse
import base64
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid
from urllib.parse import urlsplit, urlunsplit

MAX_BYTES = 256 * 1024 * 1024
MAX_FILES = 50000
# All origins are asked at once; this bounds the whole save's network wait.
ORIGIN_SECONDS = 20
EXCLUDED = {'.git', '.aws', '.ssh', '.gnupg', '.config', '.codex', '.claude',
            '.cache', '.local', '.npm', '.cargo', '.rustup', '.venv', 'venv',
            'node_modules', 'target', '__pycache__', 'dist', 'build', '.terraform'}
PRIVATE_NAMES = {'credentials', 'credentials.json', 'auth.json', 'id_rsa', 'id_ed25519',
                 '.npmrc', '.netrc', '.pypirc', '.git-credentials'}
# buzz-acp discards this helper's output, so the exit code names the failed
# step (helper_step in crates/buzz-acp/src/checkpoint.rs).
STEPS = {'reading the checkpoint settings': 10, 'scanning for secrets': 11,
         'packing the workspace': 12, 'uploading the checkpoint': 13,
         'restoring the checkpoint': 14}
STEP = ['packing the workspace']


def step(name):
    STEP[0] = name


def run(*args, cwd=None, timeout=60, env=None, input=None):
    result = subprocess.run(args, cwd=cwd, env=env, input=input, capture_output=True, timeout=timeout)
    if result.returncode:
        raise ValueError('Checkpoint command failed; command output suppressed')
    return result.stdout


def git(repo, *args, **options):
    return run('git', '-c', 'core.hooksPath=/dev/null', '-C', str(repo), *args, **options)


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


def scan(directory, *, history=False, negatives=b''):
    """Refuse a credential the agent added: commits not on a remote, or files.

    History and files already on a remote are on the forge, where the agent's
    token can read them, so the scan leaves them out. `negatives` are the
    `^<commit>` origin tips of an origin-based save: its bundle holds every
    commit beyond them, so the scan covers exactly those."""
    # Immutable image configuration prevents workspace allowlists from bypassing
    # the scan. Scanner output can contain secrets even on errors, so suppress it.
    previous = STEP[0]
    step('scanning for secrets')
    env = {k: v for k, v in os.environ.items() if not k.startswith('GITLEAKS_')}
    command = ['gitleaks', 'git' if history else 'dir', str(directory),
               '--config', '/opt/agent-tools/checkpoint-gitleaks.toml',
               '--gitleaks-ignore-path', '/dev/null', '--ignore-gitleaks-allow',
               '--no-banner', '--redact', '--timeout', '60']
    if history:
        command += ['--log-opts=--all HEAD ' + (' '.join(negatives.decode().split())
                                                if negatives else '--not --remotes')]
    run(*command, timeout=65, env=env)
    step(previous)


def review_changes(repo, review):
    """Write what the working tree adds to HEAD into review, for the secret scan.

    Untracked files are copied whole. A tracked file contributes only its added
    lines, so a credential already upstream in an edited file is not a finding."""
    def write(path, content):
        target = review / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)

    for raw in git(repo, 'ls-files', '-z', '--others', '--exclude-standard').split(b'\0'):
        path = safe_path(os.fsdecode(raw)) if raw else None
        if path and not excluded(path) and (repo / path).is_file() and not (repo / path).is_symlink():
            write(path, (repo / path).read_bytes())
    for raw in git(repo, 'diff', '--name-only', '--no-renames', '-z', 'HEAD').split(b'\0'):
        path = safe_path(os.fsdecode(raw)) if raw else None
        if not path or excluded(path):
            continue
        patch = git(repo, 'diff', '--no-ext-diff', '--no-color', '--no-renames', '-U0',
                    'HEAD', '--', str(path))
        hunks = patch.split(b'\n@@', 1)[1] if b'\n@@' in patch else b''
        added = [line[1:] for line in hunks.split(b'\n') if line.startswith(b'+')]
        if added:
            write(path, b'\n'.join(added) + b'\n')


def origin_listings(origins):
    """`git ls-remote --heads --tags` of each origin, or None where unusable.

    Asks the normalized URL restore clones, with the global Git configuration
    restore uses, so a save never relies on a credential restore lacks. Only
    HTTPS origins qualify: the Sandbox's GitHub credential serves HTTPS. All
    origins are asked at once under one ORIGIN_SECONDS deadline; an origin
    that cannot answer in time is saved as a bundle instead."""
    deadline = time.monotonic() + ORIGIN_SECONDS
    env = dict(os.environ, GIT_TERMINAL_PROMPT='0')

    def ask(origin):
        remaining = deadline - time.monotonic()
        if urlsplit(origin).scheme != 'https' or remaining <= 0:
            return None
        # Its own process group: git's HTTPS helper would otherwise outlive a
        # timed-out git and hold the output pipe open.
        process = subprocess.Popen(['git', '-c', 'core.hooksPath=/dev/null', 'ls-remote', '--heads',
                                    '--tags', origin], cwd='/', env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
        try:
            listing, _ = process.communicate(timeout=remaining)
        except subprocess.TimeoutExpired:
            listing = None
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate()
        return listing if listing is not None and process.returncode == 0 else None

    with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
        return list(pool.map(ask, origins))


def origin_base(repo, listing):
    """What a clone of origin would give this repository.

    Returns `^<commit>` rev-list lines for origin's branch tips that this clone
    has, and origin's (ref, object) pairs. A clone restores everything
    reachable from those tips, so the checkpoint needs only what lies beyond
    them. Asks origin, not remote-tracking refs: those can be stale, and after
    a bundle restore they name commits origin never had. No tips means restore
    cannot build on origin."""
    if listing is None:
        return b'', set()
    refs = set()
    for line in listing.decode().splitlines():
        if '\t' in line:
            name, ref = line.split('\t', 1)
            refs.add((ref, name))
    heads = sorted({value for ref, value in refs if ref.startswith('refs/heads/')})
    found = git(repo, 'cat-file', '--batch-check=%(objectname) %(objecttype)',
                input=''.join(head + '\n' for head in heads).encode())
    tips = b''.join(b'^' + line.split(b' ', 1)[0] + b'\n'
                    for line in found.splitlines() if line.endswith(b' commit'))
    return tips, refs


def local_refs(repo, origin_refs):
    """Branches, and tags origin lacks, for restore to recreate exactly.

    Annotated tag objects travel by content: a bundle omits a ref whose target
    origin already has, and a tag object is not on origin."""
    refs, tags = [], {}
    listing = git(repo, 'for-each-ref', '--format=%(objectname) %(objecttype) %(refname)',
                  'refs/heads/', 'refs/tags/')
    for line in listing.decode().splitlines():
        value, kind, ref = line.split(' ', 2)
        if ref.startswith('refs/tags/') and (ref, value) in origin_refs:
            continue
        refs.append([ref, value])
        if kind == 'tag':
            tags[value] = base64.b64encode(git(repo, 'cat-file', 'tag', value)).decode()
    return refs, tags


def pack(workspace, output):
    workspace = workspace.resolve(strict=True)
    manifest = {'version': 1, 'repositories': []}
    total = 0
    count = 0
    with tempfile.TemporaryDirectory(prefix='agent-checkpoint-') as temp, \
            tempfile.TemporaryDirectory(prefix='agent-checkpoint-review-') as review_temp:
        stage = Path(temp)
        # What the agent added to each repository, for the secret scan.
        review = Path(review_temp)
        repos = repositories(workspace)
        if any(repo == workspace for repo in repos):
            raise ValueError('Use a workspace directory containing repositories')
        origins = [public_origin(repo) for repo in repos]
        listings = origin_listings(origins)
        for index, (repo, origin, listing) in enumerate(zip(repos, origins, listings)):
            relative = repo.relative_to(workspace).as_posix()
            tips, origin_refs = origin_base(repo, listing)
            base = 'origin' if tips else 'bundle'
            record = {'path': relative, 'origin': origin, 'base': base,
                      'head': git(repo, 'rev-parse', 'HEAD').decode().strip(),
                      'branch': git(repo, 'branch', '--show-current').decode().strip(),
                      'excluded': [], 'files': [], 'deleted': []}
            if base == 'origin':
                # Older images refuse version 2, so it marks only what needs it.
                manifest['version'] = 2
                record['refs'], record['tags'] = local_refs(repo, origin_refs)
                total += sum(len(content) for content in record['tags'].values())
            destination = stage / 'repositories' / str(index)
            (destination / 'files').mkdir(parents=True)
            scan(repo, history=True, negatives=tips)
            review_changes(repo, review / str(index))
            bundle = destination / 'history.bundle'
            if base == 'bundle':
                # The whole history, and every file: restore needs nothing else.
                git(repo, 'bundle', 'create', str(bundle), '--all', 'HEAD')
                names = git(repo, 'ls-files', '-z', '--cached', '--others', '--exclude-standard')
            else:
                # Only commits origin lacks, and only files that differ from HEAD.
                # git refuses an empty bundle, so a fully pushed repository has none.
                if git(repo, 'rev-list', '--max-count=1', '--all', 'HEAD', '--stdin', input=tips).strip():
                    git(repo, 'bundle', 'create', str(bundle), '--all', 'HEAD', '--stdin', input=tips)
                names = (git(repo, 'ls-files', '-z', '--others', '--exclude-standard') + b'\0'
                         + git(repo, 'diff', '--name-only', '--no-renames', '-z', 'HEAD'))
            if bundle.exists():
                total += bundle.stat().st_size
            for raw in sorted(set(names.split(b'\0')) - {b''}):
                path = safe_path(os.fsdecode(raw))
                if excluded(path):
                    record['excluded'].append(str(path))
                    continue
                source = repo / path
                if not source.exists() and not source.is_symlink():
                    if base == 'origin':
                        record['deleted'].append(str(path))
                    continue  # A bundle snapshot represents a deletion by absence.
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
        shutil.copyfile(stage / 'manifest.json', review / 'manifest.json')
        scan(review)
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
        if manifest.get('version') not in (1, 2):
            raise ValueError('Unsupported checkpoint version')
        destination.mkdir(mode=0o700, parents=True)
        try:
            restore_repositories(stage, manifest, destination)
        except BaseException:
            # The destination is new, so a refused restore leaves nothing behind.
            shutil.rmtree(destination, ignore_errors=True)
            raise
        return manifest


def restore_repositories(stage, manifest, destination):
    with tempfile.TemporaryDirectory(prefix='agent-restore-review-') as review_temp:
        # The same scope as pack(): what each repository adds to its HEAD.
        review = Path(review_temp)
        (review / 'manifest.json').write_text(json.dumps(manifest))
        for index, record in enumerate(manifest['repositories']):
            relative = safe_path(record['path'])
            if excluded(relative):
                raise ValueError('Checkpoint repository path is reserved')
            source = stage / 'repositories' / str(index)
            repo = destination / relative
            if not re.fullmatch('[0-9a-f]{40,64}', record['head']):
                raise ValueError('Invalid checkpoint revision')
            # Version 1 checkpoints are all bundle snapshots.
            base = record.get('base', 'bundle')
            if base == 'bundle':
                run('git', '-c', 'core.hooksPath=/dev/null', 'clone', '--no-checkout',
                    str(source / 'history.bundle'), str(repo))
            elif base == 'origin':
                clone_origin(record['origin'], repo)
                if (source / 'history.bundle').exists():
                    fetch_bundle(repo, source / 'history.bundle')
                restore_refs(repo, record)
            else:
                raise ValueError('Unsupported checkpoint repository base')
            # Checking out a large tree can take minutes.
            git(repo, 'checkout', '--detach', record['head'], timeout=600)
            if record['branch']:
                git(repo, 'check-ref-format', '--branch', record['branch'])
                git(repo, 'checkout', '-B', record['branch'], timeout=600)
            git(repo, 'remote', 'set-url', 'origin', record['origin'])
            if base == 'bundle':
                # Absence from the snapshot means deletion, except for excluded
                # paths: they were never saved, so they keep their committed content.
                deleted = [os.fsdecode(raw) for raw in git(repo, 'ls-files', '-z').split(b'\0') if raw]
            else:
                deleted = record.get('deleted', [])
            for value in deleted:
                path = safe_path(value)
                if not excluded(path):
                    remove(repo, path)
            for value in record['files']:
                path = safe_path(value)
                if excluded(path):
                    raise ValueError('Checkpoint file path is reserved')
                place(repo, source / 'files', path)
            review_changes(repo, review / str(index))
        scan(review)


def clone_origin(origin, repo):
    """Clone a checkpoint's pushed base with the Sandbox's Git credentials.

    A large repository can clone slowly but steadily, so there is no overall
    deadline; Git fails the clone only when the transfer stalls."""
    if urlsplit(origin).scheme != 'https':
        raise ValueError('Checkpoint origin base requires an HTTPS origin')
    run('git', '-c', 'core.hooksPath=/dev/null', '-c', 'http.lowSpeedLimit=1000',
        '-c', 'http.lowSpeedTime=60', 'clone', '--no-checkout', '--', origin, str(repo),
        timeout=None, env=dict(os.environ, GIT_TERMINAL_PROMPT='0'))


def remove(repo, path):
    """Delete the file or symlink at path, never through a symlink or file.

    Git tracks nothing beyond a symlink, so a parent that is not a real
    directory means the path is already absent."""
    target = repo
    for part in path.parts[:-1]:
        target = target / part
        if target.is_symlink() or not target.is_dir():
            return
    target = target / path.parts[-1]
    if target.is_symlink() or target.is_file():
        target.unlink()


def place(repo, files, path):
    """Put the saved file or symlink at path, replacing whatever is there.

    The checkout can hold another type at the path or a parent: a symlink the
    agent retargeted or replaced, a file it turned into a directory, or the
    reverse. Replacing, not writing through, keeps every write inside repo."""
    target = repo
    for part in path.parts[:-1]:
        target = target / part
        if target.is_symlink() or (target.exists() and not target.is_dir()):
            target.unlink()
        target.mkdir(exist_ok=True)
    target = target / path.parts[-1]
    if target.is_symlink() or target.is_file():
        target.unlink()
    elif target.is_dir():
        shutil.rmtree(target)
    saved = files / path
    if saved.is_symlink():
        target.symlink_to(os.readlink(saved))
    elif saved.is_file():
        shutil.copy2(saved, target, follow_symlinks=False)
    else:
        raise ValueError('Checkpoint file is missing from the archive')


def restore_refs(repo, record):
    """Recreate the saved branches and tags, annotated tag objects first."""
    for name, content in record.get('tags', {}).items():
        made = git(repo, 'hash-object', '-t', 'tag', '-w', '--stdin',
                   input=base64.b64decode(content, validate=True)).decode().strip()
        if made != name:
            raise ValueError('Checkpoint tag object does not match its name')
    for ref, value in record.get('refs', []):
        if not ref.startswith(('refs/heads/', 'refs/tags/')) or not re.fullmatch('[0-9a-f]{40,64}', value):
            raise ValueError('Invalid checkpoint ref')
        git(repo, 'check-ref-format', ref)
        git(repo, 'update-ref', ref, value)


def fetch_bundle(repo, bundle):
    """Add a checkpoint's unpushed commits, and their refs, to an origin clone."""
    specs = []
    for line in git(repo, 'bundle', 'list-heads', str(bundle)).decode().splitlines():
        name = line.split(' ', 1)[1] if ' ' in line else ''
        if name == 'HEAD':
            specs.append('HEAD')  # A detached HEAD commit is not on any branch.
        else:
            git(repo, 'check-ref-format', name)
            specs.append('+' + name + ':' + name)
    # The bundle's prerequisites are origin commits; a missing one fails here.
    git(repo, 'fetch', '--update-head-ok', '--no-tags', str(bundle), *specs)


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
    step('reading the checkpoint settings')
    bucket, prefix = storage()
    with tempfile.TemporaryDirectory(prefix='agent-upload-') as temp:
        artifact = Path(temp) / 'checkpoint.tar.gz'
        step('packing the workspace')
        manifest = pack(workspace, artifact)
        step('uploading the checkpoint')
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
    if args.action in ('restore', 'recover', 'restore-once'):
        step('restoring the checkpoint')
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
        print('Checkpoint failed while '+STEP[0]+'; retain the workspace and inspect it before cleanup',
              file=sys.stderr)
        sys.exit(STEPS[STEP[0]])
