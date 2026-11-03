"""Exercise shallow and legacy checkpoint histories in the packaged image."""
import subprocess
import sys

SCRIPT = r'''
import importlib.machinery, importlib.util, json, os, subprocess, tarfile, tempfile
from pathlib import Path

loader = importlib.machinery.SourceFileLoader('checkpoint', '/usr/local/bin/buzz-agent-checkpoint')
spec = importlib.util.spec_from_loader(loader.name, loader)
checkpoint = importlib.util.module_from_spec(spec)
loader.exec_module(checkpoint)

def git(repo, *args, input=None):
    result = subprocess.run(['git', '-c', 'commit.gpgsign=false', '-C', str(repo), *args],
                            input=input, capture_output=True, timeout=30)
    if result.returncode:
        raise AssertionError('fixture Git command failed; output suppressed')
    return result.stdout

def rewrite(root, original, name, edit):
    stage = root/name
    with tarfile.open(original) as archive:
        archive.extractall(stage, filter='data')
    manifest_path = stage/'snapshot/manifest.json'
    manifest = json.loads(manifest_path.read_text())
    edit(stage, manifest['repositories'][0])
    manifest_path.write_text(json.dumps(manifest))
    path = root/(name+'.tar.gz')
    with tarfile.open(path, 'w:gz') as archive:
        archive.add(stage/'snapshot', arcname='snapshot')
    return path

def refused(archive, destination):
    try:
        checkpoint.restore(archive, destination)
    except ValueError:
        assert not destination.exists(), 'failed restore left a workspace behind'
    else:
        raise AssertionError('invalid history was admitted')

with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)
    upstream = root/'upstream'
    upstream.mkdir()
    git(upstream, 'init', '-b', 'main')
    git(upstream, 'config', 'user.name', 'Checkpoint Fixture')
    git(upstream, 'config', 'user.email', 'fixture@example.invalid')
    (upstream/'source.txt').write_text('old\n')
    (upstream/'deleted.txt').write_text('delete this\n')
    git(upstream, 'add', '.')
    git(upstream, 'commit', '-m', 'test: create history')
    (upstream/'source.txt').write_text('upstream\n')
    git(upstream, 'commit', '-am', 'test: advance history')
    boundary = git(upstream, 'rev-parse', 'HEAD').decode().strip()
    workspace = root/'workspace'
    workspace.mkdir()
    repo = workspace/'example'
    git(root, 'clone', '--depth=1', upstream.as_uri(), str(repo))
    assert git(repo, 'rev-parse', '--is-shallow-repository').strip() == b'true'
    git(repo, 'config', 'user.name', 'Checkpoint Fixture')
    git(repo, 'config', 'user.email', 'fixture@example.invalid')
    git(repo, 'remote', 'set-url', 'origin', 'https://github.com/example/fixture.git')
    git(repo, 'checkout', '-b', 'feature')
    (repo/'source.txt').write_text('local commit\n')
    git(repo, 'commit', '-am', 'test: preserve local history')
    git(repo, 'tag', 'local-tag')
    head = git(repo, 'rev-parse', 'HEAD').decode().strip()
    (repo/'source.txt').write_text('working edit\n')
    (repo/'untracked.txt').write_text('new file\n')
    (repo/'deleted.txt').unlink()
    original_status = git(repo, 'status', '--porcelain')
    artifact = root/'checkpoint.tar.gz'
    manifest = checkpoint.pack(workspace, artifact)
    assert manifest['repositories'][0]['shallow'] == [boundary]

    def verify(archive, name):
        destination = root/name
        checkpoint.restore(archive, destination)
        restored = destination/'example'
        assert git(restored, 'rev-parse', 'HEAD').decode().strip() == head
        assert git(restored, 'branch', '--show-current').strip() == b'feature'
        assert git(restored, 'rev-parse', 'local-tag').decode().strip() == head
        assert git(restored, 'rev-list', '--count', 'HEAD').strip() == b'2'
        assert git(restored, 'rev-parse', '--is-shallow-repository').strip() == b'true'
        assert (restored/'.git/shallow').read_text().strip() == boundary
        assert git(restored, 'status', '--porcelain') == original_status
        assert (restored/'source.txt').read_text() == 'working edit\n'
        assert (restored/'untracked.txt').read_text() == 'new file\n'
        assert not (restored/'deleted.txt').exists()
        git(restored, 'fsck', '--full')

    verify(artifact, 'restored')
    print('PASS: a real depth-one clone preserves its boundary, local commit, branch, tag, edits, additions and deletion offline')

    legacy = rewrite(root, artifact, 'legacy', lambda stage, record: record.pop('shallow'))
    verify(legacy, 'restored-legacy')
    print('PASS: legacy archives missing shallow metadata restore the same history and workspace')

    for number, value in enumerate(([], None, 'invalid', ['not-a-commit'], ['0'*40])):
        malformed = rewrite(root, artifact, 'invalid-'+str(number),
                            lambda stage, record, value=value: record.update(shallow=value))
        refused(malformed, root/('refused-'+str(number)))
    print('PASS: invalid or incorrect explicit shallow metadata is refused, rather than inferred')

    def missing_tree(stage, record):
        record.pop('shallow')
        bundle = stage/'snapshot/repositories/0/history.bundle'
        header = bundle.read_bytes().split(b'\n\n', 1)[0] + b'\n\n'
        # Keep commits but omit their trees and blobs. The legacy compatibility
        # path must not confuse object corruption with shallow parent links.
        objects = (head+'\n'+boundary+'\n').encode()
        pack = git(repo, 'pack-objects', '--stdout', input=objects)
        bundle.write_bytes(header+pack)
    corrupt = rewrite(root, artifact, 'missing-tree', missing_tree)
    refused(corrupt, root/'refused-tree')
    print('PASS: legacy recovery refuses missing trees/blobs and removes the failed destination')

    original_git = checkpoint.git
    def damaged_bundle(repo, *args, **kwargs):
        result = original_git(repo, *args, **kwargs)
        if args[:2] == ('bundle', 'create'):
            Path(args[2]).write_bytes(b'invalid bundle\n')
        return result
    checkpoint.git = damaged_bundle
    rejected = root/'must-not-publish.tar.gz'
    try:
        try:
            checkpoint.pack(workspace, rejected)
        except ValueError:
            assert not rejected.exists()
        else:
            raise AssertionError('save published a checkpoint whose bundle cannot restore')
    finally:
        checkpoint.git = original_git
    print('PASS: save validates its bundle in an independent repository before producing an archive')

    # A damaged archive can produce large fsck diagnostics. Exercise the real
    # child process boundary and verify it is reaped when the output is capped.
    tools = root/'tools'
    tools.mkdir()
    pid_file = root/'fsck.pid'
    shim = tools/'git'
    shim.write_text('#!/usr/bin/env python3\nimport os, time\n'
                    +f'open({str(pid_file)!r}, "w").write(str(os.getpid()))\n'
                    +'while True: os.write(1, b"x" * 65536)\n')
    shim.chmod(0o700)
    original_path = os.environ['PATH']
    os.environ['PATH'] = str(tools)+os.pathsep+original_path
    try:
        try:
            checkpoint.history_check(root)
        except ValueError as error:
            assert 'output limit' in str(error)
        else:
            raise AssertionError('history diagnostics were not bounded')
    finally:
        os.environ['PATH'] = original_path
    try:
        os.kill(int(pid_file.read_text()), 0)
    except ProcessLookupError:
        pass
    else:
        raise AssertionError('history validator survived its output limit')
    print('PASS: excessive history diagnostics stop and reap the validator process')
'''

result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                         '--entrypoint', 'python3', sys.argv[1], '-'],
                        input=SCRIPT, text=True, capture_output=True, timeout=180)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
