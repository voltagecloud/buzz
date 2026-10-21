#!/usr/bin/env python3
"""Delegate focused work to short-lived worker Sandboxes.

    buzz-subagent spawn --focus TEXT --repo URL --base BRANCH [--runtime claude|codex] [--task ID] [--minutes N]
    buzz-subagent list
    buzz-subagent status TASK
    buzz-subagent wait TASK [--timeout SECONDS]
    buzz-subagent result TASK
    buzz-subagent cancel TASK

Each worker runs in its own Pod, so a worker that is busy or fails does not
stop the others. `spawn` creates a SandboxClaim against the developer's
`agent-worker` pool with the sub-agent token mounted at
/var/run/secrets/buzz-subagents. Workers push `worker/<task>` and write
`result.json`, which `result` reads from the checkpoint bucket.
"""
import argparse
import datetime
import json
import os
import re
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

CREDENTIALS = Path(os.environ.get('BUZZ_SUBAGENT_CREDENTIALS', '/var/run/secrets/buzz-subagents'))
GROUP = 'apis/extensions.agents.x-k8s.io/v1beta1'
POOL = 'agent-worker'
TASK_LABEL = 'voltage.cloud/worker-task'
TASK = re.compile(r'[a-z0-9][a-z0-9-]{0,39}')
MAX_MINUTES = 120


class Refused(Exception):
    pass


def api(method, path, body=None):
    token = (CREDENTIALS / 'token').read_text().strip()
    server = os.environ.get('BUZZ_SUBAGENT_API') or 'https://{}:{}'.format(
        os.environ['KUBERNETES_SERVICE_HOST'], os.environ.get('KUBERNETES_SERVICE_PORT', '443'))
    # The API server is verified against the cluster CA; plain HTTP is only for tests.
    context = ssl.create_default_context(cafile=str(CREDENTIALS / 'ca.crt')) if server.startswith('https://') else None
    request = urllib.request.Request(server + '/' + path, method=method,
                                     data=None if body is None else json.dumps(body).encode(),
                                     headers={'Authorization': 'Bearer ' + token,
                                              'Content-Type': 'application/json', 'Accept': 'application/json'})
    try:
        with urllib.request.urlopen(request, context=context, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        detail = json.loads(error.read() or b'{}').get('message', error.reason)
        raise Refused(f'{method} {path.split("?")[0]}: {error.code} {detail}') from None


def namespace():
    return (CREDENTIALS / 'namespace').read_text().strip()


def claims_path(name=''):
    return f'{GROUP}/namespaces/{namespace()}/sandboxclaims' + (f'/{name}' if name else '')


def claim_name(task):
    if not TASK.fullmatch(task):
        raise Refused('task must be 1-40 lowercase letters, digits, or hyphens')
    return f'worker-{task}'


def summarize(claim):
    status = claim.get('status', {})
    ready = next((c for c in status.get('conditions', []) if c.get('type') == 'Ready'), {})
    return {'task': claim['metadata'].get('labels', {}).get(TASK_LABEL),
            'claim': claim['metadata']['name'],
            'sandbox': status.get('sandbox', {}).get('name'),
            'ready': ready.get('status'), 'reason': ready.get('reason'),
            'shutdown_time': claim['spec'].get('lifecycle', {}).get('shutdownTime')}


def spawn(args):
    task = args.task or uuid.uuid4().hex[:12]
    if args.minutes < 3:
        raise Refused('worker lifetime must be at least 3 minutes, including 2 minutes to publish results')
    minutes = min(args.minutes, MAX_MINUTES)
    shutdown = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=minutes))
    annotations = {}
    if os.environ.get('BUZZ_MANAGED_AGENT_START_NONCE'):
        # Groups worker activity under the orchestrator's session in Grafana.
        annotations['buzz.block.xyz/session-id'] = os.environ['BUZZ_MANAGED_AGENT_START_NONCE']
    env = {'BUZZ_WORKER_TASK': task, 'BUZZ_WORKER_FOCUS': args.focus, 'BUZZ_WORKER_REPO': args.repo,
           'BUZZ_WORKER_BASE': args.base, 'BUZZ_WORKER_RUNTIME': args.runtime,
           'BUZZ_WORKER_TIMEOUT': str(minutes * 60 - 120)}
    claim = {'apiVersion': 'extensions.agents.x-k8s.io/v1beta1', 'kind': 'SandboxClaim',
             'metadata': {'name': claim_name(task), 'labels': {TASK_LABEL: task}},
             'spec': {'warmPoolRef': {'name': POOL},
                      'env': [{'name': k, 'value': v} for k, v in env.items()],
                      'lifecycle': {'shutdownTime': shutdown.strftime('%Y-%m-%dT%H:%M:%SZ'),
                                    'ttlSecondsAfterFinished': 3600},
                      'additionalPodMetadata': {'annotations': annotations}}}
    return summarize(api('POST', claims_path(), claim))


def result_key(task):
    developer = os.environ['BUZZ_DEVELOPER_ID']
    return f'{developer}/workers/{task}/result.json'


def result(task):
    claim_name(task)
    with tempfile.TemporaryDirectory(prefix='worker-result-') as directory:
        path = Path(directory) / 'result.json'
        output = subprocess.run(['aws', 's3api', 'get-object', '--bucket', os.environ['BUZZ_CHECKPOINT_BUCKET'],
                                 '--key', result_key(task), '--range', 'bytes=0-65535', str(path)],
                                capture_output=True, text=True, timeout=60)
        if output.returncode:
            if '(NoSuchKey)' in output.stderr or '(404)' in output.stderr:
                return None
            raise Refused('unable to read worker result; check AWS credentials, bucket permissions, and connectivity')
        try:
            return json.loads(path.read_bytes())
        except (ValueError, UnicodeError):
            raise Refused('worker result is invalid or exceeds 64 KiB') from None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    actions = parser.add_subparsers(dest='action', required=True)
    spawn_parser = actions.add_parser('spawn', help='Start a worker for one focused task')
    spawn_parser.add_argument('--focus', required=True, help='What the worker should do, and how to verify it')
    spawn_parser.add_argument('--repo', required=True, help='https://github.com/<owner>/<repo>')
    spawn_parser.add_argument('--base', required=True, help='Branch to start from')
    spawn_parser.add_argument('--runtime', choices=['claude', 'codex'], default='claude')
    spawn_parser.add_argument('--task', help='Task ID (default: random)')
    spawn_parser.add_argument('--minutes', type=int, default=60, help=f'Lifetime, at most {MAX_MINUTES}')
    actions.add_parser('list', help='List this session\'s workers')
    for name in ('status', 'result', 'cancel'):
        actions.add_parser(name).add_argument('task')
    wait_parser = actions.add_parser('wait', help='Wait for a worker result')
    wait_parser.add_argument('task')
    wait_parser.add_argument('--timeout', type=int, default=3600)
    args = parser.parse_args(argv)

    if args.action == 'spawn':
        print(json.dumps(spawn(args)))
    elif args.action == 'list':
        claims = api('GET', claims_path() + f'?labelSelector={TASK_LABEL}')['items']
        print(json.dumps([summarize(c) for c in claims]))
    elif args.action == 'status':
        print(json.dumps(summarize(api('GET', claims_path(claim_name(args.task))))))
    elif args.action == 'cancel':
        api('DELETE', claims_path(claim_name(args.task)))
        print(json.dumps({'task': args.task, 'cancelled': True}))
    elif args.action == 'result':
        found = result(args.task)
        if found is None:
            raise Refused('no result yet; the worker is still running or failed before writing one')
        print(json.dumps(found))
    else:
        deadline = time.monotonic() + args.timeout
        while (found := result(args.task)) is None:
            if time.monotonic() > deadline:
                raise Refused('timed out waiting for the worker result')
            time.sleep(15)
        print(json.dumps(found))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (Refused, FileNotFoundError, KeyError) as error:
        print(f'buzz-subagent: {error}', file=sys.stderr)
        sys.exit(1)
