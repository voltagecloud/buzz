"""Exercise the node-local heavy-job gate and its cargo/go wrappers inside the image."""
import subprocess
import sys

SCRIPT = r'''
import os, signal, socket, subprocess, sys, tempfile, time
from pathlib import Path

def start_gate(port, slots):
    gate = subprocess.Popen(['buzz-agent-gate'], env={**os.environ, 'BUZZ_HEAVY_PORT': str(port),
        'BUZZ_HEAVY_SLOTS': str(slots)}, stdout=subprocess.DEVNULL)
    for _ in range(100):
        try:
            socket.create_connection(('127.0.0.1', port), timeout=1).close()
            return gate
        except OSError:
            time.sleep(0.05)
    raise SystemExit('gate did not start')

def heavy(port, *command, **kwargs):
    env = {**os.environ, 'BUZZ_HEAVY_GATE': f'127.0.0.1:{port}'}
    env.pop('BUZZ_HEAVY_HELD', None)
    return subprocess.Popen(['buzz-heavy', '--', *command], env=env, stderr=subprocess.PIPE, **kwargs)

def wait_for(path):
    for _ in range(200):
        if path.exists():
            return
        time.sleep(0.05)
    raise SystemExit(f'{path} never appeared')

with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)

    # At most two jobs run at once; every job finishes.
    gate = start_gate(17070, 2)
    (root/'running').mkdir()
    job = f"d=$(mktemp -d {root}/running/j.XXXX); ls {root}/running | wc -l >> {root}/peaks; sleep 0.5; rmdir $d"
    jobs = [heavy(17070, 'sh', '-c', job) for _ in range(5)]
    assert all(j.wait(timeout=30) == 0 for j in jobs)
    peaks = [int(line) for line in (root/'peaks').read_text().split()]
    assert len(peaks) == 5 and max(peaks) <= 2, peaks
    gate.terminate(); gate.wait()

    # A holder killed without cleanup releases its slot; a waiter that gives
    # up leaves the queue instead of blocking the next job.
    gate = start_gate(17071, 1)
    holder = heavy(17071, 'sh', '-c', f'touch {root}/held; sleep 60', start_new_session=True)
    wait_for(root/'held')
    waiter = heavy(17071, 'touch', str(root/'never'))
    time.sleep(0.5)
    waiter.kill(); waiter.wait()
    os.killpg(holder.pid, signal.SIGKILL); holder.wait()
    started = time.monotonic()
    assert heavy(17071, 'true').wait(timeout=10) == 0
    assert time.monotonic() - started < 5
    assert not (root/'never').exists()

    # A nested call inside a held job reuses its slot rather than deadlocking.
    nested = heavy(17071, 'sh', '-c', 'buzz-heavy -- sh -c "exit 3"')
    assert nested.wait(timeout=10) == 3
    gate.terminate(); gate.wait()

    # Without a gate, the job still runs, warns, and keeps its exit status.
    alone = heavy(1, 'sh', '-c', 'exit 7')
    _, warning = alone.communicate(timeout=10)
    assert alone.returncode == 7 and b'running without admission' in warning, warning

    # Only compiling cargo and go commands are gated.
    assert subprocess.run(['sh', '-c', 'command -v cargo'], capture_output=True, text=True).stdout.strip() == '/opt/buzz-heavy/bin/cargo'
    env = {**os.environ, 'BUZZ_HEAVY_GATE': '127.0.0.1:1'}
    for command, gated in [(['cargo', '--version'], False), (['cargo', '+stable', 'build'], True),
                           (['cargo', 'test'], True), (['cargo', 'fmt', '--check'], False),
                           (['go', 'version'], False), (['go', 'build', './...'], True)]:
        result = subprocess.run(command, env=env, cwd=temp, capture_output=True)
        assert (b'running without admission' in result.stderr) == gated, (command, result.stderr[-300:])
print('ok')
'''


def main():
    image = sys.argv[1]
    result = subprocess.run(['docker', 'run', '--rm', '--entrypoint', 'python3', image, '-c', SCRIPT],
                            capture_output=True, text=True, timeout=300)
    if result.returncode or result.stdout.strip() != 'ok':
        print(result.stdout, result.stderr, file=sys.stderr)
        raise SystemExit('FAIL: heavy-job gate')
    print('PASS: heavy jobs are limited per node, released on crash, and fail open without a gate')


if __name__ == '__main__':
    main()
