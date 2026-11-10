"""Prove real producer scratch stays inside durable caller cleanup, including SIGKILL."""
import subprocess
import sys

SCRIPT = r'''
import importlib.machinery, importlib.util, json, os, shutil, signal, subprocess, tempfile, time
from pathlib import Path

def load():
    loader=importlib.machinery.SourceFileLoader('snapshot','/usr/local/bin/buzz-agent-snapshot')
    module=importlib.util.module_from_spec(importlib.util.spec_from_loader('snapshot',loader))
    loader.exec_module(module)
    return module

def git(repo,*args):
    output=subprocess.run(['git','-C',str(repo),*args],capture_output=True)
    assert output.returncode==0,'fixture Git operation failed'
    return output.stdout

with tempfile.TemporaryDirectory() as temporary:
    root=Path(temporary);workspace=root/'workspace';workspace.mkdir()
    git(workspace,'init','--template=')
    git(workspace,'config','user.name','Fixture');git(workspace,'config','user.email','fixture@example.invalid')
    (workspace/'safe').write_text('safe\n');git(workspace,'add','safe');git(workspace,'-c','commit.gpgsign=false','commit','-m','fixture')
    job=root/'job';job.mkdir(mode=0o700);foreign=root/'foreign';foreign.mkdir()
    marker=job/'scratch.json'
    child_code=r"""
import importlib.machinery,importlib.util,json,sys,tempfile,time
from pathlib import Path
loader=importlib.machinery.SourceFileLoader('snapshot','/usr/local/bin/buzz-agent-snapshot')
snapshot=importlib.util.module_from_spec(importlib.util.spec_from_loader('snapshot',loader));loader.exec_module(snapshot)
tempfile.tempdir=sys.argv[3]  # Deliberately cache a foreign root before pack clears its environment.
original=snapshot.inventory
marker=Path(sys.argv[2]).parent/'scratch.json'
def hold(directory,cp):
    result=original(directory,cp)
    if Path(directory).name=='history-0':
        marker.write_text(json.dumps({'scratch':str(Path(directory).parent.resolve())}))
        time.sleep(60)
    return result
snapshot.inventory=hold
snapshot.pack(sys.argv[1],sys.argv[2],work_bytes=128*1024*1024)
"""
    child=subprocess.Popen(['python3','-c',child_code,str(workspace),str(job/'archive.tar.gz'),str(foreign)],stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
    try:
        deadline=time.monotonic()+15
        while not marker.exists() and child.poll() is None and time.monotonic()<deadline:time.sleep(0.01)
        assert marker.exists(),'producer did not reach real neutral inventory'
        scratch=Path(json.loads(marker.read_text())['scratch'])
        assert scratch.is_relative_to(job),'producer scratch escaped its durable job directory'
        assert (scratch/'snapshot/repositories/0/history.bundle').is_file()
        os.killpg(child.pid,signal.SIGKILL);child.communicate(timeout=5)
        assert scratch.exists(),'SIGKILL unexpectedly ran Python cleanup'
        shutil.rmtree(job)
        assert not scratch.exists() and not list(foreign.iterdir())
    finally:
        if child.poll() is None:os.killpg(child.pid,signal.SIGKILL)
        child.communicate(timeout=5)
    print('PASS: cached foreign tempfile root cannot escape caller job; real SIGKILL cleanup removes captured scratch')

    snapshot=load();visible=workspace/'visible-job';visible.mkdir()
    try:
        snapshot.pack(workspace,visible/'archive.tar.gz')
        raise AssertionError('visible output parent was accepted inside source')
    except ValueError:assert not list(visible.iterdir())
    visible.rmdir()
    private=workspace/'.local/state/job';private.mkdir(parents=True)
    for number in range(100):(private/(str(number)+'x'*100)).write_text('private caller state\n')
    cp=snapshot.checkpoint();original_git=cp.git
    def limited(directory,*args,**options):
        if args and args[0]=='ls-files':options['limit']=64
        return original_git(directory,*args,**options)
    cp.git=limited;snapshot.checkpoint=lambda:cp
    manifest=snapshot.pack(workspace,private/'archive.tar.gz',work_bytes=128*1024*1024)
    assert manifest['repositories'][0]['files']==['safe']
    assert (private/'archive.tar.gz').is_file()
    assert not list(private.glob('buzz-snapshot-*'))
    print('PASS: visible output parents refused; excluded in-source job scratch stays outside bounded Git file listing')
'''

command = ['docker', 'run', '--rm', '-i', '--network', 'none', '--read-only',
    '--tmpfs', '/tmp:rw,nosuid,nodev,size=1g,uid=10001,gid=10001,mode=0700',
    '--memory', '1g', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges']
if len(sys.argv) == 3:
    command += ['--mount', f'type=bind,src={sys.argv[2]},dst=/usr/local/bin/buzz-agent-snapshot,readonly']
command += ['--entrypoint', 'python3', sys.argv[1], '-']
result = subprocess.run(command, input=SCRIPT, text=True, capture_output=True, timeout=90)
print(result.stdout, end='')
print(result.stderr, end='', file=sys.stderr)
sys.exit(result.returncode)
