#!/bin/sh
# A stand-in for Claude for the end-to-end test: answers --version and
# `auth status`, and speaks enough stream-json for Buzz. Each user message
# is answered with a final `send` through the in-process Buzz tools; a
# message holding READ:<path> sends that file's text instead.
case "$1" in
--version)
  echo "2.1.130 (Claude Code)"
  exit 0
  ;;
auth)
  echo '{"loggedIn":true,"email":"e2e@example.com"}'
  exit 0
  ;;
esac

session=fresh
replay=
while [ $# -gt 0 ]; do
  case "$1" in
  --session-id | --resume) session="$2"; shift ;;
  --replay-user-messages) replay=1 ;;
  esac
  shift
done

calls=0
started=
while IFS= read -r line; do
  case "$line" in
  *'"type":"control_request"'*)
    id=$(printf '%s' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
    printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s"}}\n' "$id"
    ;;
  *'"type":"user"'*)
    if [ -z "$started" ]; then
      started=1
      printf '{"type":"system","subtype":"init","session_id":"%s","cwd":"%s"}\n' "$session" "$(pwd)"
    fi
    if [ -n "$replay" ]; then
      printf '{"type":"user","isReplay":true,"session_id":"%s","message":{"role":"user","content":""}}\n' "$session"
    fi
    calls=$((calls + 1))
    path=$(printf '%s' "$line" | sed -n 's/.*READ:\([^ \\"]*\).*/\1/p')
    if [ -n "$path" ]; then
      arguments=$(printf '{"path":"%s","audience":"everyone","final":true}' "$path")
    else
      arguments='{"text":"pong","audience":"everyone","final":true}'
    fi
    printf '{"type":"control_request","request_id":"tool-%s","request":{"subtype":"mcp_message","server_name":"buzz","message":{"jsonrpc":"2.0","id":%s,"method":"tools/call","params":{"name":"send","arguments":%s}}}}\n' "$calls" "$calls" "$arguments"
    # The tool's answer comes back before the turn ends.
    while IFS= read -r answer; do
      case "$answer" in
      *"\"request_id\":\"tool-$calls\""*) break ;;
      esac
    done
    printf '{"type":"control_request","request_id":"hook-%s","request":{"subtype":"hook_callback","callback_id":"final","input":{"tool_name":"mcp__buzz__send","tool_input":{"final":true}}}}\n' "$calls"
    printf '{"type":"result","subtype":"success","is_error":false,"session_id":"%s"}\n' "$session"
    printf '{"type":"system","subtype":"session_state_changed","state":"idle","session_id":"%s"}\n' "$session"
    ;;
  esac
done
