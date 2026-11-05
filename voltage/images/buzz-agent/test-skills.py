"""Check that profile.py links engineering skills into each harness without blocking a session."""
import subprocess
import sys

SCRIPT = r'''
import io, os, sys, tempfile
from contextlib import redirect_stderr
from importlib.machinery import SourceFileLoader
from pathlib import Path

profile = SourceFileLoader('profile', '/usr/local/bin/buzz-agent-profile').load_module()

def require(condition, message):
    if not condition: raise RuntimeError(message)

root = Path(tempfile.mkdtemp())
source = root/'opt'/'skills'
for name in ('commit-policy', 'pr-review'):
    (source/name).mkdir(parents=True)
    (source/name/'SKILL.md').write_text('---\nname: '+name+'\ndescription: fixture\n---\n')
(source/'not-a-skill').mkdir()
claude, codex = root/'claude'/'skills', root/'codex'/'skills'
# The agent wrote its own pr-review skill before this session started.
(claude/'pr-review').mkdir(parents=True)
(claude/'pr-review'/'SKILL.md').write_text('agent-owned\n')

profile.install_skills({}, source=source, targets=(claude, codex))
for directory in (claude, codex):
    link = directory/'commit-policy'
    require(link.is_symlink() and link.resolve() == (source/'commit-policy').resolve(), 'skill not linked')
    require((link/'SKILL.md').is_file(), 'linked skill unreadable')
    require(not os.path.lexists(directory/'not-a-skill'), 'a directory without SKILL.md was linked')
require(not (claude/'pr-review').is_symlink(), 'agent-owned skill replaced')
require((claude/'pr-review'/'SKILL.md').read_text() == 'agent-owned\n', 'agent-owned skill changed')
require((codex/'pr-review').is_symlink(), 'skill missing where the agent had none')

# A rerun, as on every session start, changes nothing.
profile.install_skills({}, source=source, targets=(claude, codex))
require((claude/'commit-policy').is_symlink() and not (claude/'pr-review').is_symlink(), 'rerun changed links')

# A newer image drops commit-policy: its links go, the agent's skill stays.
(source/'commit-policy'/'SKILL.md').unlink()
(source/'commit-policy').rmdir()
profile.install_skills({}, source=source, targets=(claude, codex))
for directory in (claude, codex):
    require(not os.path.lexists(directory/'commit-policy'), 'stale skill link kept')
require((claude/'pr-review'/'SKILL.md').read_text() == 'agent-owned\n', 'agent-owned skill changed')

# A failing directory warns and the next one is still linked: skills are optional.
blocked = root/'blocked'
blocked.write_text('a file, not a directory\n')
after = root/'after'/'skills'
stderr = io.StringIO()
with redirect_stderr(stderr):
    profile.install_skills({}, source=source, targets=(blocked/'skills', after))
require('Engineering skills not installed in skills of blocked' in stderr.getvalue(), 'failure not reported')
require((after/'pr-review').is_symlink(), 'one failing directory stopped the others')

# Default locations: Claude Code, CODEX_HOME, and Buzz's own .agents directory.
require(profile.skill_directories({'CODEX_HOME': '/home/agent/.codex-x'}) == (
    Path('/home/agent/.claude/skills'), Path('/home/agent/.codex-x/skills'),
    Path('/home/agent/.agents/skills')), 'unexpected skill directories')
print('ok')
'''


def main():
    image = sys.argv[1]
    result = subprocess.run(['docker', 'run', '--rm', '--entrypoint', 'python3', image, '-c', SCRIPT],
                            capture_output=True, text=True, timeout=120)
    if result.returncode or result.stdout.strip() != 'ok':
        print(result.stdout[-2000:], result.stderr[-4000:], file=sys.stderr)
        sys.exit(1)
    print('engineering skills: linking checks passed')


if __name__ == '__main__':
    main()
