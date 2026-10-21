# Writing a plugin

A Buzz plugin is a program on your computer. Buzz starts it, asks what it
adds, and draws what it sends with Buzz's own components. You can write it in
any language that reads stdin and writes stdout: a short Python or Node script
is enough, and no SDK is needed.

A plugin can add:

- **Pages**, in the channel sidebar or the header, with a badge.
- **Panels** beside the conversation. A panel opens from a link it matches,
  a button in the channel header or the channel's menu.
- **Settings sections** and **notification categories**.
- **Message actions** in a message's More menu, with an optional marker.
- **Renderers** that draw link chips, message bodies or attachments.
- **Composer tools**, **completions** and **keyboard shortcuts**.
- **Agent tools** every agent session gets, and **kinds of agent** the plugin
  runs.

[`examples/plugins/hello`](../examples/plugins/hello) is a complete plugin in
one Python file with no dependencies. It adds one of each, except an
attachment renderer.

## Try the example

1. Open **Settings → Plugins** and click **Add plugin folder…**.
2. Choose `examples/plugins/hello`. New plugins start off.
3. Read the command it runs, then turn it on. **Hello** appears in the channel
   sidebar.
4. Edit `plugin.py` and save. Buzz restarts the plugin, and the open page
   redraws.

## Trust

A plugin runs with your full access. It is not sandboxed: it can read your
files and use your network like any other program you run. Turn on only
plugins you trust. Settings shows the command a plugin runs before you turn it
on.

This is buzz-app's model too: its plugins are trusted code in the app's own
WebView. Buzz never gives a plugin your private key, and it removes `BUZZ_*`
and `NOSTR_*` variables from the plugin's environment.

Because the key stays in Buzz, acting as you is something Buzz does for the
plugin, and only when the plugin declares it (see `permissions` below).
Settings lists what a plugin can do; turning the plugin on allows it. If a
new version asks for more, Buzz turns the plugin off until you review it and
turn it on again.

## The folder

A plugin folder holds `buzz-plugin.json` and whatever the plugin needs:

```json
{
  "id": "com.example.hello",
  "name": "Hello",
  "description": "A sample page and link panel.",
  "command": ["python3", "plugin.py"],
  "permissions": ["send-messages"]
}
```

| Field | Meaning |
| --- | --- |
| `id` | Unique. Lowercase letters, digits, `.`, `-` and `_`, such as `com.example.weather`. Ids starting with `buzz.` are reserved. |
| `name` | Shown in Settings. 1 to 64 characters. |
| `description` | Optional. Shown in Settings. |
| `command` | The program and its arguments. A program containing `/` is relative to the folder, such as `./plugin`. Otherwise Buzz finds it on `PATH`, except `node`, which runs the Node that comes with Buzz when it has one. |
| `apiVersion` | Optional. `1`, the only version so far. |
| `permissions` | Optional. What the plugin may do as you. `send-messages`: send, edit, react to and delete messages, and upload files to attach. `publish-events`: publish other Nostr events (see `events/publish`). `manage-agents`: create, change and remove your agents and teams. `manage-channels`: start DMs; create, join, leave, archive and delete channels; change members, details, workflows and your kit. Reminders need `send-messages`, signing a Git token `publish-events`, and an agent log proof `manage-agents`. Reading needs no permission, and neither do your read marks and thread follows. |

The plugin runs in its folder. Buzz sets `BUZZ_PLUGIN_ID`,
`BUZZ_PLUGIN_API_VERSION`, `BUZZ_PLUGIN_DATA_DIR` (a folder of its own for
files it keeps, which outlives updates and is never watched) and
`PYTHONUNBUFFERED=1`.

Buzz watches the folder. Saving any file restarts the plugin, except for files
under hidden folders (such as `.cache`), `node_modules`, `target`,
`__pycache__` and `venv`. Keep the plugin's own state under a hidden folder so
that writing it does not restart the plugin.

## How Buzz talks to a plugin

Messages are [JSON-RPC 2.0](https://www.jsonrpc.org/specification), one JSON
object per line, on the plugin's stdin and stdout. stdout is only for
messages: write logs to stderr. Buzz logs each stderr line and shows the last
ones if the plugin stops.

The plugin should exit when stdin closes. To stop a plugin, Buzz closes its
stdin, waits up to two seconds, then ends the plugin and every process it
started.

### 1. `initialize` (request from Buzz)

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"apiVersion":1,"app":{"name":"Buzz","version":"0.1.0"},"host":{"browser":false}}}
```

`host` says what this app offers. `browser` is `false`: there is no embedded
browser, so a `link` element or `ui/openUrl` opens the system's browser.

Answer with what the plugin adds. The answer is due within 10 seconds.

```json
{"jsonrpc":"2.0","id":1,"result":{
  "pages":[{"id":"home","title":"Weather","icon":"Sun"}],
  "panels":[{
    "id":"forecast",
    "title":"Forecast",
    "icon":"Globe",
    "matches":["https://weather.example/"],
    "launcher":{"label":"Forecast","placement":"side"}
  }]
}}
```

- A contribution `id` starts with a lowercase letter or digit, then uses
  those, `.`, `-` and `_`, up to 128 characters. Titles are
  1 to 64 characters.
- `icon` names a Buzz icon, such as `House`, `Globe`, `Bell`, `Gear`, `Code`
  or `Plug`. An unknown name shows the plug icon. A page's `icon` can also be
  a `data:image/` URL (PNG, JPEG, GIF, WebP or SVG, up to 64 KiB), drawn at
  16px in the sidebar and search, as buzz-app's page icons are.
- `matches` lists link prefixes. A matching link clicked in a message opens
  the panel as a tab. When several panels match, the lowest `order` wins,
  then the lowest `plugin/id` key. Buzz's own panels are at 0. By convention,
  use -10 to specialise links another panel shows, 0 (the default) for your
  own links and 100 for a catch-all.
- `launcher` adds a button to the channel header. `placement` is `bottom`
  (the default, the drawer under the conversation) or `side`; `target` is what the panel opens showing (the
  channel's id by default). `appLauncher` (`{"label","target"}`) adds a
  button to the app's header instead, which opens the panel at `target`
  beside any page and keeps it there while you move between pages.
  A panel's `layout` is `document` (the default, padded) or `workspace`
  (edge to edge, for a panel that draws its own `header`). `pattern`, a
  regular expression of up to 500 characters, narrows the links a panel
  shows (with `matches`, or alone), so the rest open in the browser.
  Outside a conversation, `ui/openPanel`, `panel/open` and a matching
  link open the panel there too. `menu` (`{"label"}`) adds an entry to the channel's
  menu, which opens the panel as one of the channel's tabs; `kinds` (such as `["stream", "forum"]`) and `private` (`true` or
  `false`) limit the channels it is offered in, and so do `member`
  (channels you are in), `writable` (live channels you can write in: not
  this device's saved copy, read-only or archived) and `archive` (only
  while this device saves your agents' activity).
- A page's `placement` is `sidebar` (the default), `topbar` (`toolbar` is the
  same here) or `hidden`; one Buzz does not know is dropped, leaving the
  default. `primary: false` also keeps it out of navigation. `layout` is `document` (the default, padded) or
  `workspace` (edge to edge). Buzz's dock shows the companion beside every
  page; `companion: true` is reported in `contributions/list`. With `routes: true`, `ui/navigate` can open the page at `params` (any
  JSON value but `null`, up to 8 KiB): its view's `view/open` carries `route`, and Buzz
  sends `page/route` with the `view`, `route` and visit `entry` when it changes. Back and
  Forward step through routes. `routeVersion` (default 1) says what the
  params mean: raise it when they change, and a `ui/navigate` that names
  another `version` is refused. A page settles a route itself: it draws
  what the route shows, fails it with `page/settle` (with
  `handlesNavigation`), or redirects with `ui/navigate` and `replace: true`. With `handlesNavigation: true`, a
  visit to the page stays opening, so `ui/navigate` has not answered, until
  the page sends `page/settle` with `opened` or `failed`, or 15 seconds pass
  (it then fails with `timeout`); `page/resolve` picks the visit's route
  meanwhile (on the same page: buzz-app's `resolve` can name any target,
  which here is `ui/navigate` with `replace: true`). A page's `view/open` carries the visit's `entry` and `focus`
  (`preserve` when the control that opened it keeps the focus, else
  `content`, as buzz-app's `NavigationFocus`). Every later visit to an open
  `handlesNavigation` page, its sidebar or top-bar button, Back, Retry or
  the same `ui/navigate` included, sends `page/route` with the route (or
  `null`), the new `entry` and its `focus`, and waits for `page/settle`
  again.
- A plugin can add at most 16 panels and 8 pages, and at most 16 of each kind
  below. Ids are unique across all of them, except that a notification
  category may share its id with another contribution, as a page and the
  category for its notifications do.

The rest of the result can list:

| Key | Each entry | What it adds |
| --- | --- | --- |
| `settingsCards` | `id`, `title`, `icon`, `placement`: `community` (the default, under the selected community), `administration` or an account-level group name, such as `Integrations` | A Settings section. It is a view with `kind` `settings`; its `view/open` carries the selected `community`'s relay. |
| `notifications` | `id`, `label` | A category with its own switch in Settings → Notifications. Send with `notifications/submit`. |
| `messageActions` | `id`, `title`, `icon`, `kinds` (default chat messages), and filters in place of buzz-app's `matches(message)`: `mine`, `attachments` (`true` or `false`), `replies` (`true` for thread replies, `false` for top-level messages), `pattern` (a regular expression the text must match, up to 500 characters), `channels` (channel ids), `authors` (up to 500 pubkeys, such as your agents), `reactions` (`true` or `false`) | An item in a message's More menu, on the messages every filter allows. Choosing it sends `message/action`. |
| `linkRenderers` | `id`, `title` (optional), `matches` (link prefixes), `order` | Draws the chip of matching links in messages. Buzz asks with `link/render`. Only the renderer that ranks first is asked; Buzz's own chips (`buzz.links/links`) rank at 0 for Buzz and known links and 100 for the web, so a plugin outranks them with a lower order, or the same order and a lower key. A `null` leaves the chip to Buzz. |
| `messageRenderers` | `id`, `title` (optional), `kinds` (default chat messages), `prefix` (optional), `order` | Draws the body of matching messages. Buzz asks with `message/render`, once per message and edit; answer `null` to leave a message to Buzz, as a buzz-app `matches()` that declines. Without a `prefix` it is asked about every message of its kinds. Of renderers that match, the lowest `order`, then the lowest `plugin/id` key, is asked first, and a `null` passes to the next; Buzz's own diff renderer (`buzz.diffs/diff`) is at 0. buzz-app takes the first registered instead. Every message is offered whole, with no text or with a time prefix as a timecoded reply has. |
| `attachmentRenderers` | `id`, `title` (optional), `mime` (MIME type prefixes) | Draws matching attachments, as buzz-app does: audio hosted by the community in messages, in place of the audio player; a voice note being recorded into the composer; and, in a draft's preview, audio and voice drafts and uploaded files (pictures and videos keep Buzz's own stage, and a file not uploaded yet has no preview). Audio shown before its upload finished keeps its local `url` (`draft-audio:` or `voice-draft:`, which `media/get` reads) while its slide shows. Buzz asks with `attachment/render`. Images, videos and files in messages keep Buzz's own views, and voice notes keep the Voice Notes plugin's card while it is on. |
| `inlineRenderers` | `id`, `title` (optional), `pattern` (a regular expression, up to 500 characters, that cannot match empty text) | Draws the pieces of message text the pattern matches, such as ticket numbers. Buzz asks with `inline/render`, a plain run of text at a time: mentions, links, channel names and code are left alone, and so are custom emoji while Emoji is on (with it off they are text a renderer may draw). Reaction pills stay Buzz's own, as in buzz-app. With Emoji off, a custom emoji in the hover bar's quick reactions is offered too, and what a renderer's pattern matches in its `:shortcode:` is drawn by it (`ReactionLabel`). |
| `composerTools` | `id`, `title`, `icon`, `order`, `reactionPicker` | A button in every composer's toolbar, placed among Buzz's own by `order` then key: mentions at -10 (`buzz.mentions/picker`), emoji at 0 (`buzz.emoji/picker`) and voice notes at 10 (`buzz.voice-notes/recorder`); tools ordered below 0 lead it, beside the recipients. Switching one of those plugins off frees its place, its trigger and, for Emoji, the reaction picker; Links off frees its link chips. Clicking it sends `composer/tool`. Buzz disables the button while the composer cannot be used, such as while it sends or in a read-only channel. With `reactionPicker: true`, it can also replace the emoji picker that "Add reaction" opens: the tool with the lowest `order`, then the lowest `plugin/id` key, wins, and Buzz's own picker is `buzz.emoji/picker` at 0. The picker is a view of `kind` `reactionPicker`; its `view/open` carries the `community` the reaction goes to and `disabled` (true while nothing can be sent there, as at opening), and `reaction/select` is refused while that holds. Send `reaction/select` with its `view` and an `emoji` to react. |
| `composerAccessories` | `id`, `title`, `order`, `placement`, `kinds` | With `placement` `composer` (the default): a view above every composer, of `kind` `accessory`; `view/open` also carries the composer's `community`, `channel` and `thread`. Keep it to a line or two. With `placement` `message`: something beside each message's reactions, such as work on it; Buzz asks `message/accessory`, for chat messages or the `kinds` listed. |
| `namingPolicies` | `id` (at most one) | Display names for people and agents (`identityNames.register`). Buzz asks `names/resolve` and shows public names until the answer comes. As in buzz-app, where a second policy is refused, the first one on applies: Buzz's own Identity Naming, else the plugin policy registered first. Switch off Identity Naming in Settings → Plugins to use a plugin's. |
| `services` | `id` | Something the plugin offers other plugins (`ctx.provide`). Another plugin calls it with `services/request`, and Buzz asks this one `service/request`. The plugin sends the notification `service/publish` (`service`, `event`, `data`) to tell plugins that subscribed. |
| `accountActions` | `id`, `title`, `icon` | A row in the account menu that opens a dialog titled `title`. The dialog's body is a view of `kind` `dialog`. Send `ui/closeView` with its `view` to close it. |
| `completions` | `id`, `title`, `trigger` (one character, not a letter, digit or space), `order` | Typing the trigger at the start of a word in a composer opens the plugin's suggestions. Buzz asks with `completion/query`. When plugins share a trigger, the lowest `order`, then the lowest `plugin/id` key, answers. Buzz's own mentions (`@`, order -10, `buzz.mentions/typeahead`), channels (`#`, order 0, `buzz.channels/channel-typeahead`) and emoji (`:`, order 0, `buzz.emoji/typeahead`) rank the same way: a completion takes one of those triggers only by ranking before Buzz's own, as in buzz-app. A query runs from the trigger to the caret within one word, up to 64 bytes. |
| `shortcuts` | `id`, `title`, `keys` (at least one, such as `mod+shift+k`), `order`, `allowInEditable`, `allowInModal`, `repeat` | A shortcut, listed and rebindable in Settings → Shortcuts. Pressing it sends `shortcut`; when several share a chord, the first by `plugin/id` key that is on and may run where the key was pressed takes it. Write the plus key as `mod++` or `+`. A chord may be a bare key such as `j` or `?`, and a symbol typed with Shift needs no `shift` (`shift+?` is `?`); Buzz's own chords win. It does not run while a text field has focus or a dialog is open unless `allowInEditable` or `allowInModal` is set, and a held chord runs once unless `repeat` is set. |

### Agents

`agentTools` lists tools every agent gets beside the Buzz tools: Claude,
Codex and the plugin's own agents. Each has a `name` (a lowercase letter,
then lowercase letters, digits or `_`, not a Buzz tool's name), a
`description` and an `inputSchema` (a JSON Schema object). When an agent
calls one, Buzz sends a request:

```json
{"jsonrpc":"2.0","id":4,"method":"agentTool/call","params":{
  "call":12,"tool":"hello_time","arguments":{},
  "agent":"<agent pubkey>","channel":"<channel id>","root":"<thread root id>"
}}
```

Answer with `{"text"}`, `{"image": {"data", "mime"}}` (base64) or an error.
A call may take five minutes. Agent sessions read their tools when they
start, so adding or changing a tool replaces idle Claude sessions (which
resume) and starts new Codex threads.

`agentTypes` lists kinds of agent, which show in Create agent beside Claude
and Codex. Each has an `id`, a `title`, an optional `description`, `config`
(a new agent's settings), `instructions` (a new agent's default Interest),
`attention` (more attention objects a new agent starts with, by slug, as
`agents/save` takes them) and
`timeoutSeconds` (default 60, at most 1800). Each time such an agent wakes,
Buzz sends a request and the run lasts until the plugin answers it:

```json
{"jsonrpc":"2.0","id":5,"method":"agent/run","params":{
  "run":13,"type":"greeter",
  "trigger":{"kind":"mention","event":{"id","pubkey","createdAt","kind","content","tags"}},
  "channel":"<channel id>","root":"<thread root id>",
  "agent":{"pubkey","name","owner"},
  "config":{},"model":null,"effort":null,"team":null
}}
```

A trigger's `kind` is `mention`, `watch` (with `watch`, the slug,
`watchObject`, `interest` and `classifierSkipped`, the reason a classifier did
not run) or `timer` (with `timer`, `timerObject`, `interest` and `spent`). A run
that queues its work can ask `agent/current` with the `run` whether the
trigger should still run. If the run is cancelled, Buzz
sends the notification `agent/cancel` with the `run`.

A type can also describe its agents on the Agents page. Each request's params
are `type` and `agent`, the whole record as `agents/list` gives it, and Buzz
keeps the answer until anything in it changes:

- `summary: true`: Buzz asks `agent/summary` for the line under the agent's
  name. Answer `{"text"}` (up to 120 characters) or `null` for the title.
- `peek: true`: Buzz asks `agent/peek` for a glance shown when the agent is
  selected. Answer `{"tree"}` or `null`, as a renderer does.
- `createInstructions: true`: the New agent dialog offers the new agent's
  instructions.
- `icon` names a Buzz icon shown with the type; the robot without one.
- `createForm: true`: the type's own part of the New agent dialog, in place
  of the instructions. It is a view of `kind` `agentCreate` whose `view/open`
  carries `type`, the starting `config` and `disabled` (the agent is being
  made); send `agent/createConfig` with the `view` and the whole new `config`
  as it changes. Buzz sends `view/agentCreate` (`view`, `config`, `disabled`)
  when either changes, such as a model picked from `createModels`.
- `createModels` (`{"models": [{"id", "label", "description"}], "defaultLabel",
  "allowCustom"}`): model choices in the New agent dialog, saved as the
  config's `model`.
- `models: true`: Buzz asks `agent/models` for the models and effort levels the
  owner can pick while mentioning an agent. Answer `{"current", "models":
  [{"id", "label", "efforts": [{"id", "label"}], "effort"}]}`. The picks reach
  `agent/run` as `model` and `effort`.
- `provider: true`: the type has device-wide readiness under Provider settings
  on the Agents page. Send the notification `agentType/readiness` with `type`,
  `readiness` (`checking`, `ready` or `attention`), and optional
  `attentionLabel`, `blocking` and `action`. Buzz sends `agentType/check` when
  the user asks to check again. The provider's dialog is a view of `kind`
  `agentProvider`; close it with `ui/closeView`. With `action: true`, picking
  the provider while it needs attention sends `agentType/attention` (`type`)
  rather than opening the dialog, for a fix the plugin starts itself, such as
  an install.
- `hosted: true`: the plugin holds each agent's key and runs it, as a hosted
  service would. Buzz never sends `agent/run` for it. Making one asks
  `agent/provision` (`type`, `name`, `community`; answer `{"pubkey"}`, a key
  not yet in use), then `agent/activate` (`type`, `pubkey`, `name`,
  `community`, `auth`: your NIP-OA attestation `["auth", owner, "",
  signature]` to attach to what it signs), then lists it in your agent list
  and, with a picture, asks `agent/profile` (`type`, `pubkey`, `community`,
  `name`, `picture`). Deleting it asks `agent/remove` (`type`, `pubkey`,
  `community`). Each step may come again after a failure, so make each safe to
  repeat, and each must answer within 2 minutes. The Agents page shows these agents as Hosted, and `agents/create`
  with this type makes one the same way. `agents/save` renames one or
  changes its picture through `agent/profile` (its other settings are the
  provider's and are refused), and `agents/remove` asks `agent/remove`,
  then archives it where you may, deletes its record and takes it out of
  its channels. A hosted type takes no `models`
  or `timeoutSeconds`, and no local agent can be changed to it or brought
  over as one (`agents/retype`, `agents/previewRetype`, `agents/port`).
- `carry` (`{"instructions", "workspace"}`, each a config key): what another
  type takes over when an agent changes type, and where this type keeps what
  it takes over. `migrationWarning` says what leaving this type costs.
  `migrationSettings` (`[{"id", "label", "key", "default"}]`) lists the
  settings a change of type previews: one is the agent's own when its config
  has `key` set to something other than `default`, and the preview says
  whether the new type keeps it (it has a setting with the same `id` and
  value), resets it or drops it.
- `tabs` (`[{"id", "title", "role"}]`, at most 8; `attention`, `memory` and
  `settings` are Buzz's ids): settings tabs before Attention. Each is a view of `kind` `agentTab`; `view/open` also carries
  `type`, `agent`, `contribution` (the tab id) and `conversation`
  (`{"channel","root"}` when the agent was opened from a message). Buzz sends
  `view/agent` (`view`, `agent`) when the agent shown changes, such as a
  change saved elsewhere. A tab not selected is closed (`view/close`) and
  opens again when it is selected. To save, send `agent/save` with the `view` and the whole
  new `config`. With `role:
  "settings"` the tab is the agent's Settings, first, with Buzz's Provider
  field at its top. When the user picks this type there for an agent of
  another type, the tab opens with `pending: true` and `agent.config` set to
  what this type starts with; `agent/save` then makes the change with that
  config.

While a call or run is open, the plugin can act as its agent. A run that
must keep working after it answers (buzz-app's `workingUntil`) answers, then
opens a call of its own with `agent/open` and acts through that:

| Request | Params | Result |
| --- | --- | --- |
| `agent/open` | `agent` (the pubkey of one of your type's agents on this device), `channel`, `root` (optional) | `{"call"}`, a call that stays open until `agent/close` or the plugin stops |
| `agent/close` | `call` | `null` |
| `agent/tool` | `call` or `run`, `tool` (a Buzz tool, such as `send`, `read`, `react` or `channels`, or in a run one of the attention tools, such as `watch_add`), `arguments` | The tool's answer: `{"text"}` or `{"image"}` |
| `agent/step` | `run`, `tool`, `target` (optional) | `null`. Shows what the run is doing on its message, such as `read` on a channel. |
| `agent/acknowledged`, `agent/superseded`, `agent/ignored` | `run` | `null`. Marks the run's message as taken up, as replaced by a newer request (its bubble hides), or as declined (nothing shows). |
| `agents/activity` | none | What agents on this device are doing, newest first: `[{"community","agent","channel","message","root","state","startedAt","error","dismissed","step"}]`, `state` being `waiting`, `working`, `done`, `error` or `superseded`; `community` is the relay it runs in, and `dismissed` says you hid its error |
| `agents/watchActivity` | none | `{"watch"}`. Buzz then sends `agents/activityChanged` with `activity`, what `agents/activity` gives, now and whenever it changes. Stop with `agents/unwatch`. |
| `agent/current` | `run` | Whether the trigger should still run: `false` once the agent is removed, its attention turned off, or its watch changed |
| `agent/query` | `call` or `run`, `filters` (1 to 10 NIP-01 filters) | The events, read as the agent, at most 500 |
| `agent/publish` | `call` or `run`, `kind`, `content`, `tags` | The signed event, published as the agent |
| `agent/upload` | `call` or `run`, `path` (a file, up to the community's limit for its type) or `data` (base64, up to 3 MiB), `mime` | `{"url","sha256","size","type",…}`, uploaded as the agent |

The agent's key stays on its host; Buzz signs for it. Buzz tools that read
local files or memories are not available to plugin agents. `send` needs
`audience`: `everyone`, or `agents` for a message only agents act on.

### Renderers

Buzz draws while it paints and does not wait for a plugin. The first time a
renderer matches something, Buzz asks the plugin, draws what it would have
drawn anyway, and redraws when the answer comes. Answers are kept until the
plugin restarts, so answer from the request alone. Completions work the
same way, per query.

| Request from Buzz | Params | Answer |
| --- | --- | --- |
| `link/render` | `renderer`, `url` | `{"tree"}`, or `null` to leave the chip as it is |
| `message/render` | `renderer`, `message` | `{"tree"}` or `null` |
| `message/accessory` | `accessory`, `message`, `community` (the message's `wss://` relay), `openThread` (`true` where this place can open the message's thread, as a timeline can and a thread cannot) | `{"tree"}` to show beside its reactions, or `null`. Asked again when the message is edited or gets a reply. To open the thread as the place the user last acted in does, send `message/openThread` (after an event from that place; `false` once that place is gone). |
| `attachment/render` | `renderer`, `attachment` (`url`, `kind`: `image`, `video`, `audio` or `file`, `mime`, `name`, `size`, `duration`, `waveform`, `voiceNote`, `dimensions` `{"width","height"}`, `blurhash`, `previewUrl`), `removable` (a draft the plugin may take out with `attachment/remove`, buzz-app's `onRemove`) | `{"tree"}` or `null` |
| `inline/render` | `renderer`, `text` (what the pattern matched, up to 200 bytes; at most 32 pieces per segment are asked about), `segment` (the plain-text segment it was found in, `content.text`) and `start` (where, in characters), `message` (the message it is in), `reaction` (a quick reaction drawn, as `{"content","emoji":{"shortcode","url"},"events":[]}`, or `null` for message text) | `{"tree"}`, drawn in the line of text, or `null` to leave it as text. Buzz asks once per piece and message. |
| `names/resolve` | `policy`, `identities` (`[{"pubkey","name","isAgent","owner"}]`, at most 5,000), `viewer`, `candidates` (the keys in view, or `null` for everyone) | `{"labels": {"<pubkey>": {"name", "qualifier"}}}`. Keys left out show your agent's library name (source `agent-directory`) when the key is one of your agents, else their public name. |
| `service/request` | `from` (the asking plugin's id), `service`, `method`, `params` | Any JSON, handed back to the plugin that asked. An error is passed on as its error. |
| `completion/query` | `completion`, `request` (this query's id, for `completion/publish`), `query` (the text after the trigger, up to the caret), `community` (the composer's `wss://` relay), `observation` (`{"text","start","end"}`: the draft and the query's character range in it, as it was when the query was first typed; there is no `revision`, as Buzz checks a pick against the draft itself), `channel`, `thread` (the root the composer replies in, or `null`), `inviteAgents` (`true` in a new session's composer, in a session conversation's own view or thread and in any conversation Me shows, never while editing or in an embedded or media composer unless it asks: it may offer your library agents not yet in the conversation) | `{"items", "status", "retry"}`: at most 50 items `{"label", "detail", "text", "mentions", "preview", "disabled", "exact", "image", "avatar"}`. `image` (an https or `data:image/` URL) or `avatar` (a pubkey, drawn with the person's picture) shows before the label. Picking one replaces the trigger and query with `text` and a space (none after text that is only emoji), or with `mentions` (pubkeys, or `{"pubkey","name"}` to name the chip) as chips that make them recipients; while editing a message they become identity links instead. `preview` is a line under the label; `disabled` is why it cannot be picked; a space typed after the query picks the item marked `exact`. `status` shows a line such as "No matches", and `retry: true` adds a row that asks again. |

Answers are kept, but not forever:

| Request | Params | Result |
| --- | --- | --- |
| `message/openThread` | `accessory`, `message` (its id) | `true` after opening the message's thread as the place the accessory was drawn in does (`openThread`), such as a plugin's embedded `message`; `false` where that place cannot. |
| `render/invalidate` | `renderer` (one of yours, or an agent type's id for its summary and peek), and optionally `target` (as `render/event` gives it) or `message` (a message id) | How many answers it marked. Buzz asks again for each the next time it draws, and shows the old answer until the new one comes, as a live buzz-app component updates in place. |
| `completion/publish` | `request` (from `completion/query`), `items`, `status`, `retry`, or `withdraw: true` | `true` when the query is still kept; its suggestions are replaced, such as results after a first "Searching…" answer (`publish`), or with `withdraw` taken back so nothing shows (`publish`'s disposer). |
| `completion/invalidate` | `completion` | `null`. Every query is asked again the next time it is typed. |

A request that fails or times out draws nothing and is asked again 30
seconds later; an answer of `null` declines until it is invalidated. A
completion query that fails shows "Suggestions are not available." with a
Retry row instead, and is asked again on Retry, `completion/invalidate` or a
restart.

A renderer's tree can use every element but `input`. Its buttons and switches
send `render/event` with `renderer`, `target` (what it drew: the URL, the
attachment's URL, for a message `<message id>:<revision>`, or for an
inline piece `<message id>/<text>`, `<message id>/reaction/<text>` in a
quick reaction), `message` (the message id, for message renderers,
accessories and inline renderers), `id`, `event` and `value`.

### Other notifications from Buzz

| Notification | Params |
| --- | --- |
| `message/action` | `action`, `message`, `community` (its `wss://` relay) |
| `composer/tool` | `tool`, `community` (the composer's `wss://` relay, which may not be the selected one), `channel`, `thread` (the root it replies in, or `null`), `composer` (an id for `composer/insert`), `inviteAgents` (as in `completion/query`) |
| `shortcut` | `shortcut` |
| `render/event` | `renderer`, `target`, `id`, `event`, `value` |

### Notifications from the plugin

| Notification | Params | Effect |
| --- | --- | --- |
| `message/markers` | `action`, `messages` (event ids) | Those messages show the action's icon after the byline; the list replaces the last one. |
| `page/badge` | `page`, `text` (up to 8 characters, or `null` to clear), `tone` | A badge on the page's row. |
| `shortcut/enable` | `shortcut`, `enabled` | Turns one of the plugin's shortcuts off or back on for now, as buzz-app's `when` does. |
| `settings/visibility` | `card`, `visibility` (`visible`, `hidden`, `pending`, or `error` when the check failed and opening the section should offer a retry) | Lists or hides one of the plugin's Settings sections, such as one only some communities need. Sections start visible. Each time Settings shows its sections, Buzz sends `settings/check` with the plugin's `cards`, so it can check them again (`visibility.ensure`). |

### 2. `view/open` (notification from Buzz)

Each open page or panel is a view with its own id. Buzz tells the plugin when
one opens:

```json
{"jsonrpc":"2.0","method":"view/open","params":{
  "view":"v3","contribution":"forecast","kind":"panel",
  "target":"https://weather.example/today","channel":"<channel id>","thread":null,
  "root":"<thread root id>"
}}
```

For a page, `kind` is `page`, and `target` and `channel` are `null`. A panel
opened from the channel header has the channel id as its `target`. `thread`
is the thread showing beside the conversation when the panel opened, and
`root` the thread of the message whose link opened the panel
(`context.rootId`), or `null`. A panel beside a page (an `appLauncher`'s,
or one opened outside a conversation) has no `channel`, `thread` or
`root`, and none of `community`, `viewer`, `scope` or `channelName`. A panel's
`view/open` also carries the rest of `ChannelPanelContext`: `community` (its
relay, which may not be the selected community), `viewer`, `scope` (the relay and you, `relay#pubkey`) and `channelName`.

### 3. `view/render` (notification from the plugin)

Send a view's tree whenever it changes. The first one replaces the loading
spinner.

```json
{"jsonrpc":"2.0","method":"view/render","params":{"view":"v3","tree":{
  "type":"column","children":[
    {"type":"text","text":"Sunny","style":"title"},
    {"type":"button","id":"refresh","label":"Refresh"}
  ]
}}}
```

### 4. `view/event` (notification from Buzz)

Clicks, toggles and typing come back as events:

```json
{"jsonrpc":"2.0","method":"view/event","params":{"view":"v3","id":"refresh","event":"click"}}
```

| Element | `event` | `value` |
| --- | --- | --- |
| `button` | `click` | none |
| `switch`, `checkbox` | `toggle` | `true` or `false` |
| `choice`, `menu`, `select`, `tabs` | `change` |
| `collapsible` | `toggle` | `true` to open, `false` to close | the chosen option's `value` |
| `input` | `change` | the field's text |
| `input` | `submit` (Enter) | the field's text |
| `textarea` | `change` | the field's text |
| `composer` | `sent` | `{"id"}` of the message it sent |
| `composer` | `submit` (with `submit: "plugin"`) | the draft `{"content","mentions","references","attachments"}` |
| `composer` | `ready` | `{"composer"}`, for `composer/insert` |
| `message` | `openThread` (its replies summary) | `{"message","root","channel"}` |
| `message` | `reply` (Reply in its action bar) | `{"message","root","channel"}`: open its thread, or embed a composer for it |
| `message` | `edit` (Edit message) | `{"message","root","channel","content"}` |

### 5. `view/close` (notification from Buzz)

The view is gone. Forget its state.

When the plugin restarts, Buzz sends `initialize` again, then `view/open` for
every view that is still open. Until a new tree arrives, the old one stays on
screen, and each composer element in it sends `ready` again with a new id; a
draft the old run took with `submit` is handed back unsent.
Composer tools and accessories are made anew: their views, tool popovers and
captures begun through a tool close without `view/close`, and each accessory
opens again as a new view (a new id and `view/open`) when its composer is
next mounted: at once for a composer that is shown or kept out of sight (an
unselected conversation or thread tab, closed comments), when its tab is next
selected for one inside a plugin panel tab not selected, and when you return
for one on a hidden page. A
reaction picker opens again while its tool still picks reactions, and tool
states start over.

## Elements

Every element is an object with a `type`. Unknown types draw nothing, so a
plugin written for a newer Buzz still shows what this one understands. A tree
can hold at most 5,000 elements, nested at most 32 deep.

| `type` | Fields |
| --- | --- |
| `column` | `children`, `gap` (`none`, `sm` (default), `md`, `lg`), `align` (`start`, `center`, `end` or `stretch` (default): across the column), `justify` (as a row's, down the column), `grow` (takes the height left in its parent, such as to centre something on a page), `padding` (`none` (default), `sm`, `md`, `lg`), `background` (`inset`, as a card's header, or `panel`) |
| `row` | `children`, `gap`, `justify` (`start`, `center`, `end`, `between`), `wrap`, `grow` (takes the width left), `padding`, `background` |
| `header` | `title`, `icon`, `subtitle` (a caption after the title, such as the channel's name), `children` (its actions, such as buttons): a page or panel's title bar, as Buzz's own pages have. Put it first in a `workspace` page or panel. |
| `card` | `title` (optional), `children`, `padding` (default `md`), `gap` (default `sm`) |
| `text` | `text`, `style` (`body`, `small`, `large`, `label`, `title`, `heading`, `code`), `tone` (`default`, `subtle`, `accent`, `success`, `warning`, `danger`, and `complete` or `progress` for a task's status), `truncate` (one line, cut with an ellipsis), `tooltip` (shown on hover, such as a time's full date) |
| `markdown` | `text`, rendered as a Buzz message is |
| `button` | `id`, `label`, `variant` (`prominent`, `subtle`, `ghost`, `outline`, `destructive`), `icon`, `disabled`, `loading`, `size` (`sm` for a small one), `tooltip`, `fill` (takes the full width of its column). With an empty `label` and an `icon`, an icon button. |
| `link` | `label`, `url`, `external`. Opens like a link in a message: a panel that matches it opens; otherwise the browser does. With `external: true` it always opens in the browser. |
| `input` | `id`, `value`, `placeholder`, `version`, `clearOnSubmit`, `disabled`, `grow` (takes the width left in its row) |
| `textarea` | `id`, `value`, `placeholder`, `version`, `rows` (lines it shows at least, default 3), `maxRows` (lines it grows to, default 8), `disabled`: text of several lines, kept as an `input`'s is |
| `switch` | `id`, `checked`, `label` |
| `checkbox` | `id`, `checked`, `label`, `indeterminate` (drawn part-way, as a task in progress), `completed` (the label struck through and muted), `disabled`; sends `toggle` like a switch |
| `choice` | `id`, `value`, `options` (`[{"value","label"}]`, up to 50), drawn as radio buttons; sends `change` with the chosen `value` |
| `progress` | `value`, from 0 to 1 |
| `image` | `url` (https, or the community's media), `width` and `height` (pixels, up to 1200), `alt` |
| `audio` | `url` (the community's media, or a draft's local URL from `attachment/render`), and optionally `name`, `mime`, `duration` (seconds), `waveform` (0 to 255 per bar) and `voice` (draw the voice note card) | Buzz's own audio player, so an attachment renderer can play what it draws (`AttachmentRendererProps.source`). |
| `composer` | `id`, `channel` (or empty, with `submit: "plugin"`, for a draft not in a conversation yet, such as a new conversation's first message; it then takes no attachments and keeps its draft under `draftKey`), `thread` (optional root), `parent` (optional: the reply it answers in the thread), `placeholder`, `disabled`, `autoFocus`, `draft` (text it starts with, put back whenever it changes), `mentions` (people it starts with as chips, `[{"pubkey","name"}]`), `inviteAgents` (offer your library agents not yet in the conversation), `submit`: Buzz's own composer (only in views, not renderers). Once made it sends `ready` with `{"composer"}`, an id for `composer/insert` and `composer/insertMentions`. It sends as the channel's does, then sends `sent` with the message `{"id"}`. With `submit: "plugin"` it sends nothing: the plugin gets `submit` with the draft `{"content","mentions","references","attachments"}` (attachments already uploaded) and sends it itself, such as once the channel it needs exists. The draft stays, locked, until the plugin calls `composer/settle`. |
| `message` | `id`, `channel`, `message` (an event id), and optionally `layout` (`timeline`, `thread` or `nested`: as a conversation, a thread panel or a nested reply draws it), `compact` (without the avatar and byline, as a follow-up) and `actions` (`false` for no action bar): one message as Buzz draws it in a conversation, with its byline, attachments and reactions, kept up to date (only in views). It has the conversation's own action bar: reactions and the picker, copying, read state, follows, delete, report and plugins' message actions all work as in a channel, and a failed send of yours retries from it. Replying, editing and opening its thread go to the plugin, which decides where they happen: see the events below. |
| `thread` | `id`, `channel`, `root`, `height` (pixels, default 480): Buzz's own thread view; closing it sends `close` (only in views) |
| `avatar` | `pubkey`, `size` (`small`, `default` or `large`): their profile picture, or their initial. With `url` (https) and `name` in place of `pubkey`, a picture from the web with the name's initial behind it, such as a GitHub user's. |
| `badge` | `text`, `tone` |
| `icon` | `name`, `size` (pixels, 8 to 128, default 16), `tone` |
| `menu` | `id`, `options` (`[{"value","label","icon","iconTone","avatar","disabled"}]`, up to 200; `avatar` is a pubkey whose picture leads the row), `value` (the option shown chosen), `title` (the menu's heading), `label` for a button that says so, or `icon` and `iconTone`, or `avatar`, for a small one, `tooltip`, `disabled`: a button that opens a menu; choosing sends `change` with the option's `value`. It opens only in views. |
| `select` | `id`, `options` (as a `menu`'s), `value`, `placeholder`, `disabled`, `width` (pixels; it fills its place by default): a field that opens the list; choosing sends `change` with the option's `value`. It opens only in views. |
| `popover` | `id`, `icon` (default `Info`), `tooltip`, `title`, `description`: a small button that shows a note under it (only in views) |
| `empty` | `title`, `icon`, `description`: an empty list's placeholder, as Buzz's own (`EmptyState`) |
| `divider` | none |
| `spinner` | none |
| `code` | `lines`, each `{"text","tone"}` or `{"cells"}`, a row of `{"text","tone","kind"}`: `kind` `text` (the default, taking the width left), `number` (a line number in a gutter) or `omitted` (a gutter with no line, as a split diff's empty side). `tone` is `context` (the default), `added`, `removed`, `hunk` or `meta`. `wrap` (default `true`: long lines wrap; `false` scrolls sideways), `minWidth` (pixels, up to 2400: narrower, it scrolls sideways). Monospace, as Buzz draws a diff. |
| `document` | `text` (GitHub-flavoured Markdown), `html` (optional: the HTML GitHub rendered for it, which tells its media apart), `base` (the page its links resolve against): a body as GitHub shows an issue, pull request or comment, with single newlines as breaks, raw HTML reduced to its text, images, players for GitHub's uploads, and every link kept to the web and opened in the browser (buzz-app's `GitHubBody`). `document/preview` summarises one. |
| `collapsible` | `id`, `open`, `marker` (elements in the gutter, such as an avatar or icon), `markerTooltip`, `header` (elements, such as author and time), `preview` (one line shown while closed), `fit` (text: when it fits the header's width on one line, it does not open and shows `children` in place of the preview), `expandable` (default `true`), `children` (the body while open), `below` (under the header while closed, such as `thumbnails`), `actions` (at the header's end, shown on hover), `footer` (under the body while open): a message that opens to read. Clicking its marker or header sends `toggle` with the state wanted; set `open` from it. |
| `thumbnails` | `images` (https URLs), `label`: square thumbnails, as many as fit the width, the last counting the rest; each opens the media viewer |
| `tabs` | `id`, `options` (`[{"value","label"}]`), `value`: tabs over a view's content; choosing one sends `change` with its `value` |
| `ring` | `segments` (`[{"value","tone"}]`), `size` (pixels, 16 to 128, default 32): coloured arcs, one per segment by its share, from the top, such as checks by outcome |
| `scroll` | `children`, `maxHeight` (pixels), `padding`: scrolls once taller than `maxHeight`; a wheel it cannot use scrolls the page |

Buzz keeps what is typed in an `input`, so typing never waits on the plugin.
`value` is the text a field starts with. To replace the text later, send a
new `value` with a higher `version`. Set `clearOnSubmit` to empty the field
when Enter sends it.

## Asking Buzz for things

A plugin can send requests to Buzz. Use your own ids; Buzz answers each one.

| Method | Params | Result |
| --- | --- | --- |
| `ui/toast` | `title`, `body` (optional), `tone` (`info`, `success`, `warning`, `error`) | `null` |
| `ui/openUrl` | `url` (http or https), opened in the browser | `null` |
| `community/current` | none | `{"name","relay","viewer","status","error","live","cached","restoring","generation","scope","liveStatus"}` for the selected community, or `null`. `viewer` is your pubkey; `status` is `connecting`, `ready` or `error` (with `error`), and with no community selected the answer is `null` (buzz-app's `disconnected`); `live` says the live connection is up; `cached` says it shows saved data while the relay has not answered. `generation` changes with each retry and cache clear, so state kept for a session can start over; `scope` is the relay and you (`relay#pubkey`); `restoring` says the saved copy is still being read from disk, before anything shows. `liveStatus` is `session.live`: `{"status","error","retryInMs","routes","roster","rosterError","heads","issue"}`, with each route `{"id","channel","status","replay","error"}`, each channel catch-up `{"channel","state","error"}`, and `issue` the first problem worth showing. Buzz sends the notification `community/changed` with the same fields whenever any of them change, such as when another community is selected, or `{"status":"none"}` when none is. |
| `community/refresh` | `what`: `unread`, `inbox`, `reminders`, `emoji`, `hiddenDms` (the DMs you hid, `channelLifecycle.refreshVisibility`), `sidebar`, `channels` (the channel list, `channels.refreshList`), `roster` (read fresh from the relay's primary, `refreshRoster`), `agents` (your agent list, read again even once read, and the identity archive unless it was never read: `agentChoices.refresh`) or `kit` | `null`. Reads that view again, as its Retry does after an error. |
| `community/list` | none | Your communities: `[{"name","relay","icon","selected"}]`. Buzz sends `communities/changed` with `communities`, the same list, when you join or leave one or select another. |
| `community/client` | none | `communityReader.snapshot()`: `{"status","viewer","error","profile","communities","relayAvailable"}` (`relayAvailable` is always `true` here). `status` is `loading`, `ready` or `unavailable` (no identity yet, or one that could not be read, with `error`); `profile` is the personal profile new communities start from, `{"name","picture","about"}`; `communities` is `community/list`. Works in Personal space. Buzz sends `client/changed` with the same fields when they change. |
| `community/retry` | none | `null`. Connects to the selected community again after a failure; on a connected one, retries live updates and read sync (see How this differs from buzz-app). |
| `community/clearCache` | none | `null`, after forgetting the conversations Buzz saved of the community, here and on disk, as Clear cache in Settings does. Open channels load again. |
| `community/channels` | none | The selected community's channels: `[{"id","name","description","private","visibility","archived","kind","typeKnown","members","participants","labels","lastActivityAt","updatedAt","preview","readOnly","cached","hidden","creator","createdAt","parent"}]`, where `kind` is `stream`, `forum`, `dm` or `session`, and `visibility` is `public`, `private` or `null` while the relay has not said (`private` is then `false`) |
| `channels/list` | none | `{"status","error","cached","channels","hiddenDms","hiddenStatus","hiddenError"}`: the channels as `community/channels` gives them, with how far the list got (`loading`, `ready` or `error`), whether it is this device's saved copy, and the DMs you hid with how far that read got. |
| `channels/searchPublic` | `query`, `exact` (optional), `limit` (optional, default 8, at most 100) | `{"channels","partial"}`: open channels you have not joined whose name has `query` (or is it), as read-only previews; `partial` when the relay's page was full. An error until the channel list is `ready`. `channel/messages`, `channel/watch` and the thread reads work on them too. |
| `channels/findLabeled` | `creator` (lowercase hex), `label` | The channels `creator` made with NIP-CL `label`, oldest first; an error on a relay without labels. |
| `channels/resolve` | `ids` (at most 128 not yet resolved) | The channels you may read among `ids`, joined or not (`channels.get`, `resolve`, `describe`). |
| `channels/members` | `channel` (yours, or an open one you have not joined) | `{"channel","version","canManage","members","operation"}`: each member `{"pubkey","role","canManage","canRemove"}`, read fresh from the relay-signed roster. `operation` is the latest role change or removal here, `{"pubkey","change","status","error"}` with `status` `pending`, `uncertain`, `confirmed` or `failed`, or `null` (`administration.snapshot().operation`); the read settles an `uncertain` one the roster now shows, so the next change may go. `role` is `owner`, `admin`, `member`, `guest`, `bot` or `unknown`, the `expected` that `channels/changeMember` takes; `canRemove` also covers your own agents. |
| `channels/lifecycle` | `channel` | `{"channel","channelType","canArchive","canUnarchive","canDelete","canLeave","canHide","leaveReason","deleteUnavailable"}`: what you may do to the channel as a whole (`channelLifecycle.load`), read fresh and including what an owner agent of yours grants, with its `channelType` (`stream`, `forum` or `dm`). |
| `session/watch` | `what`: `channels`, `channelList`, `sidebar`, `me`, `sync`, `creation`, `archives`, `agentChoices`, `statuses` (with `pubkeys`, at most 500, followed as `statuses.watch` does), `presence`, `emoji`, `profiles` (with `pubkeys`, at most 500, read if not loaded), or `typing` (with a `channel`, and a `root` for one thread) or `activity` with a `channel` | `{"watch"}`. Buzz then sends `session/changed` with `what` and `value` (as `community/channels`, `channels/list`, `sidebar/get`, `me/get`, `unread/sync`, `channels/creation`, `agents/choices`'s `archives`, `agents/choices`, `statuses/get`, `presence/get`, `emoji/catalog`, `profiles/get`, `typing/get` with `channel` and `anywhere` or a thread's `thread`, or `threads/activity`), now and whenever it changes. Stop with `session/unwatch`. |
| `channel/messages` | `channel`, `limit` (default 50, at most 200), `membership` (optional: include join, leave and removal rows, each with `membership` `{"type","actor","target"}`) | `{"channel","messages","hasMore","status","loadingOlder","historyLimited","restored","stale","error"}`, the newest messages, oldest first (`ChannelWindow`). `status` is `loading`, `ready` or `error`; `loadingOlder` says older history is on its way; `historyLimited` that the relay keeps no more; `restored` that the rows came from this device. `stale` is true when the relay has not yet confirmed rows saved on this device. For older history, use `events/query` with `until`. |
| `thread/messages` | `channel`, `root` (or `message`, any message in the thread), `exact` (optional: fold the thread around `message` on its own evidence, as a link to one reply does) | `{"channel","root","replies","hasMore","limited","status","direction","readKind","target","targetStatus","error"}`: `status` is `idle`, `loading`, `ready` or `error`; `direction` how the loaded range grows (`older`, newest first, or `forward`, oldest first); `readKind` the read in flight or that last failed (`older` or `refresh`); `limited` when more exists than a view holds; with `exact`, `target` is that message and `targetStatus` `loading`, `ready`, `unavailable` or `error`. A read that failed answers `status` `error` with its `error` and whatever is known. |
| `channel/watch` | `channel`, `limit`, `membership` | `{"watch","channel"}`. Buzz then sends `channel/changed` (below). `watch/more` loads older history into it. |
| `thread/watch` | as `thread/messages` | `{"watch","channel","root","message"}`, `root` or `message` as given. Buzz then sends `thread/changed` with what `thread/messages` gives, now and whenever the thread changes. `watch/more` loads more of it. |
| `watch/retarget` | `watch` (a thread watch), `message` | How moving the watch to show `message` of the same thread as its `target` went (`ThreadView.retarget`): `selected`, `different` (it belongs to another thread: watch that one), `unavailable`, `error` or `cancelled` (a later retarget, or the watch ended). `thread/changed` shows the new `target`. |
| `watch/more` | `watch` (a channel or thread watch) | `true` when it asked for more: older messages for a channel watch, whose `limit` also grows by 50 (to 1,000), or the rest of a thread (`loadMore`, `loadOlder`); `false` when there is nothing more to load. |
| `watch/refresh` | `watch` (a channel or thread watch) | `true` at once, having asked to read it again, as its Retry does after an error (`refresh`); the watch's next notification shows the outcome; `false` for other watches. |
| `events/watch` | `filters` (as `events/query` takes, without the read extensions) | `{"watch"}`. Buzz then sends `events/live` with `events`, the live events that match, from now on, and `channel`, the channel whose live route brought them (`null` for others). |
| `channel/unwatch`, `thread/unwatch`, `events/unwatch` | `watch` | `null`. A plugin holds at most 64 watches of every kind in all. |
| `events/query` | `filters`: 1 to 10 NIP-01 filters of `ids`, `authors`, `kinds`, `#<letter>` tags, `since`, `until` and `limit`, plus the relay's read extensions: `search` with `search_mode` (`prefix` or `fulltext`), `page`, `feed_types`, `top_level`, `include_aux`, `include_summaries`, `before_id`, `thread_window`, `depth_limit`, `thread_cursor` and `thread_cursor_id`, and `consistency: "strong"` | The matching signed events, at most 500, read from the community as you; events whose signature does not check out are left out. Your own events the outbox holds that match are included, those still on their way and those it saw delivered, each with its `delivery` and `error`, as `session.read` merges them. |
| `community/info` | none | The community relay's NIP-11 document. Its `self` key signs what the relay publishes, such as rosters and the archive. |
| `contributions/list` | none | What Buzz and its plugins added, each by its `plugin/id` `key`: `pages` (`title`, `placement`, `routes`: the route version or `null`, `icon`, `layout`, `companion`, `handlesNavigation`, `primary`), `panels` (`title`, `order`, `icon`, `launcher` `{"label","placement","target"}`, `appLauncher` `{"label","target"}` and `menu` `{"label"}`, each `null` when absent), `settings` (`title`, `visibility`: `visible`, `hidden`, `pending` or `error`), `accountActions` (`title`), `shortcuts` (plugins', Buzz's built-in plugins' included: `pluginId`, `plugin` (its name), `title`, `chords` as shown, `enabled`), `composerTools` (`order`, `reactionComponent`: whether it also picks reactions) and `completions` (`order`, `trigger`), Buzz's own mentions, emoji and voice note tools, `@`, `#` and `:` completions and link chips included, and only plugins that are on, `accessories` (`placement`), `inlineRenderers`, `messageRenderers`, `linkRenderers`, `attachmentRenderers` and `messageActions` (with `title`). Every entry also has its `title` (for a renderer that declares none, its `id`) and, where it has one, its `order`. Buzz sends `contributions/changed` with the same fields when any of them changes. |
| `contributions/update` | everything `initialize` answers (pages, panels, renderers, tools, agent types and the rest) | `null`. Replaces what the plugin contributes without restarting it: what is new is added, what is gone is taken away, and the rest stays, open views included. buzz-app plugins register and dispose contributions at any time; this is how a plugin here adds a page once it learns it needs one. |
| `shortcuts/run` | `key` (a plugin shortcut's) | `true` when it ran, as its chord would (`RegisteredShortcut.run`), such as from a command palette |
| `plugins/list` | none | Every plugin, built in or added: `[{"id","name","builtIn","status","active","run"}]`, `status` being `running`, `starting`, `off`, `paused` or `failed` (`pluginStatus`), and `run` changing with each start of an added plugin, so a restart shows even when its status ends where it was. Buzz sends `plugins/changed` with the same list when one changes. |
| `profiles/get` | `pubkeys` (hex, at most 500) | `[{"pubkey","name","picture","about","nip05","isAgent","owner","loaded","nameIsFallback"}]`; `owner` is the owner an agent's `auth` tag names, as buzz-app's `ownerPubkey`: for display, not verified, so never for authorization, and `nameIsFallback` says `name` is the key's prefix for want of one |
| `messages/send` | `channel`, `content`, `thread` (optional `{"root","parent"}`), `mentions` (optional pubkeys, notified), `references` (optional pubkeys, shown but not notified), `picks` (optional `{pubkey: {"model","effort"}}` for mentioned agents), `attachments` (optional, from `files/upload`; add `voice: {"duration"}` in seconds to send one as a voice note) | `{"id"}`, once the message is queued: follow it with `outbox/get` or `outbox/watch`. `id` is `null` if Buzz changed the text on the way, such as by adding attachment links. Needs `send-messages`. |
| `messages/edit` | `channel`, `message`, `content` | `null` once sent. Only your own messages. Needs `send-messages`. |
| `messages/react` | `channel`, `message`, `emoji`, and `emojiUrl` (optional: an https picture for a custom emoji written `:shortcode:`, such as one on another reaction, whether or not the community has it) | `null` once sent. Needs `send-messages`. |
| `messages/delete` | `channel`, `message` (a message or reaction), or `messages` (1 to 100 ids) | `null` once sent; with `messages`, `{"deleted","failed"}` (each failure by id). Your own, or an agent's you own. Needs `send-messages`. |
| `messages/sendToChannel` | `channel`, `message` (one of your thread replies) | `null`. Copies it into the channel as a new message that names its thread ("Also send to channel"). Needs `send-messages`. |
| `messages/report` | `channel`, `message`, `reason` (`spam`, `profanity`, `nudity`, `impersonation`, `malware`, `illegal` or `other`; required), `note` (optional, sent whole after trimming) | `null` once the relay accepts it. Needs `send-messages`. |
| `messages/retry`, `messages/dismiss` | `message` (one of yours whose `delivery` is `failed`) | `null`. Sends it again, or drops it. Needs `send-messages`. |
| `files/upload` | `path` (a file, up to the community's limit for its type: 50 MiB for images but 10 MiB for GIFs, 500 MiB for video, 100 MiB otherwise) or `data` (base64, up to 3 MiB); `name` and `mime` (optional; Buzz guesses); `progress` (optional: a name for `files/progress`) | `{"name","url","mime","size","sha256"}`, for `messages/send`'s `attachments`. With `progress`, Buzz sends `files/progress` (`progress`, `stage`: `preparing`, `uploading` or `sending` with `sent` and `total` bytes) as it goes. Needs `send-messages`. |
| `events/publish` | `kind`, `content`, `tags` | `{"id","event"}` once the relay accepts it. Buzz signs as you, only kinds it writes itself (such as 7, 9, 30315 and 40003), plus app data (30078) and the agent and team catalogs (30175, 30178), as buzz-app does. Needs `publish-events`. |
| `ui/navigate` | `to`: `home`, `me`, `agents`, `settings` (with an optional `section`, such as `appearance`, `plugins` or a settings card's `plugin/id`, including buzz-app's `buzz.emoji/custom-emoji` (while Emoji is on) and `buzz.pairing/mobile`), `page` (with `page`, a page's `plugin/id`, and optional `params` and `version` for a page with `routes`), `conversation` (with `channel`, and optional `message`, `thread` and `members`), `back` or `forward`; optionally `community` (a joined community's `wss://` relay, or `null` for Personal space; the visit switches as it presents, Back and Forward included); `replace: true` to take the current visit's place in history, such as to redirect a route; and `focus: "preserve"` to leave the focus where it is, for a control that keeps it | Once the visit settles (`OpenResult`): `{"status":"opened"}`, `{"status":"failed","reason"}` with `navigation/get`'s reasons, or `{"status":"superseded"}` when another visit started first. A target that cannot be opened is still a visit, as buzz-app pushes a target before presenting it, and fails as it presents: a community you have not joined with `{"status":"failed","reason":"denied"}`, and an unknown page or settings section, or a route the page does not take (or no longer takes, at its `version`), with `unavailable`. A malformed target, such as a page key that is not `plugin/page`, a section that is not a section name or `plugin/card`, a `version` that is not a positive whole number, or `params` past 8 KiB, 8 levels or 1024 values, answers `invalid-target` without changing the window or `navigation/get` (`parseOpenTarget`); one the window already shows answers `opened`. Buzz has no `cancelled` outcome: a visit is replaced, never withdrawn. Every attempt, Retry, Back and Forward included, checks its place anew: a page or card of a plugin still starting is waited for, as is a settings card still checking whether it shows, a hidden one is asked about again (`settings/check`) and one whose check failed is checked again once; one that is hidden or failed its check fails the visit as `unavailable`, and one still waiting or opening when the attempt's open timeout (15 seconds, waiting included) runs out as `timeout`. A visit begun meanwhile, the user's own included, supersedes one still waiting or opening. A failed card is checked again once per attempt. `focus` holds for the whole attempt and for its Retry. Back and Forward answer `null` at once. A `conversation` may name a channel not in your list, such as an open one you have not joined; it opens if the relay has it. Buzz's own pages take route version 1 as in buzz-app: Messages (`buzz.channels/channels`) `"new-message"`, `"empty"` or `{"kind":"new-session","parentId"}`, which `navigation/get` reports back in that form, and Me (`buzz.me/me`) `"new"`, `"new:<section>"` or a conversation's id; picking a conversation in Me is a visit of its own, reported in that form. Messages' legacy version-1 params `"Inbox"` and `"Bestie"` open those pages. |
| `navigation/get` | none | `{"status","entry","attempt","place","failed","canGoBack","canGoForward"}`: `attempt` is `{"id","focus"}`, new for every attempt (a repeat visit or Retry included); `place` is where the window is, in the form `ui/navigate` takes (with `community`), or `null` before anything shows. `status` is `opening` while a conversation is still looked up, `opened`, or `failed` when the visit could not open; `failed` then says which place (for one Buzz had no place for, the `ui/navigate` request that named it, as `place` also is while that visit shows), why (`OpenFailure`: `denied`, `invalid-target`, `not-found`, `timeout`, `host-error` or `unavailable`), whether it came from outside the window (`ingress`), and whether `navigation/retry` may help. `entry` changes with each visit. |
| `navigation/watch` | none | `{"watch"}`. Buzz then sends `navigation/changed` with what `navigation/get` gives, now and whenever it changes. Stop with `navigation/unwatch`. |
| `navigation/retry` | none | Opens the visit that failed again as a new attempt, checking its place anew, and answers how it turned out, as `ui/navigate` does. A visit that failed for a page or settings section Buzz had then is retried from its request, so a page added since opens. |
| `page/settle` | `view` (an open page), `status`: `failed` or `opened`, `reason` (optional with `failed`: `not-found`, `denied`, `invalid-target`, `timeout`, `host-error`; `unavailable` by default) | With `failed`, `true` when the visit waiting on this page (`handlesNavigation`) failed with that reason, kept for Retry navigation, as buzz-app's `PageNavigation.complete({status: "failed", reason})` does; `false` otherwise: a page without `handlesNavigation` opened as it showed, and a page left behind cannot fail another visit. With `opened`, `true` when the visit was waiting on this page (`handlesNavigation`). |
| `page/resolve` | `view` (an open page), `params` (a route) | `true`, after the visit waiting on this page shows `params` without another visit (`PageNavigation.resolve`); `false` when the visit does not wait on it. |
| `format/time` | `at` (unix seconds), `now` (optional unix seconds that "today" and "ago" count from; the clock by default), `style`: `byline` ("Yesterday at 2:34 PM"), `item` (a list row: "2:34 PM", "Yesterday", "Sat, Jun 20"), `day` (a day divider, "Monday"), `full` (a byline's hover text) or `relative` ("5 minutes ago") | The text, as Buzz words times |
| `ui/openPanel` | `target` (what it shows, such as a link or any text the plugin reads from `view/open`), and optionally `panel` (one of the plugin's panel ids; without it, whichever panel of any plugin shows `target`, as `panel/resolve` picks), `channel` and `thread` (the conversation it opens for, such as an accessory's) | `true` when a panel opens: as a tab of the conversation, or beside a page. |
| `panel/canOpen` | `target` | Whether any panel, the plugin's or not, shows `target`. |
| `panel/resolve` | `target` | The panel that would show `target` (`panels.resolve`): `{"key","title","order"}`, `key` being `plugin/panel`, or `null` when none does. |
| `panel/open` | `view` (an open panel), `target` | `true` if a panel shows `target`, and `false` from a panel tab that is not the one shown or while the community's relay is not ready (`panelActive`). When it is this panel, the view shows it in place and the plugin hears `view/retarget` (`view`, `target`, `canGoBack`); otherwise the panel that shows it opens as a tab for the same conversation and thread (a tab already showing it is shown as it is), and this one closes. |
| `panel/push` | `view` (an open panel), `target` | As `panel/open`, but keeps this panel: within it, Buzz shows a Back button that returns to the previous target with another `view/retarget`; another panel opens beside it, for the same conversation but no thread. |
| `ui/openLink` | `url` (`buzz://` or a web link) | `null`. Opens it as a click in a message does: in Buzz, in a panel that shows it, or in the browser. |
| `names/get` | `pubkeys` (at most 500), `candidates` (optional: the keys in view, such as a channel's members, that namesakes are told apart among; everyone when left out), `displayFacts` (optional, at most 500 `{"pubkey","name","isAgent","owner"}`: names this view shows, such as a draft's recipients, that the naming policy weighs in place of those keys' profiles) | `{"<pubkey>": {"name","qualifier","source"}}`: names as Buzz shows them, through the naming policy, with the npub suffix that tells namesakes apart. `source` is `agent-directory` when the naming policy named the key, `public-profile` for a profile name, or `null` when the key's start shows. |
| `names/watch` | as `names/get` | `{"watch","names"}`. Buzz then sends `names/changed` with `names` whenever one changes: a profile loads, an agent is renamed, or the naming policy answers or changes. Stop with `names/unwatch`. |
| `ui/openConversation` | `channel`, `message` (optional), `thread` (optional), `members` (optional, `true` to open its members) | `null`. Buzz shows the conversation. |
| `presence/get` | `pubkeys` (at most 500) | `{"<pubkey>": "online" | "away" | "offline" | null}`, for people Buzz already follows; `null` for others |
| `presence/watch` | `pubkeys` (at most 64) | `{"watch"}`. Buzz follows their presence, shown or not, and sends `presence/changed` with `presence` (as `presence/get` gives it) now and whenever it changes. Stop with `presence/unwatch`. |
| `media/get` | `url` (the community's media, or a draft's local `url` from `attachment/render`), and optionally `size: "small"` (the relay's thumbnail, as avatars use) or `directory` (with `name`) | `{"data","size","mime"}`: the file as base64, at most 8 MiB, fetched as you; with `directory`, `{"path"}` of the file saved there, any size. Other URLs, fetch yourself. |
| `attachment/remove` | `url` (a draft `attachment/render` called `removable`) | `true` when the draft was removed from its composer |
| `media/url` | `url` (the community's media), `size` (optional, `small`) | A loopback `http://127.0.0.1` URL to fetch or stream it from, any size, with ranges (`session.media`), or `null` when Buzz cannot serve it so |
| `sidebar/get` | none | `{"status","error","loaded","cached","writable","starWritable","sortWritable","muteWritable","removeGroupWritable","starred","muted","sort","groups","assignments","sortErrors","moves","hiddenDms","meChannels"}`: your starred and muted channels, each group's sort, your saved groups `{"id","name","icon","order"}`, which group each channel is in, what can be saved now, sorts that failed `{"group","mode","error"}`, moves still saving or failed `{"channel","to","pending","error"}`, and the DMs and Me conversations the sidebar leaves out |
| `sidebar/retryMove` | `channel` | `null`, after trying a failed move again. Needs `manage-channels`. |
| `sidebar/dismiss` | `channel` (a failed move) or `group` (a failed sort) | `null`. Forgets the failure. |
| `me/get` | none | Your Me groups (`mePreferences`), as `sidebar/get` gives the Messages sidebar: `groups`, `assignments` (conversation → group), `moves`, `status`, `writable` and the rest. |
| `me/assign` | `channel`, and `group` (a group id, or `null` for Conversations) or `newGroup` (a name, to create one) | `null` once saved. Needs `manage-channels`. |
| `me/retryMove`, `me/removeGroup` | `channel`; `group` | `null`. Tries a failed move again; deletes a group, whose conversations go back to Conversations. Need `manage-channels`. |
| `me/place` | `channel` (any channel id, such as a session just made or one you left), `me` (default `true`), `group` (optional) | `null` once saved. Puts a conversation in Me, in `group` if given, or takes it out with `me: false` (`mePlacement.set`). Needs `manage-channels`. |
| `me/admit` | `channel` (any channel id, seen here or not), `group` (optional) | `null` when a new conversation would fit in Me, there or in `group`, or an error saying why not (`mePlacement.admit`). |
| `me/dismiss`, `me/refresh` | `channel`; none | `null`. Forgets a failed move; reads the groups again. |
| `sidebar/star`, `sidebar/mute` | `channel`, and `starred` or `muted` (`true` or `false`) | `null` once saved. Unstarring returns a channel to its default section. Needs `manage-channels`. |
| `sidebar/sort` | `group` (`starred`, `channels`, `forums`, `dms` or `section:<id>`), `mode` (`alpha` or `recent`) | `null` once saved. Needs `manage-channels`. |
| `sidebar/assign` | `channel`, and `group` (a saved group id), `newGroup` (a name of at most 256 characters, not blank: makes the group), or neither for its default section | `null` once saved. Needs `manage-channels`. |
| `sidebar/removeGroup` | `group` | `null` once saved; its channels return to their default sections. Needs `manage-channels`. |
| `statuses/get` | `pubkeys` (at most 20) | `{"<pubkey>": {"text","emoji","updatedAt","expiresAt"} | null}`, read fresh |
| `statuses/save` | `text`, `emoji` (a unicode emoji or `:shortcode:`), `expiresAt` (optional, whole unix seconds in the future; anything else is refused unless the save clears the status); both empty clears it | `null` once the relay has your status (`statuses.save`); `statuses/get` or the `statuses` session watch reads it again. Needs `publish-events`. |
| `typing/get` | `channel`, `root` (optional, a thread) | The pubkeys typing there |
| `typing/all` | none | `{"channels","threads"}`: who is typing anywhere in the community (`typing.snapshot()`), by channel at its top level and by thread root |
| `emoji/list` | none | The community's custom emoji: `[{"shortcode","url"}]` |
| `emoji/catalog` | none | `{"status","error","entries","mine"}`: the custom emoji with how far the read got (`idle`, `loading`, `ready`, `error`), and your own set |
| `emoji/add` | `shortcode`, `url` (an http or https image, such as one `files/upload` stored) | `{"shortcode"}`, the name as saved, once sent. Adds it to your custom emoji set, or gives it a new image, keeping the rest (`emoji.add`, and `emoji.upload` after an upload). Needs `publish-events`. |
| `people/search` | `query` (empty or left out to browse; at most 100 characters, or 256 with `members`), `page` (from 1, the default), `members` (optional: search as Add member does, yourself and exact keys included) | `{"people","more"}`, people as the New message picker finds them (`directMessages.people`): profiles that match, in the relay's order (a search with `members` ranks them by name), as `profiles/get` gives them |
| `notifications/submit` | `category`, `sourceKey` (unique per thing, up to 1024 characters; a repeat in the same community within two minutes is dropped), `title`, `body`, and what a click opens: any place `ui/navigate` takes (`to` and its fields), or the short forms `channel` with an optional `message` and `thread`, or a `page` (`plugin/id`) | `true` if it showed. Buzz applies the user's notification settings. A malformed place, none at all, or a channel with no community is refused; a well-formed page or settings section Buzz lacks is accepted, and its click fails as the visit presents, as in buzz-app; a conversation answers `false` unless it is in the selected community, that community is connected, and you are a member of the channel. |
| `agent/save` | `view` (an open `agentTab`), `config` (an object, the whole config) | `null` once saved |
| `unread/get` | none | `{"channels", "mentions", "direct", "observed"}`. `channels` maps each channel with something unread to `{"unread","count","attention","mentions","threads","manual","manualKind","latestAt","preview","stale","threadsStale"}`, `manualKind` being `none`, `local-only` (this device's mark) or `remote` (synced from another device). Counts are sampled, not exact. |
| `unread/watch` | none | `{"watch"}`. Buzz then sends `unread/changed` with the same fields, now and whenever they change. Stop with `unread/unwatch`. |
| `unread/watchTargets` | `channel`, `threads` (roots) and `messages` (each up to 500 ids) | `{"watch","channel"}`. Buzz then sends `unread/targetsChanged` with `{"channel","threads","unread","forced"}`, now and whenever it changes: each thread's `{"manual","observed","stale"}` by root (`manual` as `manualKind`; `observed` its unread replies, `null` without evidence), the messages among `messages` that are unread, and those marked unread for this visit (`unread.snapshot` for thread and message targets). Stop with `unread/unwatch`. |
| `unread/target` | `channel`, and optionally `thread` (a root) or `message` | `{"channel","latestMessage","observedCount","attentionCount","mentionCount","coverage","freshness","manual","error"}` (`unread.snapshot(target)`), or `null` before read state loads: `latestMessage` is the channel's newest `{"id","createdAt","preview"}`, counts are `null` without evidence (never a made-up zero), `coverage` is `unknown` or `observed`, `freshness` `unknown`, `observed` or `stale`, and `manual` as `manualKind`. |
| `unread/message` | `channel`, `message` | Whether that loaded message shows as unread now |
| `unread/attention` | `channel`, `message` | That loaded message's attention under the badges' policy (`unread.attention`): `{"status","category","mentioned","root","pending","unread","forced","viewing"}`, `status` being `unknown`, `ineligible` or `eligible` and `category` `mention`, `direct`, `thread` or `null`; `null` before read state loads |
| `unread/sync` | none | `{"capability","status","completeness","error"}` (`capability` `unsupported`, `read-only` or `frontier-sync`; `completeness` `unknown`, `snapshot` or `bounded`, also sent as `complete`): how read marks sync to the relay, or `null` before it is known. `unread/retrySync` tries again after a failure. |
| `threads/activity` | `channel` | `{"channel","items","coverage","freshness","error"}`: threads with unread replies for you, newest first, each `{"root","latestMessage","author","workflowOwner","createdAt","preview","unreadCount"}` |
| `inbox/get` | none | `{"status","freshness","items","error","feed","canRead"}`: the Inbox's conversations, each `{"id","channel","target","message","latestMessage","messageIds","messages","root","author","workflowOwner","preview","createdAt","mentioned","thread","unreadCount","manual","readThrough"}`. `target` and each `readThrough` mark are places in `read/mark`'s terms (`{"channel","thread"}` or `{"channel","message"}`), so marking the item read marks exactly what Buzz would; `manual` is a mark from `read/markLocal`. `feed` is the history read's `{"status","error","incomplete"}`. |
| `inbox/watch` | none | `{"watch"}`. Buzz then sends `inbox/changed` with what `inbox/get` gives, now and whenever the Inbox changes. Stop with `inbox/unwatch`. |
| `read/mark` | `all: true`, or `channel` with an optional `message`, `read` (default `true`), and `through` with an optional `root` | `{"durability","sync"}` once saved (`ReadMutationResult`), or with `all` a list of them, one per channel marked, each with its `channel`: `durability` is `saved`, and `sync` `pending` when this connection syncs read marks to the relay or `local-only` when the mark stays here (Buzz does not tell a mark that changed nothing apart). Marks everything read, a channel or message read or unread, or a channel or thread read up to `message`. Read marks sync to your other devices; unread marks stay on this device, as in buzz-app. |
| `read/markLocal` | `channel`, and optionally `thread` (its root) or `message` | `{"durability":"saved","sync":"local-only"}`, after marking it unread on this device only; it never syncs |
| `read/clearLocal` | `channel`, and optionally `thread` or `message` | As `read/markLocal`, after dropping an unread mark made on this device |
| `feedback/watch` | none | `{"watch","state"}`, then `feedback/changed` with `state` whenever it changes: `{"hydrated","available","error","pending"}`, whether feedback can be sent now and the feedback on its way, `{"id","content","delivery","error"}` (`delivery` as `outbox/get`'s). Stop with `feedback/unwatch`. |
| `feedback/upload` | `path` | Checks a PNG, JPEG, GIF or WebP image, strips the editor's private metadata, and uploads it to the deployment's feedback storage: `{"name","url","mime","size","sha256"}`. Needs `send-messages`. |
| `feedback/send` | `message`, `category` (`bug`, `praise`, `needs-work` or none), `attachments` (from `feedback/upload`), `diagnostics` (text to attach, checked against the event's limits before it uploads, or the attachment an earlier send returned) | `{"diagnostics"}`, the diagnostics attachment, for a later send to reuse. Sends product feedback (kind 42000) to the deployment's private inbox through the durable outbox. Needs `send-messages`. |
| `feedback/retry`, `feedback/dismiss` | `id` (the pending feedback's) | `null`. Sends it again as the same event, or forgets it on this device (which does not undo a delivery). Needs `send-messages`. |
| `outbox/get` | `id` (optional) | One event's `{"id","kind","channel","delivery","error","acknowledged","createdAt","content","tags","pubkey","sig"}` (`pubkey` and `sig` once signed) while Buzz still holds it, or `null`; without `id`, every event still on its way. `delivery` is `sending`, `accepted`, `unknown`, `failed` or `seen` (the relay showed it back). |
| `outbox/watch` | none | `{"watch"}`. Buzz then sends `outbox/changed` with `events`, the events still on their way, now and whenever that changes; one that leaves was delivered or dismissed. Stop with `outbox/unwatch`. |
| `outbox/retry`, `outbox/dismiss` | `id` (one of your events whose `delivery` is `failed`) | `null`. Sends it again, or drops it (`outbox.retry`, `outbox.dismiss`). Needs `publish-events`. |
| `outbox/send` | `kind`, `content`, `tags` | `{"id"}` as soon as it is queued. Buzz signs it once, as `events/publish` does, and sends it the way it sends its own messages: retried, shown in a conversation as it goes, and followed by `outbox/get` until the relay shows it back (`outbox.send`). Needs `publish-events`. |
| `outbox/acknowledge` | `id` | `null`. Says you checked the outcome of a delivered event (`outbox.acknowledge`). |
| `outbox/supports` | `kind` | Whether events of that kind can be signed and sent here (`outbox.supports`). |
| `archives/get` | `refresh` (optional) | `{"status","archived","eventId","createdAt","readAt","error"}` (`identityArchives.snapshot()`): the identities the community archived, which Buzz leaves out of mentions, member choices and search, with the read that found them; `status` is `unavailable` (no archive here), `idle` (also after a retry or cache clear), `loading`, `ready` or `error`, and `archived` lists anyone only when `ready`; Buzz hides no one while the archive is unknown |
| `archives/consent` | `pubkey` | How you may archive or unarchive it here (`archives.consent`): `{"auth"}` with the owner attestation of one of your agents, `{"admin":true}` by your relay role (both when both apply), `{}` for yourself, or `null` when you may not, the key is not lowercase hex, or the relay names no authority. |
| `archives/request` | `action` (`archive` or `unarchive`), `pubkey` | `null` once the archive shows it and Buzz has read it again. Allowed for one of your agents (its owner attestation is the consent) or with a relay owner or admin role. Needs `manage-channels`. |
| `sessions/create` | `title`, `parent` (optional session), `agents` (optional: your agents in this community, or agents already in `parent`, as lowercase hex; archived ones are refused) | `{"channel"}`: a new private work session, as Me starts, once the relay lists it with the agents added. Needs `manage-channels`. |
| `sessions/addAgents` | `channel`, `agents` (1 to 32 of your agents in this community, or agents already in the parent channel, as lowercase hex; archived ones are refused) | `null` once they are members, added to a session's parent first (`workSessions.addAgents`). Needs `manage-channels`. |
| `sessions/memberOf` | `pubkey` (as given; a key no channel lists finds none) | The ids of the channels that list them as a member |
| `agents/list` | none | `{"status","error","agents","running","elsewhere","teams","types"}`: `status` is `loading`, `ready`, `error` (with `error`) or `unavailable` (no agent host), so an empty list can be told from one not read yet; the selected community's agents and every agent running on this device, each `{"pubkey","name","type","owner","relay","picture","place","config","attentionEnabled","attention","skipped","timers"}` (`place` is `here`; `skipped` holds attention objects that could not be read, by slug, with the `problem`; `timers` is each timer's `armedAt`, `nextDue` and `used`); agents listed as running elsewhere, each `{"pubkey","name","type","owner","relay","picture","place","hosted"}` (`place` `elsewhere`, or `hosted` for a hosted agent); your teams `{"id","name","instructions","members"}`; and the agent types `{"type","title","description","hosted"}` |
| `agents/choices` | none | `{"status","error","pending","complete","identities","selectable","archives"}`: the agents you may pick in the selected community (`agentChoices`). `identities` are your agents its hosts run for this relay (`managed: true`) and the ones your agent list names there from other devices (`managed: false`, `managedName: null`), each `{"pubkey","name","avatar","managed","managedName"}`; `status` is `ready` once that list or the hosts are, else `error` if either failed, `loading` if either is being read, `idle` before the list was asked for, or `unavailable`; `pending` holds while the list is unread or being read, or the hosts are loading; `complete` needs the list read and the hosts ready or unavailable; `error` joins the list's failure and the hosts' with a space; `selectable` leaves out the ones the community archived, and `archives` is the archive read that decided it, `{"status","archived"}`. |
| `agents/watch` | none | `{"watch"}`. Buzz then sends `agents/changed` with what `agents/list` gives, now and whenever it changes. Stop with `agents/unwatch`. |
| `activity/observed` | `agent` (optional) | `{"status","turns","typing","records","history","hasOlder","historyOlder","historySkipped","historyAgents","trimmed","capture","workingChannels"}`: what the community's activity feed shows of agents anywhere, of one agent or all. `turns` are `{"agent","turn","channel","at","state","requests"}`, and `records` (newest 200) the decrypted transcript frames of your own agents, `{"id","agent","kind","createdAt","receivedAt","historical","channels","frame"}`. `typing` is each working agent's `{"agent","channel","root","since","timestamp","expiresAt","working"}` (`timestamp` the last signal), and `local` this device's own agents' requests in channels you can read, as `agents/activity` gives them. `historyOlder` says an older page than the newest is shown, `historySkipped` how many records it could not read and `historyAgents` whose records it holds; `trimmed` counts what left the live window, and `capture` (`unknown`, `saving`, `off` or `error`) whether the archive saves new frames. |
| `activity/history` | `agent` (optional), `older` (optional) | `null`. Reads saved records from the archive: an agent's, older ones with `older: true`, or everyone's newest. They arrive through `activity/watch`. |
| `archive/settings` | none | What this device saves of your agents' activity in the selected community (`ArchiveHost.settings`): `{"observer","metrics","observerDays","revision","location","path","bytes"}`. |
| `archive/configure` | `observer`, `metrics`, `observerDays` (1 to 90), `revision` (the one read) | The saved settings, as the Settings card saves them; capture follows them. Needs `manage-agents`. |
| `archive/read` | `kind` (24200 for activity, 44200 for metrics), `agent` (optional), `before` (optional: the page before's cursor) | `{"agents","records","before","skipped","revision"}`: one page of your agents' saved records, newest first (`ArchiveHost.read`), each `{"id","agent","createdAt","receivedAt","frame"}`; `skipped` counts records that no longer verify or decrypt. |
| `archive/clear` | `kind` (optional: 24200 for activity, 44200 for metrics; both without it) | `null`, after deleting the saved records (`clearHistory`). Needs `manage-agents`. |
| `activity/watch` | `agent` (optional) | `{"watch"}`. Buzz then sends `activity/changed` with what `activity/observed` gives, now and whenever it changes. Stop with `activity/unwatch`. |
| `agents/dismissError` | `agent`, `message` | `null`, after hiding that failed run's error on this device |
| `agents/clearActivity` | `agent` (optional) | `null`, after forgetting what agents on this device did, of one agent or all |
| `agents/create` | `type` (a `plugin/id` key, such as `com.example.hello/greeter`), `name`, optional `config`, `picture` (https) and `carry` (`{"instructions","workspace"}`: what a shared agent brings, which the type puts where it keeps them) | The new agent. Needs `manage-agents`. |
| `agents/save` | `pubkey`, and any of `name`, `picture` (https, or `null` or `""` to remove it), `config` (the whole config), `attentionEnabled`, `attention` (slug to attention object, or `null` to delete it) and `restart` (timer slugs to start over) | `null` once saved. Needs `manage-agents`. |
| `teams/save` | `id` (to replace one), `name`, `instructions`, `members` (agents on this device) | The team. Needs `manage-agents`. |
| `teams/remove` | `id` | `null`. Its agents stay. Needs `manage-agents`. |
| `dm/open` | `pubkeys` (1 to 8 other people, each once, as lowercase hex; any other list is refused) | `{"channel"}`: the direct message with them, made if new. Needs `manage-channels`. |
| `channels/create` | `name`, `description`, `private`, `ttlSeconds` (optional, a whole number of seconds from 1 to 2147483647; without it the channel is ongoing), `labels` (optional, up to 8), `setup` (optional, what a template adds: `{"canvas","agents","group","template"}`) | `{"channel"}`. With `setup`, Buzz writes the Canvas, adds the agents and files the channel in the group once the channel is confirmed, as a channel template does (`channelCreation.create` with `setup`). Its `agents` must be your agents in this community, none archived, by a fresh archive read; otherwise nothing is created. Needs `manage-channels`. |
| `channels/creation` | none | `{"pending","notices"}`: the channel being made now (`{"name","description","visibility","ttlSeconds","labels","setup"}`, `visibility` `open` or `private`), and setups that did not finish, each `{"id","name","error"}` (`channelCreation.snapshot`), including one the outbox still holds from before a restart. `session/watch` with `creation` follows this session's own creation and notices. |
| `channels/dismissNotice` | `id` | `null`. Hides a setup notice. |
| `channels/action` | `channel`, `action`: `join`, `leave`, `archive`, `unarchive`, `delete` (by an owner) or `hide` (a DM) | `null` once the relay shows it. Needs `manage-channels`. |
| `channels/addMember` | `channel` (yours, or an open one you have not joined), `pubkey` (lowercase hex, such as one of your agents; archived identities are refused) | `null` once the roster lists them. Needs `manage-channels`. |
| `channels/changeMember` | `channel`, `pubkey`, `change` (`admin`, `member`, `guest` or `remove`), `expected` (their role now: `owner`, `admin`, `member`, `guest` or `bot`) | `null` once done. Needs `manage-channels`. |
| `channels/details` | `channel` | `{"channel","version","details","canEdit","attempt"}`: `details` is `{"name","description","private","ttlSeconds","labels"}`; `attempt` is a save still `saving` or `unconfirmed`, with its `details`, or `null` |
| `channels/saveDetails` | `channel`, `version` (from `channels/details`), `details` (the whole draft: `name` (as given, not trimmed), `description` (empty clears it), `private`, and optionally `ttlSeconds` (a whole number of seconds, 1 to 2147483647) and `labels` (text)); or `channel` and `check: true` to check an unconfirmed save | `null` once saved. A save that may have landed stays `unconfirmed` until checked. Needs `manage-channels`. |
| `canvas/available` | none | Whether this connection can save a Canvas (`canvas.available`) |
| `canvas/read` | `channel`, `strong` (default `true`: past relay replicas, as before an edit) | The channel's Canvas `{"id","author","createdAt","content"}`, or `null` |
| `canvas/history` | `channel`, `cursor` (the `next` of the page before) | `{"revisions","next"}`: 25 revisions, newest first |
| `canvas/save` | `channel`, `content`, `expected` (the `id` it was read as, or `null` for none) | The saved revision. If the Canvas changed since, it fails with code 5 and nothing is written. Needs `send-messages`. |
| `agents/memories` | `agent` (one of your agents, as lowercase hex; read only while the live connection is up, else "Memory read unavailable") | `{"entries","partial"}`: its newest memory per slug, `{"slug","body","eventId","createdAt"}` |
| `kit/get` | none | `{"status","entries","error"}`: your teams, channel templates and sidebar groups in the community, each `{"eventId","createdAt","version","deleted","value"}`, where `value` has a `type` of `team`, `template` or `groups` |
| `kit/save` | `value` (as `kit/get` gives it), `expected` (its `eventId`, or `null` for new), `deleted` | `{"eventId"}`. Needs `manage-channels`. |
| `workflows/list` | `channel`, or `channels` (up to 128, read to the end, as the Workflows page reads them) | `{"status","workflows","partial","partialChannels","available"}`: the channels' workflows `{"id","owner","channel","revision","createdAt","yaml","webhookUrl"}`, and which of `definitions`, `history`, `save`, `trigger` and `delete` the community offers |
| `workflows/runs` | `channel`, `workflow` (an id), `owner` (where two owners use one id), `cursor` (the `next` of the page before) | `{"status","runs","next"}`: runs newest first, each `{"id","workflow","status","currentStep","trace","startedAt","completedAt","createdAt","errorCode","errorMessage"}`, `status` being `pending`, `running`, `waiting_approval`, `completed`, `failed` or `cancelled`; `next` is `{"before","beforeId"}` for the page after, or `null` |
| `workflows/save` | `channel`, `yaml`, and `workflow` (with `owner` where needed) to replace one, with the `revision` `workflows/list` gave it so the relay refuses the save if it changed since (the current one when left out) | `{"eventId"}` of the command. Needs `manage-channels`. |
| `workflows/delete`, `workflows/trigger` | `channel`, `workflow`, `owner` (where two owners use one id) | `{"eventId"}`. Needs `manage-channels`. |
| `workflows/operations` | none | The workflow commands sent this session, each `{"eventId","workflow","owner","channel","action","createdAt","delivery","outcome","error","run","secretHeld","webhookUrl"}`: `delivery` is `sending`, `accepted`, `unknown`, `failed` or `seen`, and `outcome` `pending`, `succeeded`, `rejected` or `unknown` |
| `workflows/takeSecret` | `eventId` | A new webhook's one-time secret, or `null` once taken |
| `workflows/dismiss` | `eventId` | `null`, after forgetting a finished command. Needs `manage-channels`. |
| `projects/list` | none | The community's repositories and projects, each `{"kind","owner","dtag","address","name","description","route","event"}`. `route` is what `ui/navigate` takes as `params` for the Projects page, `buzz.projects/projects`. |
| `projects/load` | `route` (an entity's `route`, or one naming a `tab`, `commit`, or an issue or pull request `id`) | `{"entity","repositories","unavailableRepositories","channels","item","status","commit","activity","items","truncated"}`: a project's repositories, linked channels, and the issue or pull request the route names with its comments, as Nostr events |
| `projects/home` | `channel` (any channel id, such as an open one from `channels/searchPublic`) | `{"home","ambiguous"}`: the channel's project, or `null` |
| `projects/gitAuthorization` | `repository` (a `https://…/git/<owner>/<d>.git` URL of the community) | `{"repository","token"}`: a NIP-98 value for `Authorization: Nostr `, good on that repository's Git routes for 60 seconds, for running Git yourself (`repository` in its canonical form), or `null` for a repository this community does not serve. Needs `publish-events`. |
| `agents/logProof` | `id` (the agent's log session), `pubkey` (the agent), `relayUrl` (the community's relay), `nonce` (the host's challenge) | `{"signature"}`: your proof, as the agent's owner, for one read of its harness log. Needs `manage-agents`. |
| `agents/attest` | `pubkey` (an agent key) | `{"auth"}`: your NIP-OA tag `["auth", owner, "", signature]` saying you own that agent, for an agent to attach to what it signs. Needs `manage-agents`. |
| `agents/presence` | `status` (`online` or `offline`), `pubkeys` (1 to 256, in one event; the relay keeps only agents you own; any other count is refused) | `true` once sent on the live connection, `false` if this community cannot take it: owner presence (kind 20003) for agents Buzz does not run itself, such as hosted ones. Needs `manage-agents`. |
| `services/list` | none | The services running plugins offer, each `{"plugin","service"}`. Buzz sends `services/changed` with `services`, the same list, when a plugin that offers any starts, stops or restarts, as a buzz-app plugin's `inject` learns a service came or went. |
| `services/request` | `plugin`, `service`, `method`, `params` | The answer of the plugin that offers it, within 30 seconds |
| `services/subscribe`, `services/unsubscribe` | `plugin`, `service` | `null`. Buzz then sends `service/event` (`plugin`, `service`, `event`, `data`) each time that plugin publishes an event of it. |
| `projects/git` | `owner`, `dtag`, optional `commit` and `path` | `{"head","commits","files","readme","file","diff"}`: read with Git from the community; `file` is the file at `path` and `diff` the commit's changes. Failures are `notFound`, `denied`, `unavailable` or `invalidTarget`. |
| `threads/follow` | `channel`, `root`, and `following` (`true` or `false`), or no `following` to ask | Whether you follow the thread now |
| `reminders/list` | none | `{"status","hydrated","reminders","error"}`, each `{"id","eventId","createdAt","notBefore","status","note","message"}` with `status` `pending`, `done` or `cancelled`; `hydrated` once the relay's history was read |
| `reminders/watch` | none | `{"watch"}`. Buzz then sends `reminders/changed` with what `reminders/list` gives. Stop with `reminders/unwatch`. |
| `reminders/create` | `channel`, `message`, `notBefore` (unix seconds), optional `note`, `preview` and `author` | `null` once saved. Needs `send-messages`. |
| `reminders/snooze`, `reminders/complete`, `reminders/cancel` | `id`, and `notBefore` to snooze | `null` once saved. Needs `send-messages`. |
| `agents/remove` | `pubkey` | `null` once the agent and its key are gone. Needs `manage-agents`. |
| `agents/previewRetype` | `pubkey` (an agent on this device), `type` | `{"config","moved","dropped","settings"}`: the config it would start with as that type, what it carries over (`instructions`, `workspace`) or leaves, and the settings the move resets or drops |
| `agents/retype` | `pubkey`, `type`, `config` (optional; from `agents/previewRetype`, edited or not) | `{"dropped"}` once it runs as the new type, keeping its key, name, picture, communities and attention. Refused while it is answering. Needs `manage-agents`. |
| `agents/portable` | none | `{"agents","warnings"}`: identities earlier agent systems saved on this device, `{"pubkey","name","picture","relay","source"}`, that are not agents yet |
| `agents/port` | `pubkey` (from `agents/portable`), `type` | The new agent, keeping the identity's key, name and picture, in the selected community. Needs `manage-agents`. |
| `agent/createConfig` | `view` (an open `agentCreate`), `config` (an object) | `null`. The new agent will start with this config. |
| `document/preview` | `text`, `html`, `base` (as a `document`'s) | `{"text","images","textOnly","line"}`: the first readable line (or "Images attached", or a placeholder for a long body), the GitHub upload images in it, whether it is text only, and the whole body as one line of text when it is a single short paragraph, for a `collapsible`'s `fit` (`bodyPreview`, `isTextBody`) |
| `clipboard/write` | `text` | `null`, once copied |
| `text/plain` | `text` | The text as a notification shows it: Markdown flattened to at most 200 characters (`plainText`), or `""` |
| `ui/pickFiles` | `multiple`, `prompt`, `images` (a hint; check what you get) | The paths of the files chosen in the system's file dialog, or `[]` when cancelled |
| `ui/confirm` | `title`, `description`, `confirm` (default "Confirm"), `cancel` (default "Cancel"), `destructive` | `true` once the user confirms, `false` if they cancel or close it (`AlertDialog`). A second question answers the first `false`. |
| `ui/openDialog` | `title`, `id` (default `dialog`), `context` (any JSON, up to 16 KiB), `size` (`default`, `wide`, or `expanded`: the window less 24px a side, with the body scrolling, closed only by Escape and its close button) | `{"view"}`. Buzz opens a dialog whose body is a view of `kind` `dialog`; its `view/open` carries `contribution` (the `id`) and `context`. Use it for what a message action or button shows next. One plugin dialog shows at a time. |
| `ui/closeView` | `view` (an open `dialog`, panel or provider dialog) | `null`, once it has closed |
| `reaction/select` | `view` (an open `reactionPicker`), `emoji` (a unicode emoji, or `:shortcode:` for one of the community's custom emoji) | `null`. Buzz reacts with it, or takes the reaction back if it is already yours, and closes the picker. Nothing is sent when the picker has closed, its message is gone or a reaction to it is still on its way; the answer is `null` either way, where buzz-app's `select` answers `false`. |
| `composer/settle` | `view`, `element` (a composer that handed the plugin its draft), `sent` | `null`. With `sent: true` the composer clears; otherwise it keeps the draft to try again. |
| `composer/insert` | `composer` (from `composer/tool`, or an embedded composer's `ready`), `text` | `null`, after putting the text at the composer's caret |
| `composer/insertMentions` | `composer`, `pubkeys` (at most 32), or `mentions` (`[{"pubkey","name"}]`, to label each chip yourself, as `insertMention` does) | `null`, after putting mention chips at the caret; the people become recipients. In a conversation, everyone must be mentionable there (not archived, and in a session a member or one of your agents), or nothing is inserted and it answers "A team member is no longer available." While editing a message the people are inserted as identity links, as an edit never adds recipients. Like the other composer writes (`composer/insert`, `composer/insertResource`, `composer/capture`), it is refused while the composer is disabled or read-only. |
| `composer/toolState` | `tool`, and optionally `composer` (one composer; all without it), `pressed`, `badge` (up to 64 characters), `disabled` (why it cannot be used, shown in its tooltip) | `null`. What the tool's button shows, as a buzz-app tool component draws its own state. |
| `composer/openPopover` | `composer`, `tool`, `context` (optional) | `{"view"}`: a view of `kind` `toolPopover`, anchored above the tool's button, whose `view/open` carries `contribution` (the tool id), `composer` and `context`. It closes with `ui/closeView`, a click outside, or another popover. |
| `composer/insertResource` | `composer`, `uri` (a Buzz link, or a web link in its normal form without credentials, such as `https://example.com/`), `label`; at most 32 links per message, web links included | `null`, after putting the link as one chip at the caret |
| `composer/focus` | `composer` | `null`, after putting the caret back in the composer |
| `composer/capture` | `composer` | `{"view"}`. A view of `kind` `capture` takes the composer's place, as recording a voice note does, until `composer/accept` or the user discards it (the view then closes), or the tool that started it goes, or the composer becomes disabled (as when the channel is archived). Refused before any view opens, as Buzz's own recorder is, while another capture, an edit or a send is under way, when the composer takes no attachments, or when the draft already holds a voice note or the most files a message takes (which also shows Buzz's "Attach at most" notice). |
| `composer/accept` | `view` (an open `capture`), and optionally `path` (a file, up to the community's limit for its type) or `data` (base64, up to 3 MiB), `name`, `mime` and `voice` (`{"duration","waveform"}`: seconds, and up to 256 levels from 0 to 255) | `null`. Ends the capture and attaches the file, if given, to the draft; with `voice`, as a voice note that plays in the draft. An error when the capture has already ended, or the composer was disabled meanwhile (the capture then ends with nothing attached). |

Reads cover the selected community and channels you can read in it. They
never mark anything read. A read waits up to 10 seconds for the relay.

Any request can name `community`, the `wss://` relay of another community
you joined (see `community/list`), to reach that one instead, such as the
community a running agent works in. Watches keep the community they
started with.

A message looks like this:

```json
{
  "id": "<event id>", "channel": "<channel id>",
  "author": "<pubkey>", "authorName": "Ada",
  "content": "Hello", "createdAt": 1792200000, "createdAtMs": 1792200000000,
  "edited": false, "threadRoot": null, "replyParent": null, "replyCount": 2,
  "mentions": [], "delivery": null, "kind": 9, "audience": null,
  "signer": null, "workflowOwner": null, "participants": [],
  "membership": null, "attachmentSourceId": null, "attachmentSeams": [],
  "reactions": [{"content": "👍", "emojiUrl": null, "shortcode": null, "count": 1, "authors": ["<pubkey>"],
                 "events": [{"id": "<reaction event id>", "author": "<pubkey>"}]}],
  "attachments": [{"url": "https://…", "kind": "image", "mime": "image/png",
    "size": 73, "name": "dot.png", "duration": null, "waveform": null,
    "voiceNote": false, "dimensions": {"width": 1, "height": 1},
    "blurhash": null, "previewUrl": null}],
  "emoji": [{"shortcode": "party", "url": "https://…"}],
  "mentionReferences": [], "sourceContent": null, "sentFromThread": null,
  "diff": null, "deliveryError": null
}
```

`mentionReferences` are people named without being notified;
`sentFromThread` (`{"root","excerpt"}`) marks a copy of a thread reply;
`diff` (`{"filePath","repoUrl","commitSha","description","truncated"}`)
describes a diff message; `sourceContent` is the text before Buzz moved
attachment links out of it. `attachmentSourceId` is the edit whose
attachments show, when not the message itself, and `attachmentSeams` the
character offsets in `content` where removing an attachment joined
once-separate text (buzz-app counts UTF-16 units). A reaction's
`shortcode` names its custom emoji.

`authorName` is `null` until Buzz knows the author. Ask `profiles/get`
for anyone you need: a profile with `loaded: false` is a placeholder, and
Buzz fetches the real one for a later call. `delivery` is set on your own
messages: `sending`, `accepted`, `unknown`, `failed` or `seen`.

`messages/send` answers once the message is safely queued, as Buzz's own
composer does. The relay can still refuse it later; a followed channel then
shows it with `delivery: "failed"`, and `outbox/get` with its `id` says how
it is going.

### `channel/changed` (notification from Buzz)

A followed channel changed. It carries the same fields as `channel/messages`,
plus the `watch` id:

```json
{"jsonrpc":"2.0","method":"channel/changed","params":{"watch":1,"channel":"<id>","messages":[],"hasMore":true,"status":"ready","loadingOlder":false,"historyLimited":false,"restored":false,"stale":false,"error":null}}
```

Buzz sends it once when following starts, and again on each change. Following
ends with `channel/unwatch` or when the plugin stops.

### Errors

| Code | Meaning |
| --- | --- |
| `-32601` | Buzz has no such method. |
| `-32602` | A param is missing or not valid. |
| `1` | Nothing to act on: no community is selected, or the channel is not one you can read. |
| `2` | The relay did not answer, or the read failed. |
| `3` | Sending failed, such as when you cannot post in the channel. |
| `4` | The plugin did not declare the permission this needs. |
| `5` | A write lost to a newer one, such as a Canvas saved since it was read. |

Toast titles are cut to 128 characters, and bodies to 200.

## Sharing a plugin

### Add from Git

**Settings → Plugins → Add from Git…** takes an HTTPS or SSH repository
(`owner/repository` means GitHub) and an optional branch or tag. Buzz clones
it and lists every folder with a `buzz-plugin.json`: the command it runs, what
it may do, and who signed it. Installing copies that folder into Buzz's own
plugin folder, and the plugin starts off.

Nothing in the repository runs while Buzz reads it. Git uses no global or
system config, runs no hooks, fetches no submodules or LFS files, asks for no
passwords, and speaks only HTTPS and SSH. Symlinks are checked out as plain
files. A clone may take 60 seconds and 256 MiB. SSH uses your SSH agent and
known hosts, not your SSH config.

A repository hosted by the selected community, including a private one, is
fetched as you: Buzz signs in with NIP-98, and sends that sign-in only to that
repository.

**Update** on an installed plugin reads its repository again and shows the
same preview. An update keeps the plugin's switch, and a running plugin
restarts. If the new version asks for more permissions, it is turned off until
you review it.

A plugin added from a folder and one installed from Git cannot share an id.
Remove one to add the other.

### Signing

Signing tells people who published a plugin. It uses buzz-app's NIP-PS
signature event (kind `1064`) over a list of the folder's files:

- `plugin.artifact.json` lists every file except hidden ones, with its size and
  SHA-256.
- `plugin.signature.json` is the signed event: empty content, an `x` tag with
  the SHA-256 of `plugin.artifact.json`, and a `t` tag of
  `buzz-plugin-folder-v1`.

**Sign** on a plugin added from a folder writes both files with your Buzz key.
Commit them with the plugin. A signed folder cannot contain symlinks.

Buzz checks the signature and every file before each start. Once Buzz has
seen a plugin signed, later versions must be signed by the same key: an
unsigned copy or another publisher stops the plugin until you remove it and
add it again. Editing a signed folder stops it too; sign it again, or delete
`plugin.signature.json` while you work on it.

A signature identifies a publisher. It does not make a plugin safe.

### Safe mode

Start Buzz with `BUZZODZ_SAFE_MODE=1`, as with buzz-app, to keep the plugins
you added from running for that launch. Their switches stay as they were.

## When something goes wrong

- Lines on stdout that are not JSON-RPC are logged and ignored. A stray
  `print` to stdout does not stop the plugin, but its output goes nowhere.
- A tree that is not valid keeps the last good one on screen and shows the
  error under it.
- If the plugin exits, its pages and panels stay and show the exit code and
  its last stderr lines, with a **Restart** button. Fixing a file in the
  folder restarts it too. Its agent types stay, so its agents keep their
  settings; everything else it added, such as composer tools, message
  actions, completions and settings cards, goes until it runs again.
- **Settings → Plugins** shows each plugin's state. **Reload** reads the
  manifest again and restarts the plugin.

## Buzz's own plugins

Some of Buzz's own plugins are programs on this API too, written in
TypeScript in [`plugins/`](../plugins): Bestie, Diffs, Todos, Channel
Usage, Feedback, Reminders and GitHub. They ship with Buzz
and the Node that runs them, and Settings → Plugins lists them under Built
in with the rest of Buzz's own. They are trusted: they may use `buzz.` ids,
are allowed the permissions they ask for, and run in safe mode.
[`plugins/README.md`](../plugins/README.md) explains how to build and work
on them.

## How this differs from buzz-app

buzz-app plugins are JavaScript running inside the app; these are programs
that draw with Buzz's elements. Everything buzz-app's plugin API offers has a
request, a notification or a contribution here. Where a buzz-app plugin
passes a function, or reaches into the app, it works differently:

- **A place belongs to a community.** A `ui/navigate` or a page or
  settings notification without `community` is tied to the community
  selected when it was sent, so opening it later (a click, Back or
  Forward) selects that community again; buzz-app's leaves the selection
  alone.

- **Views stay open while their page is hidden.** A plugin page; the
  agent tab shown on the Agents page or in an agent's profile; the
  Agents page's provider and New agent dialogs; the panel tabs and
  drawer of the conversation Messages or Me shows; the Inbox and Drafts
  readers; and the accessories above those pages' and readers' composers, and a plugin's capture in them
  (`composer/capture`), stay open when you go elsewhere, and are
  shown again as they were. A panel beside the page (an `appLauncher`'s,
  or one opened outside a conversation) stays open as you move between
  Messages or Me and other pages. buzz-app unmounts all of these with
  their page and opens them afresh when you come back. A page with
  `routes` or `handlesNavigation` hears `page/route` when it is visited
  again; any page can follow visits with `navigation/watch`.

- **A composer outlasts its conversation in tabs and the media review.**
  When a conversation open as a tab is archived or left, or the message
  a media review shows is deleted, loses that attachment in an edit, or
  its conversation is archived, its composer and accessories stay open
  until the tab or review closes (a review that opens on an attachment
  no longer there still mounts them); buzz-app replaces them with an
  "unavailable" notice and closes the accessories. Opening a media
  review from Messages also leaves the side tabs, panels and the panel
  beside the page open, which buzz-app closes until the review ends.

- **Readers still open panels and the media review.** In the Inbox and
  Drafts readers and in media review comments, links open nowhere in
  Buzz as in buzz-app, but an accessory there can still open panels with
  `panel/canOpen` and `ui/openPanel`, where buzz-app's `canOpen` and
  `open` answer `false`, and an image there opens the full media review,
  comments included, where buzz-app shows a bare viewer.

- **A panel's context is fixed when it opens.** A drawer's `thread` and
  `channelName` are those when it opened; buzz-app passes the drawer a
  live context that follows the thread opened beside it. Switching
  community closes the panel beside the page, which buzz-app keeps
  open.

- **A retry keeps a connected community's session.** `community/retry` on
  a connected community leaves `status` at `ready` and keeps its channels,
  watches, threads, unread counts and outbox; Buzz retries live updates,
  the roster and read sync. buzz-app replaces the session, reporting
  `connecting` with an empty session until it is ready again.
  `generation` changes with every retry and cache clear, where buzz-app's
  changes only when the session is replaced: not for a retry while
  connecting, a retry of a saved snapshot, or a cache clear of a
  connected, saved or failed session.

- **Buzz's own shortcuts work behind dialogs.** Buzz's chords, such as
  Settings or Go home, still navigate while a dialog or the media review
  is open, which `navigation/watch` reports; buzz-app holds them while a
  dialog is open. Plugin shortcuts wait out dialogs as in buzz-app.

- **Statuses are read for anyone.** `statuses/get` answers for every key
  asked about; buzz-app's `statuses.current` answers only for people
  watched and for you.

- **A profile without a name has no name source without a policy.** With
  no naming policy on, `names/get` gives such a key its first characters
  with `source: null`, where buzz-app calls that a public-profile name.
  With a policy on (Identity Naming, by default) the source is
  `agent-directory`, as in buzz-app.

- **Bundled renderers keep their place.** Voice Notes' player and Emoji's
  custom emoji stay first among renderers when switched off and on
  again; in buzz-app they register last again, after plugins added
  meanwhile.

- **Buzz's own plugin chords come first.** Terminal's and Mission
  control's chords are Buzz's keymap: they take the press even while
  their plugin is off or the terminal launcher is hidden, before any
  plugin shortcut with the same keys; buzz-app passes the press on then.

- **Names know the agents this device runs.** The naming policy sees your
  agents this device runs; ones only your relay agent list names, from
  other devices, are named by their profile until it loads.

- **Policy labels are tidied.** A `names/resolve` answer's name is
  trimmed and kept to 128 characters and its qualifier to 64; buzz-app
  uses them as returned.

- **A profile sent through the outbox shows once the relay has it.** A
  kind 0 queued with `outbox/send` changes `profiles/get` and `names/get`
  when the relay echoes it back; buzz-app shows it while it is queued.

- **Sidebar write flags need loaded preferences.** `sortWritable` and
  `muteWritable` are `false` until the sidebar preferences have loaded;
  buzz-app reports them `true` meanwhile (a write is refused either way).

- **`events/query` reads what the relay gives.** Its events are
  verified and filtered to channels you can read, but a reaction, edit or
  deletion is not held back because its target is one Buzz has not
  loaded; buzz-app's `read` drops those.

- **A relay that never archived anyone.** A community whose relay has no
  NIP-IA archive snapshot (kind 13535) reads as `ready` with nobody
  archived, as the buzz CLI reads it; buzz-app waits for a snapshot and
  reports `error` until one exists. A read archive also stays through a
  dropped and restored live connection (only a retry or cache clear
  resets it); buzz-app clears it whenever the live connection drops.

- **Functions become declarations or questions.** A buzz-app plugin hands
  over functions such as `matches(url)`, `eligible(channel)` or
  `describeMigration(config)`. Here a contribution declares what they would
  decide: link prefixes, regular expressions, channel kinds, config keys.
  Where only the plugin can decide, Buzz asks it (`message/render`,
  `names/resolve`, `agent/summary` and so on), or the plugin says when
  something changed (`settings/visibility`, `shortcut/enable`,
  `render/invalidate`, and `completion/publish` for a suggestion that can
  no longer be picked, in place of `canSelect`).
- **Answers come later.** Buzz draws while it waits for a plugin's answer.
  It shows the plain message, link, name or summary first, then the
  plugin's answer once it arrives, and keeps answers until what they
  describe changes.
- **Views are element trees.** A plugin draws with Buzz's elements, and can
  embed Buzz's own composer and thread. It cannot run arbitrary UI code.
- **Reading follows Buzz's views.** buzz-app's reading handle (`observe`,
  `catchUp`) marks messages read as a plugin shows them. Here only Buzz
  draws messages, so its own views do this, including a thread a plugin
  embeds; `read/mark` covers explicit marks.
- **Reads are snapshots.** A channel's `unread/target`, agent memories,
  workflows and their runs and
  operations, project reads, the channel kit, channel details' save
  attempts and member changes (`pending`, `uncertain`) answer once rather
  than handing out live views. Ask again to refresh, or watch the
  channel, thread, unread counts, Inbox, names, presence, events, outbox
  or a `session/watch` value where a change needs to be seen as it
  happens. `events/query` is a read from the relay with the events your
  outbox holds added, on their way or delivered; it does not fold in live events that arrived
  meanwhile or your pending edits and deletions, as `session.read` does,
  so a plugin that needs those follows `events/watch` and the outbox.
- **Element slots in place of components.** The embedded composer and
  message take Buzz's own controls only: there are no React slots
  (`trailingTool`, `replyContext`, `label`, `quickControls`,
  `overflowItems`). A plugin draws its own elements around them, and its
  message actions show in the message's menu. Links open as everywhere
  else in Buzz (no `onOpenLink`), the embedded composer saves its draft
  under its `draftKey` (no `onDraftSaved` or `recoveredDraft`), and
  editing your last message with the up arrow needs the conversation's
  own views. An embedded message has no `alignViewer`, `day`,
  `compactAvatar` or media playback options (`mediaMode`, `mediaSeekTo`,
  `onMediaTime` and the like), and an embedded composer none of Me's
  conversation modes (`sessionConversation`, `personalConversation`,
  `selectedAgent`, `chatPresentation`, `activityClickOpensPanel`), media
  time stamps or `receiptOnly`: those belong to Buzz's own Me and media
  views. Links in an embedded composer or message open as everywhere in
  Buzz, so there is no `canOpenLink`; `autoFocus` stands for
  `focusRequest`; and Buzz names the channel itself (`channelName`). A
  draft in a composer is plain text: link and inline renderers draw sent
  messages, not a draft as buzz-app's composer decorates it. The
  `thread` element takes the thread's `root`, where buzz-app's `messageId`
  may be any message in it (`thread/watch` takes any). A link renderer
  draws its whole chip, so there is no `className`. Contribution readers carry no `revision`; a plugin's
  restart is its `plugins/changed`.
- **Action filters are declared.** Message actions pick messages by kind,
  author, attachments, reactions, replies, text and channel; a test that
  needs more of the session, such as the channel's kind, goes in the
  plugin's handling of `message/action`.
- **Services are announced, not injected.** A buzz-app plugin `inject`s
  the services it needs and starts once they exist. Here a plugin always
  starts; `services/list` and `services/changed` say which are offered.
- **A run ends when the plugin answers.** buzz-app's `workingUntil` keeps
  a run open past its answer. Here the plugin answers, then opens a call
  as the same agent with `agent/open` and keeps acting through it.
- **Hosted agents show no type views.** The Agents page shows a hosted
  agent as Hosted, with Delete: its type's `summary`, `peek` and tabs are
  not asked for it, where buzz-app draws them (without Attention or
  Memory).

- **Hosted agents have no unfinished state.** Making a hosted agent
  finishes or fails as a whole.
- Buzz draws a panel's channel-header launcher from its `icon` and `label`,
  as the icon button buzz-app's own launchers are; a plugin cannot draw a
  different control there.
- There is no embedded browser (`host.browser` is `false`).
- A plugin cannot take a community offline (`relay.disconnect`); Buzz owns
  its connections. `community/retry` and `community/clearCache` remain.
- There are no `available` flags on the session's capabilities (channel
  creation, the kit, Canvas, lifecycle, details, member changes, DMs, Me
  placement): ask `outbox/supports` for the event kind the write sends,
  and `workflows/list` for workflows.
- buzz-app lists a page in navigation only when it is `primary`; here a
  page shows unless its `placement` is `hidden` or `primary` is `false`.
- Contribution ids are unique across all of a plugin's contributions, not
  only within one kind. An `order` may be any number and is rounded to a
  whole one.
- `channels/findLabeled` and `channels/resolve` read as Buzz's own views
  do and take no `consistency`; `community/refresh` with `roster` reads
  past replicas.
- A channel's `members` is always a list: `[]` until its roster is read,
  where buzz-app leaves it out while unknown.
- `activity/observed` typing rows are working evidence only, so
  `working` is always `true`, and `timestamp` is worked out from when the
  row expires, up to a few seconds before the true last signal.
  `archive/clear` deletes saved records and reads history again; live
  records already on show stay until they leave the live window.
- The relay timing profiler (`session.profiling`) is a Buzz diagnostic
  and is not offered to plugins.
- Some session state is not modelled: a channel list has no `coverage`,
  `asOf` or `activityStatus`, the sidebar never reports `unsupported`,
  read marks carry no `operationId`, and there are no agent or team
  catalogs (NIP-AP), so `outbox/send` takes no `supersedes` and
  `agents/choices` no `definitions` or `templates`.
- A voice note sent with `messages/send` carries its duration but no
  waveform: the relay refuses the `waveform` key, so the waveform shows
  only where the file is played.
- Messages are deleted one kind-5 event each, as the relay refuses a
  deletion that names more than one; `messages/delete` with `messages`
  answers for each.
- `outbox/watch` sees each send once it is queued, in place of
  `observeSend`. Draft recovery for a send that fails (`recover`,
  `recovery`) stays with Buzz's own composer.
- New messages as they arrive (`subscribeIncoming`) are `events/watch` on
  kinds 9, 40002 and 40008, with `channel/watch` for the folded rows
  (author as the relay attributes it, preview, audience). Product
  feedback (`feedbackUpload`, kind 42000) is `files/upload` and
  `outbox/send`.
- Some buzz-app members are other requests here: what can be written is
  `outbox/supports` for its kind (`statuses.writable`, `archives.writable`),
  a newly opened DM shows in the `channels` watch (`subscribeOpened`), a
  member being added is in the outbox (`memberAdditions.snapshot`), and
  people past the 256 Buzz follows for presence read as `null`
  (`presence.limited`).
- Host grants (commands, network origins, processes) do not apply: a plugin
  is a program and already has your access.

## Not built yet

- A WebAssembly runtime for plugins that run sandboxed
  ([#12](https://github.com/benthecarman/buzz-gpui/issues/12)).
