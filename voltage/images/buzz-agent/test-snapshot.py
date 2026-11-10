"""Exercise the packaged scanner and self-contained archives without network."""
import subprocess
import sys
from pathlib import Path

SCRIPT = r'''
import importlib.machinery, importlib.util, json, os, subprocess, tarfile, tempfile, time
from pathlib import Path

def load(name, path):
    loader=importlib.machinery.SourceFileLoader(name,path)
    module=importlib.util.module_from_spec(importlib.util.spec_from_loader(name,loader))
    loader.exec_module(module)
    return module
snapshot=load('snapshot','/usr/local/bin/buzz-agent-snapshot')
cp=snapshot.checkpoint()
policy=json.loads(Path('/tmp/policy.json').read_text())
for value,expected in policy:
    assert cp.excluded(Path(value))==expected,(value,expected)
assert all(any(value==name+'/nested' and expected for value,expected in policy) for name in cp.EXCLUDED)
assert all(any(value=='repo/'+name and expected for value,expected in policy) for name in cp.PRIVATE_NAMES)

def git(repo,*args):
    result=subprocess.run(['git','-c','commit.gpgsign=false','-c','core.hooksPath=/dev/null','-C',str(repo),*args],capture_output=True)
    assert result.returncode==0,'fixture Git command failed'
    return result.stdout.decode().strip()

def repository(root,format='sha1'):
    repo=root/'workspace'/'example'
    repo.mkdir(parents=True)
    git(repo,'init','--object-format='+format)
    git(repo,'config','user.name','Snapshot Fixture')
    git(repo,'config','user.email','fixture@example.invalid')
    return repo

def expect_refusal(workspace,output,category=None,**options):
    try:
        snapshot.pack(workspace,output,**options)
        raise AssertionError('unsafe snapshot was accepted')
    except (ValueError,OSError,subprocess.SubprocessError):
        assert not output.exists(),'failed archive remained on disk'
        if category=='scan': assert snapshot.PHASE=='scan'

with tempfile.TemporaryDirectory() as temp:
    root=Path(temp)
    for format in ('sha1','sha256'):
        case=root/format
        repo=repository(case,format)
        (repo/'source.rs').write_text('initial\n')
        (repo/'deleted.txt').write_text('tracked deletion\n')
        (repo/'.env.example').write_text('PUBLIC_URL=https://example.invalid\n')
        git(repo,'add','.')
        git(repo,'commit','-m','initial')
        git(repo,'branch','local-only')
        (repo/'source.rs').write_text('edited\n')
        (repo/'deleted.txt').unlink()
        (repo/'new.txt').write_text('untracked\n')
        (repo/'.env').write_text('PRIVATE-LOOSE-FILE')
        (repo/'credentials.json').write_text('PRIVATE-LOOSE-FILE')
        (repo/'tool.sh').write_text('echo fixture\n')
        (repo/'tool.sh').chmod(0o755)
        (repo/'link').symlink_to('source.rs')
        # Valid own PAX output must remain bounded and round-trip.
        long=repo/('long-'+'x'*110)
        long.write_text('long path\n')
        archive=case/'snapshot.tar.gz'
        manifest=snapshot.pack(case/'workspace',archive)
        assert manifest['snapshot'] is True and manifest['scanned'] is True
        record=manifest['repositories'][0]
        assert record['base']=='bundle' and record['shallow']==[]
        assert 'deleted.txt' not in record['files']
        assert '.env.example' in record['excluded']
        assert 'source.rs' in record['files'] and 'new.txt' in record['files']
        extracted=case/'extracted'
        with tarfile.open(archive) as package:
            names=package.getnames()
            assert not any(name.endswith(('/.env','/credentials.json','/.env.example')) for name in names)
            assert package.getmember('snapshot/repositories/0/files/link').issym()
            assert package.getmember('snapshot/repositories/0/files/tool.sh').mode&0o111
            assert all(member.mtime==0 for member in package)
            package.extractall(extracted,filter='data')
        history=case/'history'
        snapshot.DEADLINE=time.monotonic()+60
        cp=snapshot.checkpoint()
        cp.restore_history(extracted/'snapshot/repositories/0/history.bundle',record,history)
        assert git(history,'show','HEAD:.env.example').startswith('PUBLIC_URL=')
        assert 'refs/heads/local-only' in git(history,'show-ref')
        assert (extracted/'snapshot/repositories/0/files'/long.name).read_text()=='long path\n'
        print('PASS: full history, exclusions, dirty files, links, PAX and '+format)

    # Shallow bundle restores with its explicit local boundary, without origin.
    source=root/'sha1/workspace/example'
    (source/'source.rs').write_text('committed second\n')
    git(source,'add','source.rs')
    git(source,'commit','-m','second')
    shallow=root/'shallow/workspace/example'
    shallow.parent.mkdir(parents=True)
    git(source,'clone','--depth=1',source.as_uri(),str(shallow))
    shallow_archive=root/'shallow/snapshot.tar.gz'
    record=snapshot.pack(shallow.parent,shallow_archive)['repositories'][0]
    assert record['shallow']
    staging=root/'shallow-extracted'
    with tarfile.open(shallow_archive) as package: package.extractall(staging,filter='data')
    snapshot.DEADLINE=time.monotonic()+60
    cp=snapshot.checkpoint()
    bundle=staging/'snapshot/repositories/0/history.bundle'
    cp.restore_history(bundle,record,root/'shallow-good')
    broken=dict(record,shallow=[])
    try:
        cp.restore_history(bundle,broken,root/'shallow-bad')
        raise AssertionError('undeclared missing parents were accepted')
    except ValueError: pass
    print('PASS: explicit shallow boundaries; undeclared missing parents refused')

    case=root/'nested';outer=repository(case)
    (outer/'outer.txt').write_text('outer\n');git(outer,'add','.');git(outer,'commit','-m','outer')
    nested=outer/'nested';git(outer,'init',str(nested))
    git(nested,'config','user.name','Snapshot Fixture');git(nested,'config','user.email','fixture@example.invalid')
    (nested/'inner.txt').write_text('inner\n');git(nested,'add','.');git(nested,'commit','-m','inner')
    manifest=snapshot.pack(outer.parent,case/'nested.tar.gz')
    assert [record['path'] for record in manifest['repositories']]==['example','example/nested']
    with tarfile.open(case/'nested.tar.gz') as package:
        assert package.extractfile('snapshot/repositories/0/files/outer.txt').read()==b'outer\n'
        assert package.extractfile('snapshot/repositories/1/files/inner.txt').read()==b'inner\n'
    print('PASS: real outer plus untracked nested Git repository have separate complete bundles')

    # Real commits between source preflight and capture must be charged BEFORE import.
    for succeeds,budget in ((False,20*1024*1024),(True,128*1024*1024)):
        case=root/('racing-control' if succeeds else 'racing-refusal');repo=repository(case)
        (repo/'safe').write_text('safe\n');git(repo,'add','.');git(repo,'commit','-m','initial')
        cp=snapshot.checkpoint();original_git=cp.git;original_restore=cp.restore_history
        seen={'changed':False,'imported':False}
        def racing(directory,*args,**options):
            if not seen['changed'] and args==('rev-parse','--verify','HEAD'):
                seen['changed']=True
                with (repo/'late-history').open('wb') as stream: stream.truncate(8*1024*1024)
                git(repo,'add','late-history');git(repo,'commit','-m','concurrent source')
                (repo/'late-history').unlink()
            return original_git(directory,*args,**options)
        def restoring(*args,**options):
            seen['imported']=True
            return original_restore(*args,**options)
        cp.git=racing;cp.restore_history=restoring
        factory=snapshot.checkpoint;snapshot.checkpoint=lambda:cp
        output=case/'snapshot.tar.gz'
        try:
            if succeeds:
                snapshot.pack(repo.parent,output,work_bytes=budget)
                assert output.exists() and seen['imported']
            else:
                try:
                    snapshot.pack(repo.parent,output,work_bytes=budget)
                    raise AssertionError('captured history escaped work accounting')
                except snapshot.Budget:
                    assert not seen['imported'] and not output.exists()
            assert seen['changed']
        finally: snapshot.checkpoint=factory
    print('PASS: concurrent source growth refused before import at20MiB and accepted at128MiB control')

    # The packaged native inspector is required; invalid metadata cannot fall back.
    case=root/'inspector-required';repo=repository(case)
    (repo/'safe').write_text('safe\n');git(repo,'add','.');git(repo,'commit','-m','initial')
    original_run=snapshot.run
    for missing in (False,True):
        def invalid_inspector(*args,**options):
            if args[0]=='/usr/local/bin/buzz':
                if missing: raise FileNotFoundError('required inspector is unavailable')
                return b'{}'
            return original_run(*args,**options)
        snapshot.run=invalid_inspector
        try: expect_refusal(repo.parent,case/'refused.tar.gz')
        finally: snapshot.run=original_run
    print('PASS: missing or invalid native inspector has no source-inventory fallback')

    # No current or historical credential may be hidden by repo/global ignores.
    secret='sk-ant-oat01-'+'Z'*90
    for historical in (False,True):
        case=root/('old-secret' if historical else 'new-secret')
        repo=repository(case)
        (repo/'safe').write_text('safe\n')
        git(repo,'add','safe');git(repo,'commit','-m','initial')
        (repo/'payload.txt').write_text(secret)
        if historical:
            git(repo,'add','payload.txt');git(repo,'commit','-m','old secret')
            (repo/'payload.txt').unlink();git(repo,'commit','-am','remove secret')
        (repo/'.gitleaksignore').write_text('*\n')
        (repo/'.gitleaks.toml').write_text('[allowlist]\nregexes=[".*"]\n')
        os.environ['GITLEAKS_CONFIG']=str(repo/'.gitleaks.toml')
        expect_refusal(case/'workspace',case/'refused.tar.gz',category='scan')
    print('PASS: staged working-tree and deleted historical secret refusal ignores bypass attempts')

    # A secret introduced only by a merge and an annotated tag still gets scanned.
    case=root/'metadata-secret';repo=repository(case)
    (repo/'safe').write_text('safe\n');git(repo,'add','safe');git(repo,'commit','-m','initial')
    git(repo,'tag','-a','secret-tag','-m',secret)
    expect_refusal(case/'workspace',case/'refused.tar.gz',category='scan')
    print('PASS: annotated-tag secret is scanned as an archived object')

    case=root/'merge-secret';repo=repository(case)
    (repo/'safe').write_text('safe\n');git(repo,'add','safe');git(repo,'commit','-m','initial')
    base=git(repo,'branch','--show-current')
    git(repo,'checkout','-b','side')
    (repo/'side').write_text('side\n');git(repo,'add','side');git(repo,'commit','-m','side')
    git(repo,'checkout',base)
    (repo/'main').write_text('main\n');git(repo,'add','main');git(repo,'commit','-m','main')
    git(repo,'merge','--no-commit','side')
    (repo/'payload.txt').write_text(secret);git(repo,'add','payload.txt');git(repo,'commit','-m','merge')
    (repo/'payload.txt').unlink();git(repo,'commit','-am','remove merge secret')
    expect_refusal(case/'workspace',case/'refused.tar.gz',category='scan')
    print('PASS: deleted merge-only secret is scanned from the exact bundled object')

    case=root/'bounds';repo=repository(case)
    (repo/'safe').write_text('safe\n');git(repo,'add','safe');git(repo,'commit','-m','initial')
    expect_refusal(case/'workspace',case/'small-budget.tar.gz',work_bytes=1024*1024)
    (repo/'escape').symlink_to('/etc/passwd')
    expect_refusal(case/'workspace',case/'external-link.tar.gz')
    (repo/'escape').unlink()
    (repo/'bad').write_bytes(b'x'*1024)
    cp=snapshot.checkpoint();cp.MAX_BYTES=64
    original=snapshot.checkpoint;snapshot.checkpoint=lambda:cp
    expect_refusal(case/'workspace',case/'scaled-source.tar.gz')
    snapshot.checkpoint=original
    output=case/'preserve-existing';output.write_text('keep me')
    try: snapshot.pack(case/'workspace',output);raise AssertionError('existing output overwritten')
    except ValueError: assert output.read_text()=='keep me'
    unborn=repository(root/'unborn')
    expect_refusal(unborn.parent,root/'unborn/refused.tar.gz')
    print('PASS: capacity/source budgets, external links, existing files and unborn HEAD refused')

    # Bound both generic subprocess output and compressed archive writes BEFORE writes.
    snapshot.DEADLINE=time.monotonic()+60
    limited=root/'limited-output'
    try:
        snapshot.run('/usr/bin/python3','-c','import sys;sys.stdout.buffer.write(b"x"*8192)',output=limited,limit=64)
        raise AssertionError('subprocess output bound was bypassed')
    except snapshot.Budget: assert limited.stat().st_size<=64
    import io
    sink=io.BytesIO();work=snapshot.Work(root,512*1024*1024)
    previous=snapshot.MAX_BLOB;snapshot.MAX_BLOB=8
    try:
        snapshot.ArchiveSink(sink,work).write(b'x'*9)
        raise AssertionError('compressed output was written before admission')
    except snapshot.Budget: assert sink.getvalue()==b''
    finally: snapshot.MAX_BLOB=previous
    # Intermediate source symlinks cannot redirect a copy outside the open repo.
    cp=snapshot.checkpoint();snapshot.DEADLINE=time.monotonic()+60
    (repo/'sub').mkdir();outside=root/'outside';outside.mkdir()
    (outside/'value').write_text('OUTSIDE-WORKSPACE')
    (repo/'sub').rmdir();(repo/'sub').symlink_to(outside)
    with snapshot.opened_repo(repo.parent,Path('example')) as (opened,descriptor):
        try:
            snapshot.copy_file(opened,descriptor,snapshot.path('sub/value'),root/'copy-refused',cp,100)
            raise AssertionError('intermediate symlink redirected a source read')
        except OSError: assert not (root/'copy-refused').exists()
    small=root/'discovery-bound';small.mkdir()
    for number in range(3): (small/str(number)).write_text('x')
    previous=cp.MAX_FILES;cp.MAX_FILES=1
    try:
        snapshot.discover(small,cp)
        raise AssertionError('streamed discovery budget was bypassed')
    except snapshot.Budget: pass
    finally: cp.MAX_FILES=previous
    print('PASS: streaming output admission and descriptor-relative source containment')
'''

policy = Path(__file__).with_name('snapshot-policy.json').resolve()
result = subprocess.run(['docker','run','--rm','-i','--network','none','--read-only',
    '--tmpfs','/tmp:rw,nosuid,nodev,size=1g,uid=10001,gid=10001,mode=0700',
    '--memory','1g','--cap-drop','ALL','--security-opt','no-new-privileges',
    '-v',f'{policy}:/tmp/policy.json:ro','--entrypoint','python3',sys.argv[1],'-'],
    input=SCRIPT,text=True,capture_output=True,timeout=240)
print(result.stdout,end='')
print(result.stderr,end='',file=sys.stderr)
sys.exit(result.returncode)
