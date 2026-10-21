# Buzz's own Node plugins

Buzz's feature plugins that run as programs on the plugin API in
[`docs/plugins.md`](../docs/plugins.md), written in TypeScript. Each folder
with a `buzz-plugin.json` is one plugin; `sdk` is the library they share
(`@buzz/plugin-sdk`).

| Folder | Plugin |
| --- | --- |
| `bestie` | `buzz.bestie`: the companion's home page |
| `diffs` | `buzz.diffs`: code diff messages (kind 40008) and their expanded view |
| `todos` | `buzz.todos`: a channel's todo list, kept in its Canvas |
| `channel-usage` | `buzz.channel-usage`: what agents reported using in a channel |
| `feedback` | `buzz.feedback`: Send feedback, in the account menu |
| `reminders` | `buzz.reminders`: Remind me, the Reminders page and due notifications |
| `github` | `buzz.github`: GitHub repository, pull request, issue and commit links in a panel |

## Working on them

From the repository root, in the dev shell (`nix develop`), which has
Node 22:

```sh
cd plugins
npm ci
npm run check   # types, tests, then the build
npm run build   # just the build: <plugin>/dist/plugin.mjs
```

The build bundles each plugin's `src/main.ts` into one
`dist/plugin.mjs`, so a plugin ships as its manifest and that file.

A debug build of the app loads the plugins from this folder and restarts
one when its files change, so rebuilding is enough to see a change. Set
`BUZZ_BUNDLED_PLUGINS` to load them from another folder, and `BUZZ_NODE` to
choose the `node` that runs them. An installed app loads them from
`share/buzz-gpui/plugins` beside its `bin` folder and runs them with its
own Node from `lib/buzz-gpui/node` (`Contents/Resources/plugins` and
`Contents/Resources/node` in the macOS app). The packaging scripts pin
that Node, check its checksum, build the plugins and run
`scripts/check-plugins.mjs` to see each one start.

They are Buzz's own: they may use `buzz.` ids, get the permissions their
manifest asks for, and are switched on and off by their Built in row in
Settings → Plugins. Their state goes in `BUZZ_PLUGIN_DATA_DIR`
(`readJson`/`writeJson` in the SDK).

## Writing one

```ts
import { el, run } from "@buzz/plugin-sdk";

void run({
  contributions: { pages: [{ id: "home", title: "Home" }] },
  views: {
    home: (open, view) => {
      let count = 0;
      return {
        render: () => el.column([el.text(`Clicked ${count}`), el.button("more", "More")]),
        event: (event) => {
          if (event.id === "more") count++;
        },
      };
    },
  },
});
```

A view draws again after each of its events; call `view.update()` to draw
after anything else, such as an answer from `view.host.request(...)`.
Test with `FakeHost` from `@buzz/plugin-sdk/testing`, which plays Buzz's
part.
