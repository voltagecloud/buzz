"""Synthetic ACP -> native buzz-agent -> MCP shell smoke, no real credentials."""
import http.server
import json
import queue
import subprocess
import sys
import threading
import time
import uuid

IMAGE = sys.argv[1] if len(sys.argv) > 1 else 'buzz-agent:local'
NAME = 'dev2900-smoke-' + uuid.uuid4().hex[:12]
seen = []


def require(condition, message):
    if not condition:
        raise RuntimeError(message)

class Model(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if not seen:
            name = next(t['function']['name'] for t in request['tools']
                        if t['function']['name'].endswith('__shell'))
            message = {'role': 'assistant', 'content': None, 'tool_calls': [
                {'id': 'probe', 'type': 'function', 'function': {'name': name,
                 'arguments': json.dumps({'command': 'test "$(id -u)" = 10001 && test "$(pwd)" = /home/agent && printf IMAGE_TOOL_OK'})}}]}
            reason = 'tool_calls'
        else:
            require('IMAGE_TOOL_OK' in json.dumps(request['messages']), 'MCP result absent')
            message = {'role': 'assistant', 'content': 'IMAGE_AGENT_OK'}
            reason = 'stop'
        seen.append(request)
        body = json.dumps({'id': 'probe', 'object': 'chat.completion', 'model': 'probe',
                           'choices': [{'index': 0, 'message': message, 'finish_reason': reason}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
threading.Thread(target=server.serve_forever, daemon=True).start()
command = ['docker', 'run', '--rm', '-i', '--name', NAME, '--network', 'host',
           '--entrypoint', '/usr/local/bin/buzz-agent',
           '-e', 'BUZZ_AGENT_PROVIDER=openai', '-e', 'OPENAI_COMPAT_API=chat',
           '-e', 'OPENAI_COMPAT_API_KEY=synthetic-not-a-credential',
           '-e', 'OPENAI_COMPAT_MODEL=probe',
           '-e', f'OPENAI_COMPAT_BASE_URL=http://127.0.0.1:{server.server_port}', IMAGE]
process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                           stderr=subprocess.DEVNULL, text=True)
frames = queue.Queue()
def reader():
    for line in process.stdout:
        frames.put(json.loads(line))
    frames.put(None)
threading.Thread(target=reader, daemon=True).start()
updates = []
def send(frame):
    process.stdin.write(json.dumps({'jsonrpc': '2.0', **frame}) + '\n')
    process.stdin.flush()
def rpc(number, method, params):
    send({'id': number, 'method': method, 'params': params})
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        frame = frames.get(timeout=max(0.1, deadline - time.monotonic()))
        require(frame is not None, 'agent exited unexpectedly')
        if frame.get('method') == 'session/request_permission':
            send({'id': frame['id'], 'result': {'outcome': {'outcome': 'selected', 'optionId': 'allow_once'}}})
        elif frame.get('id') == number:
            require('error' not in frame, frame)
            return frame['result']
        else:
            updates.append(frame)
    raise AssertionError('ACP deadline exceeded')
try:
    rpc(1, 'initialize', {'protocolVersion': 2, 'clientCapabilities': {}})
    session = rpc(2, 'session/new', {'cwd': '/home/agent', 'mcpServers': [
        {'name': 'developer', 'command': '/usr/local/bin/buzz-dev-mcp', 'args': [], 'env': []}]})
    rpc(3, 'session/prompt', {'sessionId': session['sessionId'],
                            'prompt': [{'type': 'text', 'text': 'Run the synthetic local shell probe.'}]})
    require(len(seen) == 2, 'expected tool call and final response')
    require('IMAGE_AGENT_OK' in json.dumps(updates), 'agent response absent')
    print('PASS: packaged native ACP agent, synthetic HTTP model, actual MCP shell, UID/CWD, response')
    start = time.monotonic()
    process.stdin.close()
    process.wait(timeout=10)
    require(process.returncode == 0, f'adapter EOF shutdown failed with exit code {process.returncode}')
    print(f'PASS: adapter EOF shutdown completed in {time.monotonic() - start:.2f}s (no relay involved)')
finally:
    subprocess.run(['docker', 'rm', '-f', NAME], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if process.poll() is None:
        process.kill()
        process.wait()
    server.shutdown()
