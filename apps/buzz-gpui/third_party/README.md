# Vendored crates

## gpui-pre-linux 0.3.8

GPUI's Linux platform, from crates.io (Apache-2.0, Zed Industries). The
first commit that adds it here is the published crate unchanged (without
its `Cargo.lock`), so `git log -p third_party/gpui-pre-linux` shows every
change made since. The workspace uses it through `[patch.crates-io]`.
Drop it when upstream has equivalents.

Patches:

- `set_activation_token`: the next window activation uses an
  xdg-activation token the app was handed, so GNOME and other Wayland
  compositors raise the window when a notification is clicked or a second
  launch hands over. Without it they refuse, since the app isn't focused.
- Naming windows at once: a Wayland toplevel gets its title and app id as
  soon as it is created, not after its renderer, whose setup can flush the
  connection first. GNOME otherwise showed the window as an unknown app,
  a second dock icon, for the ~180 ms in between.
- Pasting files: a `text/uri-list` clipboard offer of local files reads as
  `ClipboardEntry::ExternalPaths`, followed by the paths as text, on Wayland
  and X11. Other URI lists still read as text.

Run its tests on their own, since it isn't a workspace member:

```sh
cd third_party/gpui-pre-linux
CARGO_TARGET_DIR=../../target/vendored cargo test --features gpui/test-support --lib
rm Cargo.lock
```

## gpui-base 0.7.1

gpui-kit's base crate, which holds its text input, from crates.io
(Apache-2.0, Longbridge). The first commit that adds it here is the
published crate unchanged (without its `Cargo.lock`), so
`git log -p third_party/gpui-base` shows every change made since. The
workspace uses it through `[patch.crates-io]`. Drop it when upstream has
equivalents.

Patches:

- Decorations in every input mode: the text and range decoration stores
  move from the code editor's extras into the shared `InputBaseState`, and
  `create_decorations_collection`, `create_range_decorations_collection`
  and their collection handles work for `InputState` and `TextareaState`
  too. The handles take the mode as a type parameter that defaults to
  `EditorMode`, so existing code still compiles. The composer uses this to
  style Markdown as it is typed. Before, only `EditorState` could hold
  styled ranges. The renderer already drew decorations in every mode.
- Selectable text in paint order: a `SelectableText` without an explicit
  `document_order` takes the next one while painting, as `TextView` does,
  so a selection dragged across plain runs and text views copies them in
  the order they are on screen. Before, every such run sorted first.
- Backgrounds beside inline tokens: a line with a token (a chip) now
  paints the background colors of its text runs. Its layout never marked
  them, so a highlighted background vanished from any line with a chip.
- Concealed ranges: `set_concealed_ranges` on `InputState` and
  `TextareaState` takes `Concealment`s, byte ranges of the text that are
  laid out with no width, or with a short replacement string drawn in the
  style of their first byte. The value, history and clipboard keep every
  byte. They reuse the token path: a row with a concealed range is laid out
  as inline fragments, and its widths reach soft wrapping, auto-grow and
  horizontal scroll through the display map's inline metrics. Other rows,
  including rows without a token in a field that has one, keep the plain
  shaped layout. The caret never rests inside a range. An arrow skips the
  hidden text first, then steps over one visible character. Backspace and
  Delete remove the visible character beside the hidden text. A hit test
  that cannot tell offsets apart takes the leftmost. Edits shift ranges
  after them and drop the ones they touch inside. Ranges that cross a line
  break or overlap a token are dropped, and masked inputs ignore them. The
  composer uses this to hide Markdown markers as buzz-app does.

Run its tests on their own, since it isn't a workspace member:

```sh
cd third_party/gpui-base
CARGO_TARGET_DIR=../../target/vendored cargo test --features test-support --lib
rm Cargo.lock
```
