#!/usr/bin/python3 -I
"""Pack fully scanned, self-contained workspace snapshots. No network or upload."""
import argparse
from contextlib import ExitStack, contextmanager
import errno
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import selectors
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
import time

MAX_BLOB = 100 * 1024 * 1024 - 26  # BUZZSNAP1 NUL + AEAD tag are added later.
MAX_OBJECTS = 200000
MAX_REPOS = 64
MAX_META = 32 * 1024 * 1024
MAX_PAX = 1024 * 1024
DEADLINE = None
PHASE = 'archive'
SAFE_ENV = {'PATH': '/usr/local/bin:/usr/bin:/bin', 'LC_ALL': 'C',
            'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_SYSTEM': '/dev/null',
            'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_NO_REPLACE_OBJECTS': '1',
            'GIT_GRAFT_FILE': '/dev/null', 'GIT_NO_LAZY_FETCH': '1',
            'GIT_ATTR_NOSYSTEM': '1', 'GIT_TEMPLATE_DIR': ''}


class Budget(ValueError):
    """Refusal without exposing a file name or subprocess output."""


def remaining(seconds=60):
    value = min(seconds, DEADLINE - time.monotonic())
    if value <= 0:
        raise Budget('Snapshot deadline exceeded')
    return value


def run(*args, cwd=None, timeout=60, env=None, input=None, output=None,
        consume=None, limit=MAX_META):
    # Source Git config cannot execute hooks, start fsmonitor, fetch objects,
    # or select an unbounded pack compression worker pool.
    if args and args[0] == 'git':
        args = (args[0], '-c', 'protocol.allow=never', '-c', 'core.hooksPath=/dev/null',
                '-c', 'core.fsmonitor=false', '-c', 'pack.threads=1',
                '-c', 'pack.windowMemory=16m', '-c', 'core.logAllRefUpdates=false', *args[1:])
    expires = time.monotonic() + remaining(timeout)
    with ExitStack() as stack:
        sink = stack.enter_context(open(output, 'xb')) if output is not None else None
        process = subprocess.Popen(args, cwd=cwd, env=SAFE_ENV,
                                   stdin=subprocess.PIPE if input is not None else subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
        data = bytearray()
        written = total = 0
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                if input is not None:
                    os.set_blocking(process.stdin.fileno(), False)
                    selector.register(process.stdin, selectors.EVENT_WRITE)
                while selector.get_map():
                    left = expires - time.monotonic()
                    if left <= 0:
                        raise Budget('Snapshot command timed out')
                    for key, _ in selector.select(left):
                        if key.fileobj is process.stdin:
                            count = os.write(process.stdin.fileno(), input[written:written + 65536])
                            written += count
                            if written == len(input):
                                selector.unregister(process.stdin)
                                process.stdin.close()
                        else:
                            chunk = os.read(process.stdout.fileno(), 65536)
                            if not chunk:
                                selector.unregister(process.stdout)
                                continue
                            total += len(chunk)
                            if total > limit:
                                raise Budget('Snapshot output exceeds budget')
                            if consume is not None:
                                consume(chunk)
                            elif sink is not None:
                                sink.write(chunk)
                            else:
                                data.extend(chunk)
                if process.wait(timeout=max(.001, expires - time.monotonic())):
                    raise ValueError('Snapshot command refused; private output suppressed')
            return bytes(data)
        finally:
            if process.returncode is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            process.stdout.close()
            if process.stdin is not None and not process.stdin.closed:
                process.stdin.close()


def checkpoint():
    installed = Path(__file__).name == 'buzz-agent-snapshot'
    path = Path(__file__).resolve().with_name('buzz-agent-checkpoint' if installed else 'checkpoint.py')
    loader = importlib.machinery.SourceFileLoader('snapshot_checkpoint', str(path))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    module.run = run
    module.history_check = lambda repo: (0, run('git', '-C', str(repo), 'fsck',
        '--full', '--no-reflogs', '--no-dangling', limit=1024 * 1024))
    return module


def path(value):
    candidate = PurePosixPath(value)
    if (not value or candidate.is_absolute() or str(candidate) != value
            or len(value.encode('utf-8')) > 4096 or len(candidate.parts) > 128
            or any(part in {'.', '..'} for part in candidate.parts)
            or any(c in value for c in '\\\0:') or any(ord(c) < 32 or ord(c) == 127 for c in value)):
        raise ValueError('Snapshot contains an unsupported path')
    return candidate


def inventory(repo, cp):
    lines = cp.git(repo, 'cat-file', '--batch-all-objects',
                   '--batch-check=%(objectname) %(objecttype) %(objectsize)').decode().splitlines()
    total = 0
    if len(lines) > MAX_OBJECTS:
        raise Budget('Snapshot Git object count exceeds budget')
    for line in lines:
        oid, kind, size = line.split(' ')
        if (len(oid) not in (40, 64) or any(c not in '0123456789abcdef' for c in oid)
                or kind not in {'blob', 'tree', 'commit', 'tag'}):
            raise ValueError('Snapshot Git inventory is invalid')
        total += int(size)
        if total > cp.MAX_BYTES:
            raise Budget('Snapshot Git expansion exceeds budget')
    return lines


def inspect_captured(bundle, cp):
    # No fallback: source preflight cannot authorize costs of a different bundle
    # captured after the source commits. The native SDK validates the exact pack.
    result = json.loads(run('/usr/local/bin/buzz', 'sandbox', 'inspect-bundle',
                            str(bundle), limit=16 * 1024 * 1024))
    if (not isinstance(result, dict)
            or set(result) != {'version', 'objects', 'expanded_bytes', 'hash_bytes', 'refs'}
            or type(result['version']) is not int or result['version'] != 1
            or type(result['objects']) is not int or not 0 < result['objects'] <= MAX_OBJECTS
            or type(result['expanded_bytes']) is not int or not 0 <= result['expanded_bytes'] <= cp.MAX_BYTES
            or type(result['hash_bytes']) is not int or result['hash_bytes'] not in (20, 32)
            or not isinstance(result['refs'], list) or not 0 < len(result['refs']) <= cp.MAX_FILES):
        raise ValueError('Snapshot captured-bundle metadata is invalid')
    parents = set()
    for ref in result['refs']:
        if (not isinstance(ref, list) or len(ref) != 2 or not all(isinstance(v, str) for v in ref)
                or len(ref[0]) != result['hash_bytes'] * 2
                or any(c not in '0123456789abcdef' for c in ref[0])
                or not (ref[1] == 'HEAD' or ref[1].startswith('refs/'))):
            raise ValueError('Snapshot captured reference is invalid')
        parents.update(path(ref[1]).parents)
    return result, len(parents)


class ObjectStream:
    """Split one bounded cat-file batch without materializing object bodies."""
    def __init__(self, lines, directory):
        self.lines = lines
        self.directory = directory
        self.index = 0
        self.header = bytearray()
        self.sink = None
        self.left = None
        self.separator = False

    def feed(self, chunk):
        offset = 0
        while offset < len(chunk):
            if self.separator:
                if chunk[offset] != 10:
                    raise ValueError('Snapshot object framing is invalid')
                offset += 1
                self.separator = False
                self.left = None
                self.index += 1
            elif self.left is None:
                end = chunk.find(b'\n', offset)
                stop = len(chunk) if end < 0 else end
                self.header.extend(chunk[offset:stop])
                if len(self.header) > 160:
                    raise Budget('Snapshot object header exceeds budget')
                offset = stop
                if end < 0:
                    continue
                if self.index >= len(self.lines) or self.header.decode() != self.lines[self.index]:
                    raise ValueError('Snapshot object inventory changed')
                oid, _, size = self.lines[self.index].split(' ')
                self.sink = (self.directory / oid).open('xb')
                self.left = int(size)
                self.header.clear()
                offset += 1
                if self.left == 0:
                    self.sink.close()
                    self.sink = None
                    self.separator = True
            else:
                count = min(self.left, len(chunk) - offset)
                self.sink.write(chunk[offset:offset + count])
                self.left -= count
                offset += count
                if self.left == 0:
                    self.sink.close()
                    self.sink = None
                    self.separator = True

    def finish(self):
        if self.index != len(self.lines) or self.left is not None or self.header:
            raise ValueError('Snapshot object stream is incomplete')

    def close(self):
        if self.sink is not None:
            self.sink.close()


class Work:
    """Both a configured scratch limit and actual filesystem capacity are required.

    statvfs cannot discover kubelet emptyDir sizeLimit. Operators must configure
    this budget below that limit after reserving space for the running agent.
    """
    def __init__(self, directory, limit):
        if not 1024 * 1024 <= limit <= 2 * 1024 * 1024 * 1024:
            raise Budget('Snapshot work budget is invalid')
        info = os.statvfs(directory)
        self.block = max(512, info.f_frsize)
        self.limit = min(limit, info.f_bavail * info.f_frsize)
        self.used = 0
        self.directories = set()
        self.reserve(16 * 1024 * 1024)  # Metadata/index overhead and operational reserve.

    def reserve(self, size):
        self.used += size
        if self.used > self.limit:
            raise Budget('Snapshot scratch capacity exceeds budget')

    def file(self, path):
        missing = [parent for parent in path.parents if parent not in self.directories]
        self.reserve((len(missing) + 1) * self.block)
        self.directories.update(missing)


class ArchiveSink:
    """Admit compressed output before writing, rather than checking after close."""
    def __init__(self, sink, work):
        self.sink = sink
        self.work = work
        self.size = 0

    def write(self, data):
        if self.size + len(data) > MAX_BLOB:
            raise Budget('Snapshot archive exceeds blob budget')
        self.work.reserve(len(data))
        count = self.sink.write(data)
        self.size += count
        return count

    def tell(self):
        return self.size

    def flush(self):
        self.sink.flush()


@contextmanager
def opened_repo(workspace, relative):
    descriptor = os.open(workspace, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in relative.parts:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        # Git follows an already-open directory, not a mutable workspace path.
        yield Path(f'/proc/{os.getpid()}/fd/{descriptor}'), descriptor
    finally:
        os.close(descriptor)


def copy_file(repo, descriptor, relative, destination, cp, limit):
    # Walk each component with O_NOFOLLOW; source directory replacement cannot
    # turn a scan/copy race into a read of an unrelated runtime credential.
    parent = os.dup(descriptor)
    try:
        for component in relative.parts[:-1]:
            child = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
            os.close(parent)
            parent = child
        info = os.stat(relative.name, dir_fd=parent, follow_symlinks=False)
        destination.parent.mkdir(parents=True, exist_ok=True)
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(relative.name, dir_fd=parent)
            if (len(target.encode('utf-8')) > 4096 or Path(target).is_absolute()
                    or any(c in target for c in '\\\0:')
                    or any(ord(c) < 32 or ord(c) == 127 for c in target)):
                raise ValueError('Snapshot symlink is unsafe')
            resolved = (repo / relative.parent / target).resolve()
            if not resolved.is_relative_to(repo.resolve()):
                raise ValueError('Snapshot symlink leaves its repository')
            size = len(target.encode('utf-8'))
            if size > limit:
                raise Budget('Snapshot source exceeds budget')
            destination.symlink_to(target)
            return size
        if not stat.S_ISREG(info.st_mode):
            raise ValueError('Snapshot refuses special files')
        source = os.open(relative.name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW, dir_fd=parent)
        with os.fdopen(source, 'rb') as stream, destination.open('xb') as sink:
            actual = os.fstat(stream.fileno())
            if not stat.S_ISREG(actual.st_mode) or actual.st_size > limit:
                raise Budget('Snapshot source exceeds budget')
            total = 0
            while chunk := stream.read(65536):
                remaining()
                total += len(chunk)
                if total > limit:
                    raise Budget('Snapshot source exceeds budget')
                sink.write(chunk)
        destination.chmod(0o755 if actual.st_mode & 0o111 else 0o644)
        return total
    finally:
        os.close(parent)


def discover(workspace, cp):
    repos = []
    pending = [workspace]
    entries = 0
    while pending:
        remaining()
        root = pending.pop()
        with os.scandir(root) as children:
            for child in children:
                entries += 1
                if entries > cp.MAX_FILES * 2:
                    raise Budget('Snapshot discovery exceeds budget')
                if child.name == '.git':
                    repos.append(root)
                    if len(repos) > MAX_REPOS:
                        raise Budget('Snapshot repository count exceeds budget')
                elif (child.is_dir(follow_symlinks=False)
                      and not cp.excluded(PurePosixPath(child.name))):
                    pending.append(Path(child.path))
    return repos


def pack(workspace, output, work_bytes=512 * 1024 * 1024):
    global DEADLINE, PHASE
    DEADLINE = time.monotonic() + 180
    PHASE = 'archive'
    os.environ.clear()
    os.environ.update(SAFE_ENV)
    cp = checkpoint()
    workspace = Path(workspace).resolve(strict=True)
    output = Path(output)
    if output.exists() or output.is_symlink():
        raise ValueError('Snapshot output already exists')
    repos = discover(workspace, cp)
    if not repos or len(repos) > MAX_REPOS:
        raise Budget('Snapshot requires between one and 64 repositories')
    total = expanded = objects_count = refs_count = 0
    count = 0
    manifest = {'version': 1, 'snapshot': True, 'scanned': True, 'repositories': []}
    with tempfile.TemporaryDirectory(prefix='buzz-snapshot-') as temp:
        work = Work(temp, work_bytes)
        stage = Path(temp) / 'snapshot'
        review = Path(temp) / 'review'
        stage.mkdir()
        review.mkdir()
        for index, repo in enumerate(sorted(repos)):
            relative = repo.relative_to(workspace).as_posix() if repo != workspace else 'workspace'
            path(relative)
            original_relative = repo.relative_to(workspace)
            with opened_repo(workspace, original_relative) as (repo, descriptor):
                # Inventory before packing is conservative: even unreachable source
                # objects count, so no large source object is decoded before a bound.
                source_objects = inventory(repo, cp)
                source_expanded = sum(int(line.rsplit(' ', 1)[1]) for line in source_objects)
                expanded += source_expanded
                objects_count += len(source_objects)
                if expanded > cp.MAX_BYTES or objects_count > MAX_OBJECTS:
                    raise Budget('Snapshot aggregate Git expansion exceeds budget')
                head = cp.git(repo, 'rev-parse', '--verify', 'HEAD').decode().strip()
                record = {'path': relative, 'head': head, 'base': 'bundle',
                          'branch': cp.git(repo, 'branch', '--show-current').decode().strip(),
                          'shallow': cp.shallow_commits(repo), 'files': [], 'excluded': [], 'deleted': []}
                target = stage / 'repositories' / str(index)
                (target / 'files').mkdir(parents=True)
                bundle = target / 'history.bundle'
                source_cost = source_expanded + len(source_objects) * (work.block + 64)
                work.reserve(source_cost)
                allowance = min(MAX_BLOB, cp.MAX_BYTES - total, (work.limit - work.used) // 2)
                if allowance <= 0:
                    raise Budget('Snapshot bundle capacity exceeds budget')
                cp.git(repo, 'bundle', 'create', '-', '--all', 'HEAD', output=bundle, limit=allowance)
                bundle_size = bundle.stat().st_size
                total += bundle_size
                work.reserve(2 * bundle_size)
                if total > cp.MAX_BYTES or bundle_size > MAX_BLOB:
                    raise Budget('Snapshot source exceeds budget')
                packed, ref_directories = inspect_captured(bundle, cp)
                expanded += max(0, packed['expanded_bytes'] - source_expanded)
                objects_count += max(0, packed['objects'] - len(source_objects))
                refs_count += len(packed['refs'])
                if expanded > cp.MAX_BYTES or objects_count > MAX_OBJECTS or refs_count > cp.MAX_FILES:
                    raise Budget('Snapshot captured aggregate inventory exceeds budget')
                actual_cost = (packed['expanded_bytes'] + packed['objects'] * (work.block + 64)
                               + (len(packed['refs']) + ref_directories) * work.block)
                work.reserve(max(0, actual_cost - source_cost))
                neutral = Path(temp) / f'history-{index}'
                cp.restore_history(bundle, record, neutral)
                objects = inventory(neutral, cp)
                actual_expanded = sum(int(line.rsplit(' ', 1)[1]) for line in objects)
                if actual_expanded != packed['expanded_bytes'] or len(objects) != packed['objects']:
                    raise ValueError('Snapshot neutral inventory differs from the captured bundle')
                object_review = review / str(index)
                object_review.mkdir()
                writer = ObjectStream(objects, object_review)
                try:
                    names = ''.join(line.split(' ', 1)[0] + '\n' for line in objects).encode()
                    limit = sum(int(line.rsplit(' ', 1)[1]) for line in objects) + len(objects) * 160
                    run('git', '-C', str(neutral), 'cat-file', '--batch', input=names,
                        consume=writer.feed, limit=limit)
                    writer.finish()
                finally:
                    writer.close()
                # Include ref names and shallow metadata, not just commit diffs.
                header = bundle.open('rb')
                with header:
                    value = header.read(8 * 1024 * 1024)
                stop = value.find(b'\n\n')
                if stop < 0:
                    raise Budget('Snapshot bundle header exceeds budget')
                (object_review / 'references').write_bytes(value[:stop])
                names = cp.git(repo, 'ls-files', '-z', '--cached', '--others', '--exclude-standard')
                for raw in sorted(set(names.split(b'\0')) - {b''}):
                    name = os.fsdecode(raw)
                    directory_marker = name.endswith('/')
                    candidate = path(name[:-1] if directory_marker else name)
                    if cp.excluded(candidate):
                        record['excluded'].append(str(candidate))
                        continue
                    try:
                        info = (repo / candidate).lstat()
                    except FileNotFoundError:
                        continue  # A working-tree deletion remains absent.
                    if stat.S_ISDIR(info.st_mode):
                        continue  # Git emits untracked nested repos as directory/.
                    if directory_marker:
                        raise ValueError('Snapshot directory changed during capture')
                    destination = target / 'files' / candidate
                    work.file(destination)
                    added = copy_file(repo, descriptor, candidate, destination, cp, min(cp.MAX_BYTES - total, work.limit - work.used))
                    work.reserve(added)
                    total += added
                    count += 1
                    if total > cp.MAX_BYTES or count > cp.MAX_FILES:
                        raise Budget('Snapshot source exceeds budget')
                    record['files'].append(str(candidate))
                manifest['repositories'].append(record)
                PHASE = 'scan'
                cp.scan(neutral, history=True, revisions=('--all', 'HEAD', '--no-ext-diff', '--no-textconv'))
                PHASE = 'archive'
        encoded = json.dumps(manifest).encode()
        total += len(encoded)
        if total > cp.MAX_BYTES or len(encoded) > 8 * 1024 * 1024:
            raise Budget('Snapshot manifest exceeds budget')
        (stage / 'manifest.json').write_bytes(encoded)
        # Scan the exact copied files and manifest, plus every archived object
        # (including merge-only blobs and annotated-tag/commit messages).
        PHASE = 'scan'
        cp.scan(stage)
        cp.scan(review)
        PHASE = 'archive'
        pax = [0]
        def normalized(info):
            info.uid = info.gid = info.mtime = 0
            info.uname = info.gname = ''
            info.pax_headers = {}
            header_bytes = len(info.tobuf(format=tarfile.PAX_FORMAT)) - 512
            pax[0] += header_bytes
            if header_bytes > 16384 or pax[0] > MAX_PAX:
                raise Budget('Snapshot path metadata exceeds budget')
            return info
        created = False
        try:
            with output.open('xb') as sink:
                created = True
                os.chmod(output, 0o600)
                work.file(output)
                with tarfile.open(fileobj=ArchiveSink(sink, work), mode='w:gz', dereference=False) as archive:
                    archive.add(stage, arcname='snapshot', filter=normalized)
                if sink.tell() > MAX_BLOB:
                    raise Budget('Snapshot archive exceeds blob budget')
        except BaseException:
            if created:
                output.unlink()
            raise
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--work-bytes', type=int, default=512 * 1024 * 1024)
    args = parser.parse_args()
    try:
        manifest = pack(args.workspace, args.output, args.work_bytes)
        print(json.dumps({'version': 1, 'scanned': True,
                          'repositories': len(manifest['repositories']),
                          'bytes': Path(args.output).stat().st_size}))
        return 0
    except Exception as error:
        category = 'budget' if isinstance(error, Budget) or getattr(error, 'errno', None) in (errno.ENOSPC, errno.EDQUOT) else PHASE
        print(json.dumps({'error': category}), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
