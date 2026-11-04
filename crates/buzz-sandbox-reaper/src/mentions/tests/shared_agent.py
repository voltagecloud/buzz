"""Bounded ACP test adapter. Real Buzz CLI and harness perform all routing."""
import ctypes
import json
import os
from pathlib import Path
import subprocess
import sys

if sys.platform == "linux":
    ctypes.CDLL(None).prctl(1, 9)  # Do not survive a failed test's harness.


def send(value):
    print(json.dumps(value), flush=True)


for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message.get("method")
    result = {}
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {},
                  "agentInfo": {"name": "shared-fixture", "version": "1"}}
    elif method == "session/new":
        result = {"sessionId": "shared-fixture"}
    elif method == "session/prompt":
        marker = Path("model-turns")
        count = int(marker.read_text()) if marker.exists() else 0
        marker.write_text(str(count + 1))
        if os.environ.get("SHARED_FIXTURE_TARGET") and count == 0:
            subprocess.run([os.environ["SHARED_FIXTURE_CLI"], "agents", "handoff",
                            os.environ["SHARED_FIXTURE_TARGET"], "--from",
                            os.environ["SHARED_FIXTURE_FROM"], "--message",
                            "shared fixture handoff"], check=True, timeout=15,
                           stdout=subprocess.DEVNULL)
        subprocess.run([os.environ["SHARED_FIXTURE_CLI"], "messages", "send",
                        "--channel", os.environ["SHARED_FIXTURE_CHANNEL"],
                        "--reply-to", os.environ["SHARED_FIXTURE_FROM"],
                        "--content", "shared fixture completed"], check=True,
                       timeout=15, stdout=subprocess.DEVNULL)
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "shared-fixture", "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "shared fixture completed"}}}})
        result = {"stopReason": "end_turn"}
    send({"jsonrpc": "2.0", "id": message["id"], "result": result})
