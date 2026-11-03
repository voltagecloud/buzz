"""Exercise the packaged review command with real Git and bounded fake CLIs."""
import subprocess
import sys

SCRIPT = r'''
import json, os, pathlib, signal, subprocess, sys, tempfile, time
from pathlib import Path

root = Path(tempfile.mkdtemp(prefix='review-test-'))
repo, binaries, home = root/'source', root/'bin', root/'home'
repo.mkdir(); binaries.mkdir(); home.mkdir()
codex_home = home/'codex'; codex_home.mkdir()
(codex_home/'auth.json').write_text('{"synthetic":"PRIVATE-TEST-CREDENTIAL"}')
env = {**os.environ, 'HOME':str(home), 'CODEX_HOME':str(codex_home),
       'PATH':str(binaries)+':'+os.environ['PATH'], 'SOURCE_REPO':str(repo),
       'GIT_CONFIG_COUNT':'3', 'GIT_CONFIG_KEY_0':'commit.gpgsign', 'GIT_CONFIG_VALUE_0':'false',
       'GIT_CONFIG_KEY_1':'user.name', 'GIT_CONFIG_VALUE_1':'Review Fixture',
       'GIT_CONFIG_KEY_2':'user.email', 'GIT_CONFIG_VALUE_2':'review@example.invalid'}

def git(*args):
    return subprocess.check_output(['git','-C',str(repo),*args],env=env,text=True).strip()

git('init','-q','-b','main')
(repo/'AGENTS.md').write_text('BASE-GUIDANCE-MARKER\n')
(repo/'math.py').write_text('def add(a,b): return a-b\n')
git('add','.'); git('commit','-qm','seed')
base=git('rev-parse','HEAD')
(repo/'AGENTS.md').write_text('Changed guidance must not replace the reviewed base contract.\n')
(repo/'math.py').write_text('def add(a,b): return a+b\n')
git('commit','-qam','fix: add correctly')
head=git('rev-parse','HEAD')

fake = r"""#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys,time
from pathlib import Path
cwd=Path.cwd(); source=Path(os.environ['SOURCE_REPO'])
assert cwd!=source
prompt=sys.stdin.read()
assert os.environ.get('GUIDANCE_MARKER','BASE-GUIDANCE-MARKER') in (cwd.parent/'base-guidance/AGENTS.md').read_text()
assert (cwd.parent/'change.diff').is_file()
assert not subprocess.check_output(['git','remote'],text=True).strip()
mode=os.environ.get('MODE','pass')
codex=Path(sys.argv[0]).name=='codex'
if codex:
    assert sys.argv[sys.argv.index('--sandbox')+1]=='read-only'
    assert '--ephemeral' in sys.argv and '--output-schema' in sys.argv
    config=(Path(os.environ['CODEX_HOME'])/'config.toml').read_text()
    assert 'shell_tool = false' in config and 'unified_exec = false' in config
    assert '[mcp_servers.review_files]' in config and '--serve-files' in config
    auth=Path(os.environ['CODEX_HOME'])/'auth.json'
    assert auth.stat().st_mode & 0o777==0o600
else:
    assert '--restricted' in sys.argv
    assert sys.argv[sys.argv.index('--tools')+1]=='Read,Grep,Glob'
    assert '--strict-mcp-config' in sys.argv and '--no-session-persistence' in sys.argv
    assert sys.argv[sys.argv.index('--permission-mode')+1]=='plan'
    assert '--add-dir' in sys.argv
if mode=='fail':
    print('PRIVATE-TEST-CREDENTIAL',file=sys.stderr);sys.exit(1)
if mode=='flood':
    for _ in range(1024):os.write(1,b'x'*8192)
    sys.exit(0)
if mode in ('slow','background'):
    child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'],
                           start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    Path(os.environ['CHILD_PID']).write_text(str(child.pid))
    if mode=='slow':time.sleep(60)
if mode=='snapshot-dirty':(cwd/'math.py').write_text('changed by reviewer')
if mode=='source-dirty':(source/'math.py').write_text('source changed during review')
if mode=='source-head':
    subprocess.run(['git','-C',str(source),'commit','--allow-empty','-qm','changed meanwhile'],check=True)
result={'verdict':'pass','findings':[],'checks_not_run':['Runtime checks are separate from this review.']}
if mode=='blocking':
    result['verdict']='changes_requested'
    result['findings']=[{'severity':'blocking','file':'math.py','line':1,'problem':'Concrete fixture defect','fix':'Fix it'}]
if mode=='contradictory':
    result['findings']=[{'severity':'blocking','file':'math.py','line':1,'problem':'Defect','fix':'Fix'}]
if mode=='incomplete':result['verdict']='incomplete'
if mode=='invalid':result={'findings':[]}
if codex:
    if mode=='no-answer':print(json.dumps({'type':'thread.started'}))
    else:print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':json.dumps(result)}}))
else:print(json.dumps({'is_error':False,'structured_output':result}))
"""
for name in ['codex','claude']:
    path=binaries/name;path.write_text(fake);path.chmod(0o755)

def command(runtime='codex',timeout=20):
    return ['/usr/local/bin/buzz-subagent','review','--repo',str(repo),'--base',base,
            '--role','security','--runtime',runtime,'--timeout',str(timeout)]

def invoke(mode='pass',runtime='codex',timeout=20):
    result=subprocess.run(command(runtime,timeout),env={**env,'MODE':mode,'CHILD_PID':str(root/'child.pid')},
                          capture_output=True,text=True,timeout=timeout+15)
    assert 'PRIVATE-TEST-CREDENTIAL' not in result.stdout+result.stderr
    return result.returncode,json.loads(result.stdout)

def stopped(pid):
    p=Path('/proc')/str(pid)/'stat'
    return not p.exists() or p.read_text().rsplit(')',1)[1].split()[0] in ('Z','X')

evidence=root/'files';evidence.mkdir()
(evidence/'allowed.txt').write_text('safe review content')
(evidence/'outside').symlink_to(codex_home/'auth.json')
(evidence/'large.txt').write_text('x'*70000)
(evidence/'.git').mkdir();(evidence/'.git'/'config').write_text('private metadata')
requests=[{'jsonrpc':'2.0','id':0,'method':'initialize','params':{'protocolVersion':'2024-11-05'}},
          {'jsonrpc':'2.0','id':1,'method':'tools/list'},
          *[{'jsonrpc':'2.0','id':i+2,'method':'tools/call','params':{'name':'read_file','arguments':{'path':p}}}
            for i,p in enumerate(['allowed.txt','outside','../home/codex/auth.json','.git/config','large.txt'])]]
response=subprocess.run(['/usr/local/bin/buzz-agent-review','--serve-files',str(evidence)],
                        input=''.join(json.dumps(r)+'\n' for r in requests),capture_output=True,text=True,timeout=10)
assert response.returncode==0,response.stderr
answers=[json.loads(line)['result'] for line in response.stdout.splitlines()]
assert {tool['name'] for tool in answers[1]['tools']}=={'read_file','list_files'}
assert 'safe review content' in answers[2]['content'][0]['text']
assert all(answer.get('isError') for answer in answers[3:6])
large=json.loads(answers[6]['content'][0]['text']);assert len(large['text'])==65536 and large['next_offset']==65536
assert 'PRIVATE-TEST-CREDENTIAL' not in response.stdout
print('PASS: real MCP file boundary rejects traversal, external symlinks, and Git metadata; file reads are bounded')

for runtime in ['codex','claude']:
    code,value=invoke(runtime=runtime)
    assert code==0 and value['ok'] and value['head']==head and value['base']==base,value
    assert value['role']=='security' and value['verdict']=='pass'
assert git('rev-parse','HEAD')==head and not git('status','--porcelain')
print('PASS: both packaged runtimes review an unpublished commit with base guidance and isolated inputs')

git('checkout','-qb','review-target',base)
(repo/'AGENTS.md').write_text('TARGET-GUIDANCE-MARKER\n')
git('commit','-qam','Advance target guidance')
target=git('rev-parse','HEAD');git('checkout','-q','main')
args=command();args[args.index('--base')+1]=target
result=subprocess.run(args,env={**env,'GUIDANCE_MARKER':'TARGET-GUIDANCE-MARKER'},capture_output=True,text=True,timeout=30)
assert result.returncode==0 and json.loads(result.stdout)['base']==target,result.stdout
assert git('rev-parse','HEAD')==head and not git('status','--porcelain')
print('PASS: diverged target-branch guidance remains normative instead of stale merge-base guidance')

code,value=invoke('blocking');assert code==1 and value['verdict']=='changes_requested'
code,value=invoke('incomplete');assert code==3 and not value['ok']
for mode in ['invalid','contradictory','no-answer','fail','snapshot-dirty']:
    code,value=invoke(mode);assert code==2 and not value['ok'],(mode,value)
print('PASS: findings, incomplete reviews, invalid output, provider failures, and reviewer edits remain distinct')

for mode in ['source-dirty','source-head']:
    code,value=invoke(mode);assert code==2 and 'implementation changed' in value['error'],value
    git('reset','--hard',head)
(repo/'dirty.txt').write_text('pending work')
code,value=invoke();assert code==2 and 'commit the local work' in value['error']
(repo/'dirty.txt').unlink()
code,value=invoke('flood');assert code==2 and 'output exceeds' in value['error'],value
print('PASS: source-generation fences, dirty inputs, and bounded output bind the production command')

for mode in ['background','slow']:
    pidfile=root/'child.pid';pidfile.unlink(missing_ok=True)
    code,value=invoke(mode,timeout=1 if mode=='slow' else 20)
    assert code==(2 if mode=='slow' else 0),value
    assert stopped(int(pidfile.read_text())), 'detached reviewer descendant escaped'
print('PASS: successful and timed-out reviewers leave no detached descendants')

pidfile=root/'child.pid';pidfile.unlink(missing_ok=True)
child=subprocess.Popen(command(timeout=30),env={**env,'MODE':'slow','CHILD_PID':str(pidfile)},
                       stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
deadline=time.monotonic()+5
while not pidfile.exists() and time.monotonic()<deadline:time.sleep(0.02)
assert pidfile.exists(),'review runtime never started'
code,value=invoke();assert code==2 and 'another review is active' in value['error'],value
child.terminate();out,err=child.communicate(timeout=10)
assert child.returncode==2 and not json.loads(out)['ok']
assert stopped(int(pidfile.read_text())), 'cancelled reviewer descendant escaped'
code,value=invoke();assert code==0,value
print('PASS: concurrency, cancellation cleanup, and lock release')
'''

if __name__ == '__main__':
    result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                             '--entrypoint', 'python3', sys.argv[1] if len(sys.argv) > 1 else 'buzz-agent:local', '-'],
                            input=SCRIPT, text=True, timeout=180)
    raise SystemExit(result.returncode)
