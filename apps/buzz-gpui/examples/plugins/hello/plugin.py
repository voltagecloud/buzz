#!/usr/bin/env python3
"""A Buzz plugin in one file, with no dependencies.

Buzz starts this program and talks JSON-RPC 2.0 to it, one message per
line on stdin and stdout. Print logs to stderr: stdout is the protocol.
See docs/plugins.md.

It adds:
- a "Hello" page in the channel sidebar: a counter, notes, and the
  community's channels. Choosing a channel follows it live, and you can
  reply from the page (the manifest declares "send-messages" for that);
- a panel for https://example.com/ links, with a button in the channel
  header;
- a Settings section, a notification category, two message actions (one
  marks messages), a chip for example.com links, a card for messages that
  start with "!hello", a badge for ticket numbers like HELLO-42 in message
  text, a composer button, "/" commands in the composer and a shortcut
  (Ctrl/Cmd+Shift+H);
- an "About Hello" item in the account menu, and a note above composers
  that reply in a thread;
- an agent tool every agent can call, and a "Greeter" kind of agent that
  answers mentions, with its own settings tab.
"""

import json
import sys
import time

views = {}  # view id -> what the app opened it with, plus our state
pending = {}  # our request id -> what to do with the answer
next_request = 0
starred = set()  # message ids the "Star" action marks
# What typing "/" in the composer offers.
COMMANDS = {"shrug": "¯\\_(ツ)_/¯", "tableflip": "(╯°□°)╯︵ ┻━┻", "wave": "👋"}
settings = {"loud": False}  # what the Settings section edits


def send(message):
    message["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()


def notify(method, params):
    send({"method": method, "params": params})


def request(method, params, then=None):
    """Asks Buzz for something; `then(result, error)` gets the answer."""
    global next_request
    next_request += 1
    pending[next_request] = then
    send({"id": next_request, "method": method, "params": params})


def text(value, style="body", tone="default"):
    return {"type": "text", "text": value, "style": style, "tone": tone}


def channel_list(state):
    channels = state.get("channels")
    if channels is None:
        return [{"type": "spinner"}]
    if not channels:
        return [text("No channels yet.", tone="subtle")]
    return [
        {
            "type": "row",
            "children": [
                {
                    "type": "button",
                    "id": f"follow:{c['id']}",
                    "label": c["name"],
                    "icon": "User" if c["kind"] == "dm" else "Hash",
                    "variant": "prominent" if c["id"] == state.get("following") else "ghost",
                }
            ]
            + ([{"type": "badge", "text": "private"}] if c["private"] else []),
        }
        for c in channels[:20]
    ]


def live(state):
    """The followed channel's newest messages, and a reply field."""
    if not state.get("following"):
        return []
    messages = state.get("messages")
    if messages is None:
        body = [{"type": "spinner"}]
    elif not messages:
        body = [text("No messages yet.", tone="subtle")]
    else:
        body = [
            {
                "type": "column",
                "gap": "none",
                "children": [
                    {
                        "type": "row",
                        "justify": "between",
                        "children": [
                            text(m["authorName"] or m["author"][:10], style="label"),
                            {
                                "type": "row",
                                "gap": "sm",
                                "children": [
                                    {"type": "button", "id": f"react:{m['id']}", "label": "👋", "variant": "ghost"},
                                ]
                                # `delivery` is set on your own messages.
                                + ([{"type": "button", "id": f"delete:{m['id']}", "label": "Delete", "variant": "ghost"}] if m["delivery"] else []),
                            },
                        ],
                    },
                    {"type": "markdown", "text": m["content"]},
                ]
                + ([text(m["delivery"], style="small", tone="subtle")] if m["delivery"] else []),
            }
            for m in messages
        ]
    return [
        {
            "type": "card",
            "title": "Latest messages",
            "children": body
            + [
                {
                    "type": "input",
                    "id": "reply",
                    "placeholder": "Send a message to this channel",
                    "clearOnSubmit": True,
                },
            ]
            + ([text(state["error"], tone="danger")] if state.get("error") else [])
            + [{"type": "button", "id": "open", "label": "Open in Buzz", "icon": "ArrowSquareOut"}],
        }
    ]


def page(state):
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Hello from Python", style="title"),
        ]
        + ([text(f"Opened at route {json.dumps(state['route'])}", tone="accent")] if state.get("route") else [])
        + [
            text(
                "This page is drawn by a plugin process. Edit plugin.py and save: "
                "Buzz restarts it and this page redraws.",
                tone="subtle",
            ),
            {
                "type": "card",
                "title": "Counter",
                "children": [
                    {
                        "type": "row",
                        "children": [
                            {
                                "type": "button",
                                "id": "add",
                                "label": "Add one",
                                "variant": "prominent",
                                "icon": "Plus",
                            },
                            {"type": "button", "id": "reset", "label": "Reset"},
                            text(f"Clicked {state['count']} times"),
                        ],
                    }
                ],
            },
            {
                "type": "card",
                "title": "More elements",
                "children": [
                    {"type": "checkbox", "id": "tidy", "checked": state.get("tidy", False), "label": "Keep it tidy"},
                    {
                        "type": "choice",
                        "id": "mood",
                        "value": state.get("mood", "calm"),
                        "options": [{"value": "calm", "label": "Calm"}, {"value": "busy", "label": "Busy"}],
                    },
                    {"type": "progress", "value": min(state["count"], 10) / 10},
                    {
                        "type": "row",
                        "children": [{"type": "avatar", "pubkey": state["viewer"], "size": "small"}, text("You")]
                        if state.get("viewer")
                        else [],
                    },
                ],
            },
            {
                "type": "card",
                "title": "Notes",
                "children": [
                    {
                        "type": "input",
                        "id": "note",
                        "placeholder": "Write a note and press Enter",
                        # Buzz keeps what is typed and empties the field
                        # on Enter, so typing never waits on this plugin.
                        "clearOnSubmit": True,
                    },
                ]
                + [{"type": "markdown", "text": f"- {note}"} for note in state["notes"]],
            },
            {
                "type": "card",
                "title": "Channels in this community",
                "children": channel_list(state),
            },
        ]
        + live(state)
        + [
            {
                "type": "row",
                "children": [
                    {"type": "button", "id": "toast", "label": "Show a toast"},
                    {"type": "button", "id": "notify", "label": "Notify me", "icon": "Bell"},
                    {"type": "button", "id": "plugins", "label": "Plugin settings", "icon": "Gear"},
                    {"type": "button", "id": "route", "label": "Open at a route"},
                    {"type": "link", "label": "Open example.com", "url": "https://example.com/"},
                ],
            },
        ],
    }


def panel(state):
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Example link", style="heading"),
            # From a link, the target is the link; from the channel
            # header, it is the channel id.
            text(
                state["target"]
                if state["target"].startswith("https://")
                else "Opened from the channel header.",
                style="code",
            ),
            {"type": "divider"},
            text(f"Channel {state.get('channel')}", tone="subtle"),
            {"type": "button", "id": "toast", "label": "Say hi"},
        ],
    }


def settings_section(state):
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Hello settings", style="title"),
            text("This section is drawn by the plugin too.", tone="subtle"),
            {
                "type": "switch",
                "id": "loud",
                "checked": settings["loud"],
                "label": "Shout in toasts",
            },
        ],
    }


def greeting_tab(state):
    """A Greeter's Settings tab: what it says before the mention. Buzz
    draws its Provider field above; with a change of provider staged
    (`pending`), Save commits it."""
    greeting = state["agent"]["config"].get("greeting", "Hello")
    note = "Save to change the provider." if state.get("pending") else "Put before every reply."
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Greeting", style="label"),
            {"type": "input", "id": "greeting", "value": greeting, "placeholder": "Hello"},
            {"type": "row", "children": [{"type": "button", "id": "save", "label": "Save", "variant": "prominent"}]},
            text(state.get("saved") or note, tone="subtle"),
        ],
    }


def about_dialog(state):
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Hello is a sample plugin written in plain Python. It shows what a plugin can add to Buzz."),
            {"type": "row", "justify": "end", "children": [{"type": "button", "id": "close", "label": "Close", "variant": "prominent"}]},
        ],
    }


def thread_note(state):
    """An accessory: a line above composers that reply in a thread."""
    if not state.get("thread"):
        return {"type": "column", "gap": "none", "children": []}
    return {
        "type": "row",
        "gap": "sm",
        "children": [{"type": "icon", "name": "Smiley"}, text("Hello is following this thread.", style="small", tone="subtle")],
    }


def provider_dialog(state):
    """The Greeter's provider dialog: it needs nothing to run."""
    return {
        "type": "column",
        "gap": "md",
        "children": [
            text("Greeter runs inside the Hello plugin, so it is always ready."),
            {"type": "row", "justify": "end", "children": [{"type": "button", "id": "close", "label": "Done", "variant": "prominent"}]},
        ],
    }


def render(view):
    state = views[view]
    kind = state["kind"]
    if kind == "agentCreate":
        greeting = state["config"].get("greeting", "Hello")
        tree = {
            "type": "column",
            "gap": "sm",
            "children": [
                text("What should it say first?", style="label"),
                {"type": "input", "id": "create-greeting", "value": greeting, "placeholder": "Hello"},
            ],
        }
    elif kind == "agentProvider":
        tree = provider_dialog(state)
    elif kind == "dialog" and state["contribution"] == "words":
        tree = {
            "type": "column",
            "gap": "md",
            "children": [
                text(f"That message has {state['context']['count']} words."),
                {"type": "row", "justify": "end", "children": [{"type": "button", "id": "close", "label": "Close", "variant": "prominent"}]},
            ],
        }
    elif kind == "dialog":
        tree = about_dialog(state)
    elif kind == "accessory":
        tree = thread_note(state)
    elif kind == "agentTab":
        tree = greeting_tab(state)
    elif kind == "page":
        tree = page(state)
    elif kind == "settings":
        tree = settings_section(state)
    else:
        tree = panel(state)
    notify("view/render", {"view": view, "tree": tree})


def toast(title, tone="success"):
    if settings["loud"]:
        title = title.upper() + "!"
    request("ui/toast", {"title": title, "tone": tone})


def on_open(params):
    view = params["view"]
    views[view] = {**params, "count": 0, "notes": []}
    render(view)
    if params["kind"] == "page":

        def got_channels(result, error):
            if view in views:
                views[view]["channels"] = result or []
                render(view)

        request("community/channels", {}, got_channels)

        def got_community(result, error):
            if view in views and result:
                views[view]["viewer"] = result.get("viewer")
                render(view)

        request("community/current", {}, got_community)


def follow(view, channel):
    state = views[view]
    if state.get("watch"):
        request("channel/unwatch", {"watch": state["watch"]})
    state.update(following=channel, watch=None, messages=None, error=None)

    def watching(result, error):
        if view not in views:
            return
        if error:
            views[view]["error"] = error["message"]
        else:
            views[view]["watch"] = result["watch"]
        render(view)

    request("channel/watch", {"channel": channel, "limit": 10}, watching)


def on_changed(params):
    """A followed channel changed: show its newest messages."""
    for view, state in views.items():
        if state.get("watch") == params["watch"]:
            state["messages"] = params["messages"]
            render(view)


def on_event(params):
    view, element, event = params["view"], params["id"], params["event"]
    state = views.get(view)
    if state is None:
        return
    if element == "add" and event == "click":
        state["count"] += 1
        notify("page/badge", {"page": "hello", "text": str(state["count"])})
    elif element == "reset" and event == "click":
        state["count"] = 0
        notify("page/badge", {"page": "hello", "text": None})
    elif element == "loud" and event == "toggle":
        settings["loud"] = params.get("value", False)
    elif element == "notify" and event == "click":
        global next_request
        request(
            "notifications/submit",
            {
                "category": "greetings",
                "sourceKey": f"hello-{next_request}",
                "title": "Hello!",
                "body": "A notification from a plugin.",
            },
        )
        return
    elif element == "tidy" and event == "toggle":
        state["tidy"] = params.get("value", False)
    elif element == "mood" and event == "change":
        state["mood"] = params.get("value")
    elif element == "note" and event == "submit":
        note = params.get("value", "").strip()
        if not note:
            return
        state["notes"].append(note)
    elif element.startswith("follow:") and event == "click":
        follow(view, element.split(":", 1)[1])
    elif element == "reply" and event == "submit":
        content = params.get("value", "").strip()
        if not content:
            return

        def sent(result, error):
            if view in views:
                views[view]["error"] = error["message"] if error else None
                render(view)

        request("messages/send", {"channel": state["following"], "content": content}, sent)
        return
    elif (element.startswith("react:") or element.startswith("delete:")) and event == "click":
        action, message = element.split(":", 1)
        params = {"channel": state["following"], "message": message}
        if action == "react":
            request("messages/react", {**params, "emoji": "👋"})
        else:
            request("messages/delete", params)
        return
    elif element == "close" and event == "click" and state["kind"] in ("dialog", "agentProvider"):
        request("ui/closeView", {"view": view})
        return
    elif element == "route" and event == "click":
        count = state["count"]
        request("ui/navigate", {"to": "page", "page": "com.example.hello/hello", "params": {"count": count}})
        return
    elif element == "plugins" and event == "click":
        request("ui/navigate", {"to": "settings", "section": "plugins"})
        return
    elif element == "open" and event == "click":
        request("ui/openConversation", {"channel": state["following"]})
        return
    elif element == "toast" and event == "click":
        toast("Hello from a plugin")
        return
    elif element == "create-greeting" and event == "change":
        config = {**state["config"], "greeting": params.get("value", "").strip() or "Hello"}
        state["config"] = config
        request("agent/createConfig", {"view": view, "config": config})
        return
    elif element == "greeting" and event == "change":
        state["draft"] = params.get("value", "")
        return
    elif element == "save" and event == "click":
        current = state["agent"]["config"].get("greeting", "Hello")
        greeting = state.get("draft", current).strip() or "Hello"
        config = {**state["agent"]["config"], "greeting": greeting}

        def saved(result, error):
            if view in views:
                views[view]["saved"] = error["message"] if error else "Saved."
                views[view]["agent"]["config"] = config
                render(view)

        request("agent/save", {"view": view, "config": config}, saved)
        return
    else:
        return
    render(view)


def link_chip(url):
    """A link renderer's answer: what the chip for an example.com link shows."""
    path = url[len("https://example.com"):] or "/"
    return {
        "type": "row",
        "gap": "sm",
        "children": [{"type": "icon", "name": "Globe"}, text(f"Example · {path}")],
    }


def hello_card(message):
    """A message renderer's answer: a card for "!hello" messages."""
    rest = message["content"][len("!hello"):].strip() or "Hello!"
    return {
        "type": "card",
        "title": f"{message['authorName'] or 'Someone'} says hello",
        "children": [
            {"type": "markdown", "text": rest},
            {"type": "button", "id": "wave", "label": "Wave back", "icon": "Smiley"},
        ],
    }


def ticket_badge(ticket, message):
    """An inline renderer's answer: a badge for HELLO-<number>, or None to
    leave HELLO-0 as plain text. A ticket in a thread reply is marked
    done, to show the answer can depend on the message."""
    number = int(ticket.split("-", 1)[1])
    if number == 0:
        return None
    if message.get("threadRoot"):
        return {"type": "badge", "text": f"✅ {ticket}", "tone": "success"}
    return {"type": "badge", "text": f"🎫 {ticket}", "tone": "accent"}


def on_action(params):
    action, message = params["action"], params["message"]
    if action == "words":
        count = len(message["content"].split())
        # A dialog the plugin opens itself; its view gets the context.
        request("ui/openDialog", {"title": "Word count", "id": "words", "context": {"count": count}})
    elif action == "star":
        starred.symmetric_difference_update({message["id"]})
        notify("message/markers", {"action": "star", "messages": sorted(starred)})


def run_agent(run_request, params):
    """A Greeter run: answer a mention in its thread, as the agent."""
    trigger = params["trigger"]
    if trigger["kind"] != "mention":
        send({"id": run_request, "result": None})
        return
    event = trigger["event"]
    name = params["agent"]["name"]

    def sent(result, error):
        # Answering agent/run ends the run.
        if error:
            send({"id": run_request, "error": {"code": 1, "message": error["message"]}})
        else:
            send({"id": run_request, "result": None})

    greeting = (params.get("config") or {}).get("greeting", "Hello")
    text = f"{greeting} from {name}! You said: {event['content']}"
    request("agent/tool", {"run": params["run"], "tool": "send", "arguments": {"text": text, "audience": "everyone"}}, sent)


def handle(message):
    method = message.get("method")
    if method is None:
        then = pending.pop(message.get("id"), None)
        if then:
            then(message.get("result"), message.get("error"))
        return
    params = message.get("params") or {}
    if "id" in message:
        if method == "initialize":
            send(
                {
                    "id": message["id"],
                    "result": {
                        "pages": [{"id": "hello", "title": "Hello", "icon": "Smiley", "routes": True}],
                        "panels": [
                            {
                                "id": "example",
                                "title": "Example",
                                "icon": "Globe",
                                "matches": ["https://example.com/"],
                                "launcher": {"label": "Example", "placement": "side"},
                                "menu": {"label": "Open the example panel"},
                            }
                        ],
                        "settingsCards": [
                            {"id": "settings", "title": "Hello", "icon": "Smiley", "placement": "Integrations"}
                        ],
                        "notifications": [{"id": "greetings", "label": "Hello greetings"}],
                        "messageActions": [
                            {"id": "words", "title": "Count words", "icon": "TextAa"},
                            {"id": "star", "title": "Star or unstar", "icon": "Flag"},
                        ],
                        "linkRenderers": [{"id": "example-link", "matches": ["https://example.com/"]}],
                        "messageRenderers": [{"id": "hello-card", "prefix": "!hello"}],
                        "composerTools": [{"id": "wave", "title": "Insert a wave", "icon": "Smiley"}],
                        "shortcuts": [{"id": "hi", "title": "Say hi", "keys": ["mod+shift+h"]}],
                        "completions": [{"id": "commands", "title": "Commands", "trigger": "/"}],
                        "accountActions": [{"id": "about", "title": "About Hello", "icon": "Smiley"}],
                        "composerAccessories": [{"id": "thread-note", "title": "Thread note"}],
                        "inlineRenderers": [{"id": "tickets", "pattern": "\\bHELLO-[0-9]{1,6}\\b"}],
                        "agentTools": [
                            {
                                "name": "hello_time",
                                "description": "The time on the computer running Buzz.",
                                "inputSchema": {"type": "object", "properties": {}},
                            }
                        ],
                        "agentTypes": [
                            {
                                "id": "greeter",
                                "title": "Greeter",
                                "description": "Answers every mention with a greeting. Runs in the Hello plugin.",
                                "instructions": "Answer every mention.",
                                "config": {"greeting": "Hello"},
                                "summary": True,
                                "peek": True,
                                "tabs": [{"id": "greeting", "title": "Settings", "role": "settings"}],
                                "icon": "Smiley",
                                "createForm": True,
                                "createModels": {
                                    "models": [
                                        {"id": "polite", "label": "Polite"},
                                        {"id": "brief", "label": "Brief", "description": "One line only."},
                                    ],
                                    "defaultLabel": "Plain",
                                },
                                "models": True,
                                "provider": True,
                                "carry": {"instructions": "instructions"},
                                "migrationSettings": [
                                    {"id": "greeting", "label": "Greeting", "key": "greeting", "default": "Hello"}
                                ],
                            }
                        ],
                    },
                }
            )
            # The Greeter needs nothing installed, so it is ready at once.
            notify("agentType/readiness", {"type": "greeter", "readiness": "ready"})
        elif method == "link/render":
            send({"id": message["id"], "result": {"tree": link_chip(params["url"])}})
        elif method == "agentTool/call" and params["tool"] == "hello_time":
            now = time.strftime("%Y-%m-%d %H:%M:%S %Z")
            send({"id": message["id"], "result": {"text": f"It is {now}."}})
        elif method == "agent/run":
            run_agent(message["id"], params)
        elif method == "completion/query":
            query = params["query"].lower()
            items = [
                {"label": f"/{name}", "detail": text, "text": text + " "}
                for name, text in COMMANDS.items()
                if name.startswith(query)
            ]
            send({"id": message["id"], "result": {"items": items}})
        elif method == "inline/render":
            badge = ticket_badge(params["text"], params["message"])
            send({"id": message["id"], "result": badge and {"tree": badge}})
        elif method == "agent/models":
            send({"id": message["id"], "result": {
                "current": params["agent"]["config"].get("model") or "polite",
                "models": [
                    {"id": "polite", "label": "Polite", "efforts": [{"id": "low", "label": "Low"}, {"id": "high", "label": "High"}], "effort": "low"},
                    {"id": "brief", "label": "Brief", "efforts": []},
                ],
            }})
        elif method == "agent/summary":
            greeting = params["agent"]["config"].get("greeting", "Hello")
            send({"id": message["id"], "result": {"text": f"Says “{greeting}”"}})
        elif method == "agent/peek":
            agent = params["agent"]
            peek = {
                "type": "column",
                "children": [
                    text("Greeter", style="label"),
                    text(f"{agent['name']} answers mentions with “{agent['config'].get('greeting', 'Hello')}”.", tone="subtle"),
                ],
            }
            send({"id": message["id"], "result": {"tree": peek}})
        elif method == "message/render":
            send({"id": message["id"], "result": {"tree": hello_card(params["message"])}})
        else:
            send({"id": message["id"], "error": {"code": -32601, "message": method}})
    elif method == "view/open":
        on_open(params)
    elif method == "view/event":
        on_event(params)
    elif method == "view/close":
        views.pop(params.get("view"), None)
    elif method == "agentType/check":
        notify("agentType/readiness", {"type": params["type"], "readiness": "ready"})
    elif method == "page/route":
        if params["view"] in views:
            views[params["view"]]["route"] = params["route"]
            render(params["view"])
    elif method == "channel/changed":
        on_changed(params)
    elif method == "message/action":
        on_action(params)
    elif method == "composer/tool":
        request("composer/insert", {"composer": params["composer"], "text": "👋 "})
    elif method == "shortcut":
        toast("Hi from a shortcut")
    elif method == "render/event" and params.get("id") == "wave":
        toast("You waved back")


def main():
    print("hello plugin started", file=sys.stderr)
    while True:
        line = sys.stdin.readline()
        if not line:
            break  # Buzz closed stdin: time to exit.
        if line.strip():
            handle(json.loads(line))


if __name__ == "__main__":
    main()
