"""Check that every session receives the environment guide, with or without cluster credentials."""
import subprocess
import sys

SCRIPT = r'''
import os, runpy, sys, tempfile
from pathlib import Path

profile = runpy.run_path('/usr/local/bin/buzz-agent-profile', run_name='probe')
install = profile['install_environment_guide']
settings = install.__globals__
home = Path(tempfile.mkdtemp(prefix='environment-test-', dir='/home/agent'))
claude = Path('/home/agent/.claude/CLAUDE.md')

def require(condition, message):
    if not condition: raise RuntimeError(message)

guide = settings['ENVIRONMENT_GUIDE'].read_text()
require('via capabilities' in guide and 'KUBERNETES_SERVICE_HOST' in guide, 'guide lost its rules')
settings['CLUSTER_CREDENTIALS'] = home / 'no-worker'
settings['SUBAGENT_CREDENTIALS'] = home / 'no-primary'
claude.parent.mkdir(parents=True, exist_ok=True)
claude.write_text('existing instructions\n')
env = {**os.environ, 'CODEX_HOME': str(home / 'codex')}
install(env)
install(env)
require(claude.read_text().startswith('existing instructions\n' + guide), 'Claude Code instructions were replaced or lack the guide')
require(claude.read_text().count('## Working in the Sandbox') == 2, 'each install call should append the guide once')
require((home / 'codex/AGENTS.md').read_text() == guide * 2, 'Codex instructions lack the guide')

# Bind the production seam: each startup path must install the guide before it launches its child.
main = settings['main']
for action in ('run', 'work'):
    claude.write_text('')
    events = []
    class Launched(Exception): pass
    def launch(path, argv, env): events.append('launch'); raise Launched()
    settings.update(
        step=lambda message: None, bootstrap=lambda: env, restore_checkpoint=lambda: None,
        install_cluster_access=lambda env: None, install_subagent_guide=lambda env: None,
        install_skills=lambda env: None,
        install_environment_guide=lambda env, real=install: (events.append('guide'), real(env)))
    settings['os'].execvpe = launch
    sys.argv = ['profile', action]
    try: main()
    except Launched: pass
    require(events == ['guide', 'launch'], action + ' did not install the guide before launching: ' + repr(events))
    require(claude.read_text() == guide, action + ' did not write the guide')
print('PASS: the guide reaches Claude Code and Codex, and both startup paths install it')
'''

if __name__ == '__main__':
    result = subprocess.run(['docker', 'run', '--rm', '-i', '--network', 'none',
                             '--entrypoint', 'python3', sys.argv[1] if len(sys.argv) > 1 else 'buzz-agent:local', '-'],
                            input=SCRIPT, text=True, timeout=90)
    raise SystemExit(result.returncode)
