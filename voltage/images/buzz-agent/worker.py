#!/usr/bin/env python3
"""Run one headless sub-agent task in a short-lived worker Sandbox.

The orchestrating agent claims this Sandbox with `buzz-subagent spawn`, which
sets BUZZ_WORKER_TASK, BUZZ_WORKER_FOCUS, BUZZ_WORKER_REPO, BUZZ_WORKER_BASE,
and BUZZ_WORKER_RUNTIME. After the developer-profile bootstrap configures Git,
GitHub, and model accounts, this runner clones the repository, works on
`worker/<task>` from the base branch with Claude Code or Codex, pushes that
branch, writes a result to
`s3://$BUZZ_CHECKPOINT_BUCKET/$BUZZ_WORKER_RESULT_PREFIX<task>/result.json`,
and exits with the runtime's status.
"""
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

WORKSPACE = Path('/home/agent/work')
MAX_FOCUS = 8000
MAX_SUMMARY = 4000
TASK = re.compile(r'[a-z0-9][a-z0-9-]{0,39}')
REPO = re.compile(r'https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+?(\.git)?')
BRANCH = re.compile(r'[A-Za-z0-9._/-]{1,200}')
PROMPT = '''You are a focused sub-agent working for an orchestrating agent.

Focus:
{focus}

The repository {repo} is checked out at {path} on branch `{branch}`, created from `{base}`.
Make the change the focus describes, run the relevant checks, and commit your work with clear messages.
Do not push and do not switch branches; the runner pushes `{branch}` when you finish.
End with a short summary: what changed, what you verified, and anything left undone.
'''


class Refused(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def activity(sequence, event, details):
    """One bounded activity record for the platform collector (no prompt or output)."""
    record = {'schema': 'buzz.activity.v1', 'source_id': 'worker', 'event_id': str(uuid.uuid4()),
              'sequence': sequence, 'timestamp': time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
              'event': event, 'details': details}
    print(json.dumps({'fields': {'schema': 'buzz.activity.v1', 'record': json.dumps(record)}}),
          file=sys.stderr, flush=True)


def git(*args, cwd=None, check=True):
    return subprocess.run(['git', *args], cwd=cwd, check=check, capture_output=True, text=True, timeout=600)


def task_inputs():
    env = os.environ
    task = env.get('BUZZ_WORKER_TASK', '')
    focus = env.get('BUZZ_WORKER_FOCUS', '')
    repo = env.get('BUZZ_WORKER_REPO', '')
    base = env.get('BUZZ_WORKER_BASE', '')
    runtime = env.get('BUZZ_WORKER_RUNTIME', 'claude')
    require(TASK.fullmatch(task), 'BUZZ_WORKER_TASK must be 1-40 lowercase letters, digits, or hyphens')
    require(0 < len(focus) <= MAX_FOCUS, f'BUZZ_WORKER_FOCUS must be 1-{MAX_FOCUS} characters')
    require(REPO.fullmatch(repo), 'BUZZ_WORKER_REPO must be an https://github.com/<owner>/<repo> URL')
    require(BRANCH.fullmatch(base) and '..' not in base and not base.startswith('-'), 'Invalid BUZZ_WORKER_BASE')
    require(runtime in ('claude', 'codex'), 'BUZZ_WORKER_RUNTIME must be claude or codex')
    return task, focus, repo, base, runtime


def runtime_command(runtime, prompt):
    # The worker Sandbox is the isolation boundary, as for interactive agents.
    if runtime == 'claude':
        return ['claude', '-p', prompt, '--dangerously-skip-permissions']
    return ['codex', 'exec', '--dangerously-bypass-approvals-and-sandbox', prompt]


def run_runtime(command, path, timeout):
    """Bound captured output and stop the runtime's process group before saving work."""
    tail = bytearray()

    def read_output(stream):
        with stream:
            while chunk := stream.read(8192):
                tail.extend(chunk)
                del tail[:-MAX_SUMMARY * 4]

    class Interrupted(Exception):
        def __init__(self, number):
            self.number = number

    def interrupt(number, _frame):
        raise Interrupted(number)

    child = subprocess.Popen(command, cwd=path, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL,
                             start_new_session=True)
    reader = threading.Thread(target=read_output, args=(child.stdout,), daemon=True)
    reader.start()
    handlers = {number: signal.signal(number, interrupt)
                for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        try:
            code = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            code = 124
        except Interrupted as interrupted:
            code = 128 + interrupted.number
    finally:
        # Normal CLI exit can also leave background tools holding stdout or
        # changing files. Stop them before inspecting or committing the checkout.
        for number in handlers:
            signal.signal(number, signal.SIG_IGN)
        try:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=5)
            reader.join(timeout=5)
            require(not reader.is_alive(), 'Runtime output did not close after process cleanup')
        finally:
            for number, handler in handlers.items():
                signal.signal(number, handler)
    return (128 - code if code < 0 else code), tail.decode(errors='replace')[-MAX_SUMMARY:]


def publish(result):
    bucket = os.environ['BUZZ_CHECKPOINT_BUCKET']
    key = os.environ['BUZZ_WORKER_RESULT_PREFIX'] + result['task'] + '/result.json'
    with tempfile.NamedTemporaryFile('w', suffix='.json', delete=False) as handle:
        json.dump(result, handle)
    try:
        subprocess.run(['aws', 's3api', 'put-object', '--bucket', bucket, '--key', key,
                        '--body', handle.name, '--content-type', 'application/json'],
                       check=True, capture_output=True, timeout=120)
    finally:
        os.unlink(handle.name)
    return key


def main():
    started = time.monotonic()
    task, focus, repo, base, runtime = task_inputs()
    try:
        timeout = int(os.environ.get('BUZZ_WORKER_TIMEOUT', '3600'))
    except ValueError:
        raise Refused('BUZZ_WORKER_TIMEOUT must be an integer from 1 through 7200') from None
    require(1 <= timeout <= 7200, 'BUZZ_WORKER_TIMEOUT must be an integer from 1 through 7200')
    activity(1, 'worker_started', {'runtime': runtime})
    name = repo.rstrip('/').rsplit('/', 1)[1].removesuffix('.git')
    path = WORKSPACE / task / name
    branch = f'worker/{task}'
    path.parent.mkdir(parents=True, exist_ok=True)
    git('clone', '--branch', base, '--single-branch', repo, str(path))
    git('switch', '-c', branch, cwd=path)
    base_head = git('rev-parse', 'HEAD', cwd=path).stdout.strip()
    prompt = PROMPT.format(focus=focus, repo=repo, path=path, branch=branch, base=base)
    exit_code, output = run_runtime(runtime_command(runtime, prompt), path, timeout)
    # Keep edits the runtime left uncommitted rather than losing them.
    if git('status', '--porcelain', cwd=path).stdout.strip():
        git('add', '-A', cwd=path)
        git('commit', '-m', f'wip(worker): uncommitted changes from {task}', cwd=path)
    head = git('rev-parse', 'HEAD', cwd=path).stdout.strip()
    commits = int(git('rev-list', '--count', f'{base_head}..{head}', cwd=path).stdout.strip())
    pushed = False
    if commits:
        pushed = git('push', '--set-upstream', 'origin', branch, cwd=path, check=False).returncode == 0
    result = {'version': 1, 'task': task, 'runtime': runtime, 'repo': repo, 'base': base,
              'branch': branch, 'head': head, 'commits': commits, 'pushed': pushed,
              'exit_code': exit_code, 'duration_ms': int((time.monotonic() - started) * 1000),
              'summary': output.strip()[-MAX_SUMMARY:]}
    key = publish(result)
    activity(2, 'worker_completed', {'runtime': runtime, 'exit_code': exit_code, 'commits': commits,
                                     'pushed': pushed, 'duration_ms': result['duration_ms']})
    print(json.dumps({'version': 1, 'task': task, 'result': key, 'exit_code': exit_code}))
    if commits and not pushed:
        return 1
    return exit_code


if __name__ == '__main__':
    try:
        sys.exit(main())
    except Refused as error:
        print(f'buzz-agent-worker: {error}', file=sys.stderr)
        sys.exit(2)
    except subprocess.CalledProcessError as error:
        print(f'buzz-agent-worker: {error.cmd[0]} {error.cmd[1] if len(error.cmd) > 1 else ""} failed', file=sys.stderr)
        sys.exit(1)
