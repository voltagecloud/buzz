"""Exercise packaged checkpoint save/restore without accounts or network access."""
import subprocess
import sys

SCRIPT = r'''
import base64, hashlib, importlib.machinery, importlib.util, io, json, os
from pathlib import Path
import socket, subprocess, tarfile, tempfile, threading, time

loader = importlib.machinery.SourceFileLoader('checkpoint', '/usr/local/bin/buzz-agent-checkpoint')
spec = importlib.util.spec_from_loader(loader.name, loader)
checkpoint = importlib.util.module_from_spec(spec)
loader.exec_module(checkpoint)

def git(repo, *args):
    return subprocess.check_output(['git','-c','commit.gpgsign=false','-C',str(repo),*args],stderr=subprocess.DEVNULL).decode().strip()

with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)
    workspace = root/'workspace'
    repo = workspace/'example'
    repo.mkdir(parents=True)
    git(repo,'init')
    git(repo,'config','user.name','Checkpoint Fixture')
    git(repo,'config','user.email','fixture@example.invalid')
    origin = 'ssh://git@ssh.github.com:443/example/fixture.git'
    git(repo,'remote','add','origin',origin)
    for configured, expected in [
        (origin, origin),
        ('git@github.com:example/fixture.git', 'https://github.com/example/fixture.git'),
        ('https://user:password@github.com:8443/example/fixture.git?token=private#secret', 'https://github.com:8443/example/fixture.git'),
        ('ssh://git:password@example.com:2222/fixture.git', 'ssh://git@example.com:2222/fixture.git'),
        ('ssh://git@[2001:db8::1]:2222/fixture.git', 'ssh://git@[2001:db8::1]:2222/fixture.git'),
    ]:
        git(repo,'remote','set-url','origin',configured)
        assert checkpoint.public_origin(repo) == expected
    git(repo,'remote','set-url','origin',origin)
    (repo/'source.rs').write_text('fn main() {}\n')
    (repo/'deleted.txt').write_text('delete this\n')
    (repo/'.gitignore').write_text('.env\ntarget/\n')
    (repo/'.env.example').write_text('API_URL=https://example.invalid\n')
    git(repo,'add','.')
    git(repo,'commit','-m','feat: create fixture')
    head = git(repo,'rev-parse','HEAD')
    (repo/'source.rs').write_text('fn main() { println!("edited"); }\n')
    (repo/'new.rs').write_text('pub const NEW: bool = true;\n')
    (repo/'deleted.txt').unlink()
    (repo/'.env').write_text('DO-NOT-CHECKPOINT-ACCOUNT-CREDENTIAL\n')
    (repo/'target').mkdir()
    (repo/'target/build-output').write_text('DO-NOT-CHECKPOINT-BUILD-OUTPUT\n')
    (workspace/'.config/buzz-agent').mkdir(parents=True)
    (workspace/'.config/buzz-agent/github-token').write_text('DO-NOT-CHECKPOINT-ACCOUNT-CREDENTIAL\n')
    (repo/'source-link').symlink_to('source.rs')
    artifact = root/'snapshot.tar.gz'
    manifest = checkpoint.pack(workspace,artifact)
    assert len(manifest['repositories']) == 1
    with tarfile.open(artifact) as archive:
        names = archive.getnames()
        assert not any(n.endswith('/.env') or '/target/' in n or '/.config/' in n for n in names)
    destination = root/'restored'
    checkpoint.restore(artifact,destination)
    restored = destination/'example'
    assert git(restored,'rev-parse','HEAD') == head
    assert (restored/'source.rs').read_bytes() == (repo/'source.rs').read_bytes()
    assert (restored/'new.rs').read_bytes() == (repo/'new.rs').read_bytes()
    assert not (restored/'deleted.txt').exists()
    assert (restored/'.env.example').read_text() == 'API_URL=https://example.invalid\n'
    assert (restored/'source-link').is_symlink()
    assert git(restored,'remote','get-url','origin') == origin
    try:
        checkpoint.restore(artifact,destination)
        raise AssertionError('restore overwrote an existing workspace')
    except ValueError:
        pass
    # Without an origin base the manifest stays version 1, which older images
    # restore; archives written before origin bases still restore too.
    assert manifest['version'] == 1 and manifest['repositories'][0]['base'] == 'bundle'
    legacy = root/'legacy'
    with tarfile.open(artifact) as archive:
        archive.extractall(legacy, filter='data')
    old_manifest = json.loads((legacy/'snapshot/manifest.json').read_text())
    for record in old_manifest['repositories']:
        del record['base'], record['deleted']
    (legacy/'snapshot/manifest.json').write_text(json.dumps(old_manifest))
    with tarfile.open(root/'legacy.tar.gz','w:gz') as archive:
        archive.add(legacy/'snapshot',arcname='snapshot')
    checkpoint.restore(root/'legacy.tar.gz',root/'restored-legacy')
    assert git(root/'restored-legacy/example','rev-parse','HEAD') == head
    assert (root/'restored-legacy/example/new.rs').read_bytes() == (repo/'new.rs').read_bytes()
    assert not (root/'restored-legacy/example/deleted.txt').exists()
    print('PASS: SSH origin identity/port, local commit, edited/untracked source, deletion, and symlink survive restore; account files and build outputs excluded; excluded tracked files keep committed content')

    (repo/'leak.txt').write_text('github_token = "ghp_'+'aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL3oR6'+'"\n')
    try:
        checkpoint.pack(workspace,root/'leaked.tar.gz')
        raise AssertionError('secret scanner admitted a synthetic GitHub token')
    except ValueError:
        pass
    (repo/'leak.txt').write_text('CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-'+'SyntheticFixture'*6+'\n')
    try:
        checkpoint.pack(workspace,root/'leaked.tar.gz')
        raise AssertionError('secret scanner admitted a synthetic Claude subscription token')
    except ValueError:
        pass
    (repo/'leak.txt').unlink()
    print('PASS: untracked GitHub or Claude credential causes checkpoint refusal')

    # A credential already on the remote does not block the checkpoint; one the
    # agent adds, in a file or in an unpushed commit, does.
    token = 'github_token = "ghp_'+'aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0iL3oR6'+'"\n'
    upstream = root/'upstream'
    upstream.mkdir()
    git(upstream,'init','-b','main')
    git(upstream,'config','user.name','Checkpoint Fixture')
    git(upstream,'config','user.email','fixture@example.invalid')
    (upstream/'values.yaml').write_text(token)
    (upstream/'large.bin').write_bytes(bytes(1024*1024))
    (upstream/'remove.txt').write_text('pushed, then deleted locally\n')
    git(upstream,'add','.')
    git(upstream,'commit','-m','chore: commit a credential upstream')
    clone = workspace/'clone'
    subprocess.check_call(['git','clone','-q',str(upstream),str(clone)],stderr=subprocess.DEVNULL)
    git(clone,'remote','set-url','origin','https://github.com/example/upstream.git')
    # Origin-based restore clones origin; offline, serve it from the fixture.
    os.environ['GIT_CONFIG_GLOBAL'] = str(root/'gitconfig')
    git(root,'config','--global','url.'+str(upstream)+'.insteadOf','https://github.com/example/upstream.git')
    git(clone,'config','user.name','Checkpoint Fixture')
    git(clone,'config','user.email','fixture@example.invalid')
    checkpoint.pack(workspace,root/'upstream.tar.gz')
    # Editing other lines of a file that holds an upstream credential is allowed,
    # and the checkpoint restores.
    (clone/'values.yaml').write_text(token+'replicas: 2\n')
    checkpoint.pack(workspace,root/'upstream-edited.tar.gz')
    checkpoint.restore(root/'upstream-edited.tar.gz',root/'restored-upstream')
    assert (root/'restored-upstream/clone/values.yaml').read_text() == token+'replicas: 2\n'
    leaked = token+'replicas: 2\ngithub_token_2 = "ghp_'+'Zy9Xw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0Fe9Dc8'+'"\n'
    (clone/'values.yaml').write_text(leaked)
    try:
        checkpoint.pack(workspace,root/'edited.tar.gz')
        raise AssertionError('secret scanner admitted a credential added to a tracked file')
    except ValueError:
        pass
    # Restore applies the same scope: an archive whose files add a credential
    # is refused, and leaves no destination behind.
    forged = root/'forged'
    with tarfile.open(root/'upstream-edited.tar.gz') as archive:
        archive.extractall(forged, filter='data')
    index = [r['path'] for r in json.loads((forged/'snapshot/manifest.json').read_text())['repositories']].index('clone')
    (forged/'snapshot/repositories'/str(index)/'files/values.yaml').write_text(leaked)
    with tarfile.open(root/'forged.tar.gz','w:gz') as archive:
        archive.add(forged/'snapshot',arcname='snapshot')
    try:
        checkpoint.restore(root/'forged.tar.gz',root/'restored-forged')
        raise AssertionError('restore admitted a credential the archive added')
    except ValueError:
        pass
    assert not (root/'restored-forged').exists()
    git(clone,'commit','-am','chore: commit a credential locally')
    try:
        checkpoint.pack(workspace,root/'committed.tar.gz')
        raise AssertionError('secret scanner admitted a credential in an unpushed commit')
    except ValueError:
        pass
    # The exit code names the step, because buzz-acp discards helper output.
    cli = subprocess.run(['buzz-agent-checkpoint','pack','--workspace',str(workspace),
                          '--archive',str(root/'cli.tar.gz')],capture_output=True,text=True)
    assert cli.returncode == 11 and 'while scanning for secrets' in cli.stderr, cli.returncode
    git(clone,'reset','-q','--hard','origin/main')
    checkpoint.pack(workspace,root/'reset.tar.gz')
    print('PASS: credentials already on the remote are allowed and restore; one added in a file, an unpushed commit or an archive is refused with the scan step')

    # With an HTTPS origin, a checkpoint holds only what origin lacks, so
    # pushed content larger than the budget does not count against it.
    git(clone,'checkout','-q','-b','feature')
    (clone/'local.txt').write_text('unpushed commit\n')
    git(clone,'add','local.txt')
    git(clone,'commit','-m','feat: add an unpushed commit')
    # A remote-tracking ref origin does not have, as a bundle restore leaves:
    # the save asks origin, so the commit stays in the checkpoint.
    git(clone,'update-ref','refs/remotes/origin/feature','HEAD')
    (clone/'remove.txt').unlink()
    (clone/'untracked.txt').write_text('untracked\n')
    budget = checkpoint.MAX_BYTES
    checkpoint.MAX_BYTES = 512*1024
    try:
        thin = checkpoint.pack(workspace,root/'thin.tar.gz')
        # With an SSH origin the same workspace is a bundle snapshot, which
        # counts the pushed tree and exceeds the budget.
        git(clone,'remote','set-url','origin','ssh://git@example.com/upstream.git')
        try:
            checkpoint.pack(workspace,root/'ssh.tar.gz')
            raise AssertionError('an SSH origin repository skipped its bundle snapshot')
        except ValueError as error:
            assert 'budget' in str(error), error
        git(clone,'remote','set-url','origin','https://github.com/example/upstream.git')
    finally:
        checkpoint.MAX_BYTES = budget
    record = next(r for r in thin['repositories'] if r['path'] == 'clone')
    assert record['base'] == 'origin' and record['deleted'] == ['remove.txt'], record
    # The unpushed commit travels in the bundle, not as a changed file.
    assert record['files'] == ['untracked.txt'], record['files']
    with tarfile.open(root/'thin.tar.gz') as archive:
        assert not any(n.endswith('/large.bin') for n in archive.getnames())
    checkpoint.restore(root/'thin.tar.gz',root/'restored-thin')
    restored = root/'restored-thin/clone'
    assert git(restored,'rev-parse','HEAD') == git(clone,'rev-parse','HEAD')
    assert git(restored,'branch','--show-current') == 'feature'
    assert git(restored,'config','--get','remote.origin.url') == 'https://github.com/example/upstream.git'
    assert (restored/'large.bin').stat().st_size == 1024*1024
    assert (restored/'local.txt').read_text() == 'unpushed commit\n'
    assert (restored/'untracked.txt').read_text() == 'untracked\n'
    assert not (restored/'remove.txt').exists()
    print('PASS: an HTTPS origin repository saves only unpushed commits and changed files, within budget, and restores from origin; an SSH origin keeps the full bundle')

    # Origin-based restore replaces each saved path over the origin checkout,
    # whatever type the agent changed it from, and recreates refs origin lacks.
    types_upstream = root/'types-upstream'
    types_upstream.mkdir()
    git(types_upstream,'init','-b','main')
    git(types_upstream,'config','user.name','Checkpoint Fixture')
    git(types_upstream,'config','user.email','fixture@example.invalid')
    for name in ('a.txt','b.txt','c.txt'):
        (types_upstream/name).write_text(name+'\n')
    (types_upstream/'d').mkdir()
    (types_upstream/'d/x.txt').write_text('in a directory\n')
    (types_upstream/'retarget').symlink_to('a.txt')
    (types_upstream/'to-file').symlink_to('a.txt')
    git(types_upstream,'add','.')
    git(types_upstream,'commit','-m','test: create typed paths')
    types = workspace/'types'
    subprocess.check_call(['git','clone','-q',str(types_upstream),str(types)],stderr=subprocess.DEVNULL)
    git(types,'remote','set-url','origin','https://github.com/example/types.git')
    git(root,'config','--global','url.'+str(types_upstream)+'.insteadOf','https://github.com/example/types.git')
    git(types,'config','user.name','Checkpoint Fixture')
    git(types,'config','user.email','fixture@example.invalid')
    (types/'retarget').unlink()
    (types/'retarget').symlink_to('b.txt')
    (types/'to-file').unlink()
    (types/'to-file').write_text('now a regular file\n')
    (types/'c.txt').unlink()
    (types/'c.txt').mkdir()
    (types/'c.txt/inner.txt').write_text('file became a directory\n')
    (types/'d/x.txt').unlink()
    (types/'d').rmdir()
    (types/'d').write_text('directory became a file\n')
    git(types,'tag','light')
    git(types,'-c','tag.gpgsign=false','tag','-a','-m','annotated, unpushed','annotated')
    git(types,'branch','wip')
    typed = checkpoint.pack(workspace,root/'types.tar.gz')
    assert typed['version'] == 2
    assert next(r for r in typed['repositories'] if r['path'] == 'types')['base'] == 'origin'
    checkpoint.restore(root/'types.tar.gz',root/'restored-types')
    restored = root/'restored-types/types'
    assert os.readlink(restored/'retarget') == 'b.txt'
    assert not (restored/'to-file').is_symlink()
    assert (restored/'to-file').read_text() == 'now a regular file\n'
    assert (restored/'a.txt').read_text() == 'a.txt\n', 'a write went through the old symlink'
    assert (restored/'c.txt/inner.txt').read_text() == 'file became a directory\n'
    assert (restored/'d').read_text() == 'directory became a file\n'
    assert git(restored,'status','--porcelain') == git(types,'status','--porcelain')
    for ref in ('refs/tags/light','refs/tags/annotated','refs/heads/wip'):
        assert git(restored,'rev-parse',ref) == git(types,'rev-parse',ref), ref
    assert git(restored,'cat-file','-t','annotated') == 'tag'
    assert git(restored,'cat-file','tag','annotated') == git(types,'cat-file','tag','annotated')
    # A crafted deletion cannot reach through a symlinked parent.
    forged = root/'forged-types'
    with tarfile.open(root/'types.tar.gz') as archive:
        archive.extractall(forged, filter='data')
    forged_manifest = json.loads((forged/'snapshot/manifest.json').read_text())
    (root/'outside.txt').write_text('outside\n')
    git(types_upstream,'rm','-rq','d')
    (types_upstream/'escape').symlink_to(root)
    git(types_upstream,'add','escape')
    git(types_upstream,'commit','-m','test: add a symlink out of the repository')
    record = next(r for r in forged_manifest['repositories'] if r['path'] == 'types')
    record['head'] = git(types_upstream,'rev-parse','HEAD')
    record['branch'] = ''
    record['files'], record['refs'], record['tags'] = [], [], {}
    record['deleted'] = ['escape/outside.txt']
    (forged/'snapshot/manifest.json').write_text(json.dumps(forged_manifest))
    with tarfile.open(root/'forged-types.tar.gz','w:gz') as archive:
        archive.add(forged/'snapshot',arcname='snapshot')
    checkpoint.restore(root/'forged-types.tar.gz',root/'restored-forged-types')
    assert (root/'outside.txt').read_text() == 'outside\n'
    print('PASS: origin restore replaces retargeted symlinks, symlinks turned files, and file/directory swaps in place; unpushed tags, annotated tag objects and branches survive; deletions never follow a symlink')

    # Unreachable origins cost one shared deadline, not one per repository.
    server = socket.socket()
    server.bind(('127.0.0.1', 0))
    server.listen()
    held = []
    threading.Thread(target=lambda: [held.append(server.accept()) for _ in range(8)], daemon=True).start()
    hanging = 'https://127.0.0.1:%d/example.git' % server.getsockname()[1]
    deadline = checkpoint.ORIGIN_SECONDS
    checkpoint.ORIGIN_SECONDS = 2
    try:
        started = time.monotonic()
        listings = checkpoint.origin_listings([hanging]*4 + ['ssh://git@example.com/x.git'])
        elapsed = time.monotonic() - started
    finally:
        checkpoint.ORIGIN_SECONDS = deadline
    assert listings == [None]*5, listings
    assert elapsed < 5, elapsed
    print('PASS: four hanging origins and an SSH origin resolve to bundle saves within one shared deadline')

    (root/'private').write_text('outside workspace')
    (repo/'escape').symlink_to(root/'private')
    try:
        checkpoint.pack(workspace,root/'escaped.tar.gz')
        raise AssertionError('external symlink was admitted')
    except ValueError:
        pass
    (repo/'escape').unlink()
    evil = root/'evil.tar.gz'
    with tarfile.open(evil,'w:gz') as archive:
        member = tarfile.TarInfo('../escape')
        member.size = 1
        archive.addfile(member,io.BytesIO(b'x'))
    try:
        checkpoint.restore(evil,root/'evil-restore')
        raise AssertionError('archive traversal was admitted')
    except ValueError:
        pass
    print('PASS: unsafe source symlinks and archive paths are refused')
'''

result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                         '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges',
                         '--entrypoint', 'python3', sys.argv[1], '-'],
                        input=SCRIPT, text=True, capture_output=True, timeout=180)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
