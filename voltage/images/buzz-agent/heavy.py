#!/usr/bin/env python3
"""Node-local admission for heavy agent jobs such as builds and test suites.

Agent Pods on one worker share its CPU and memory, and Kubernetes limits do not
stop several builds from peaking at once. As `buzz-agent-gate`, this script is
the per-node gate: a job holds one of BUZZ_HEAVY_SLOTS slots for as long as its
TCP connection stays open, so a job that crashes releases its slot when the
kernel closes the socket. Waiters are served in arrival order.

As `buzz-heavy [--] COMMAND ...`, it runs COMMAND while holding a slot. Nested
calls (a build that runs cargo again) reuse the outer slot. If the gate is
unreachable, the command runs without admission and a warning is printed, so a
gate outage never blocks development work.
"""
import asyncio
import collections
import json
import os
import signal
import socket
import subprocess
import sys
import time

DEFAULT_GATE = 'agent-gate.buzz-agent-gate.svc.cluster.local:7070'
HELD = 'BUZZ_HEAVY_HELD'


def log(message, **fields):
    print(json.dumps({'msg': message, **fields}), flush=True)


class Gate:
    def __init__(self, slots):
        self.slots = slots
        self.active = 0
        self.waiting = collections.deque()

    def _grant_next(self):
        while self.waiting and self.active < self.slots:
            waiter = self.waiting.popleft()
            if not waiter.done():
                self.active += 1
                waiter.set_result(None)

    async def handle(self, reader, writer):
        granted = False
        closed = None
        try:
            request = await asyncio.wait_for(reader.readline(), 10)
            if request.strip() != b'ACQUIRE':
                writer.write(b'ERROR expected ACQUIRE\n')
                return
            started = time.monotonic()
            # Completes when the client closes the connection: a waiter that
            # gives up leaves the queue, and a holder releases its slot.
            closed = asyncio.ensure_future(reader.read())
            waiter = asyncio.get_running_loop().create_future()
            self.waiting.append(waiter)
            self._grant_next()
            if not waiter.done():
                writer.write(f'QUEUED {len(self.waiting)}\n'.encode())
                await writer.drain()
                await asyncio.wait({waiter, closed}, return_when=asyncio.FIRST_COMPLETED)
                if not waiter.done():
                    waiter.cancel()
                    return
            granted = True
            writer.write(b'GRANTED\n')
            await writer.drain()
            log('granted', waited_seconds=round(time.monotonic() - started, 1),
                active=self.active, waiting=len(self.waiting))
            await closed
        except (asyncio.TimeoutError, ConnectionError):
            pass
        finally:
            if closed is not None:
                closed.cancel()
            if granted:
                self.active -= 1
                log('released', active=self.active, waiting=len(self.waiting))
                self._grant_next()
            writer.close()


async def serve(port, slots):
    gate = Gate(slots)
    server = await asyncio.start_server(gate.handle, '0.0.0.0', port)
    log('serving', port=port, slots=slots)
    async with server:
        await server.serve_forever()


def acquire(address):
    """Return a connection that holds a slot, or None to run without admission."""
    host, _, port = address.rpartition(':')
    try:
        connection = socket.create_connection((host, int(port)), timeout=3)
        connection.sendall(b'ACQUIRE\n')
        connection.settimeout(None)
        stream = connection.makefile('rb')
        while True:
            line = stream.readline()
            if line.startswith(b'GRANTED'):
                return connection
            if line.startswith(b'QUEUED'):
                print(f'buzz-heavy: waiting for a heavy-job slot on this node '
                      f'(position {line.split()[1].decode()})', file=sys.stderr, flush=True)
                continue
            raise ConnectionError(line.decode(errors='replace').strip() or 'gate closed the connection')
    except (OSError, ValueError) as error:
        print(f'buzz-heavy: job gate unavailable ({error}); running without admission',
              file=sys.stderr, flush=True)
        return None


def run(command):
    address = os.environ.get('BUZZ_HEAVY_GATE', DEFAULT_GATE)
    if os.environ.get(HELD) or address in ('', 'off'):
        os.execvp(command[0], command)
    slot = acquire(address)
    child = subprocess.Popen(command, env={**os.environ, HELD: '1'})
    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, lambda received, _frame: child.send_signal(received))
    code = child.wait()
    if slot is not None:
        slot.close()
    return 128 - code if code < 0 else code


def main(argv):
    if os.path.basename(argv[0]) == 'buzz-agent-gate':
        slots = int(os.environ.get('BUZZ_HEAVY_SLOTS', '2'))
        port = int(os.environ.get('BUZZ_HEAVY_PORT', '7070'))
        if slots < 1:
            raise SystemExit('BUZZ_HEAVY_SLOTS must be at least 1')
        asyncio.run(serve(port, slots))
        return 0
    command = argv[1:]
    if command[:1] == ['--']:
        command = command[1:]
    if not command:
        raise SystemExit('usage: buzz-heavy [--] COMMAND [ARG...]')
    return run(command)


if __name__ == '__main__':
    sys.exit(main(sys.argv))
