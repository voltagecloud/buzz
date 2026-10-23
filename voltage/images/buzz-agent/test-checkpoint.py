"""Exercise packaged checkpoint save/restore without accounts or network access."""
import subprocess
import sys

SCRIPT = r'''
import base64, hashlib, importlib.machinery, importlib.util, io, json, os
from pathlib import Path
import subprocess, tarfile, tempfile

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
    git(upstream,'add','.')
    git(upstream,'commit','-m','chore: commit a credential upstream')
    clone = workspace/'clone'
    subprocess.check_call(['git','clone','-q',str(upstream),str(clone)],stderr=subprocess.DEVNULL)
    git(clone,'remote','set-url','origin','https://github.com/example/upstream.git')
    git(clone,'config','user.name','Checkpoint Fixture')
    git(clone,'config','user.email','fixture@example.invalid')
    checkpoint.pack(workspace,root/'upstream.tar.gz')
    (clone/'values.yaml').write_text(token+'github_token_2 = "ghp_'+'Zy9Xw8Vu7Ts6Rq5Po4Nm3Lk2Ji1Hg0Fe9Dc8'+'"\n')
    try:
        checkpoint.pack(workspace,root/'edited.tar.gz')
        raise AssertionError('secret scanner admitted a credential added to a tracked file')
    except ValueError:
        pass
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
    print('PASS: credentials already on the remote are allowed; one in an edited file or unpushed commit is refused with the scan step')

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
