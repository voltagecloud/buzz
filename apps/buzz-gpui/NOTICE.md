# Attribution

Buzz GPUI is licensed under Apache-2.0 (see [LICENSE](LICENSE)). It reimplements
[block/buzz-app](https://github.com/block/buzz-app) (Apache-2.0, © Block, Inc.)
natively, using buzz-app's behavior and documentation as its reference. It
depends on buzz-app's `buzz-agent-controller` and `buzz-credential-store`
crates.

`third_party/gpui-pre-linux/` is GPUI's Linux platform crate, gpui-pre-linux
0.3.8 ([zed-industries/zed](https://github.com/zed-industries/zed),
Apache-2.0, © Zed Industries, Inc.), with the changes listed in
`third_party/README.md`; its license is in `third_party/gpui-pre-linux/`.

`third_party/gpui-base/` is gpui-kit's base crate, gpui-base 0.7.1
([longbridge/gpui-kit](https://github.com/longbridge/gpui-kit), Apache-2.0,
© 2024-2026 Longbridge), with the changes listed in `third_party/README.md`;
its license is in `third_party/gpui-base/`.

The app icons in `assets/icons/` are copied from buzz-app's
`src-tauri/icons`, which copied them from Block Buzz's
`desktop/src-tauri/icons` ([block/buzz](https://github.com/block/buzz),
Apache-2.0). The background is recoloured navy so this app is told apart
from buzz-app.

## Fonts

`assets/fonts/inter/` holds static instances of Inter 4.1
([rsms/inter](https://github.com/rsms/inter)), extracted from the release's
`Inter.ttc` (as packaged by nixpkgs). They are modified only by removing
U+2600, U+2665, U+26A0 and U+2764 from their character maps, so those
symbols draw as colour emoji. Copyright 2016 The Inter Project Authors.
Licensed under the SIL Open Font License 1.1; see
`assets/fonts/inter/OFL.txt`.

`assets/fonts/buzz-color-emoji/BuzzColorEmoji.ttf` is Noto Color Emoji 2.051
([googlefonts/noto-emoji](https://github.com/googlefonts/noto-emoji)),
modified and renamed "Buzz Color Emoji": the family is renamed and U+006D maps
to its space glyph (`crates/ui/src/fonts.rs` explains why). Copyright 2022
Google Inc. Licensed under the SIL Open Font License 1.1; see
`assets/fonts/buzz-color-emoji/OFL.txt`.

`assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf` and
`JetBrainsMono-SemiBold.ttf` (the terminal's bold) are JetBrains Mono 2.304
([JetBrains/JetBrainsMono](https://github.com/JetBrains/JetBrainsMono)),
unchanged. Copyright 2020 The JetBrains Mono Project Authors. Licensed under
the SIL Open Font License 1.1; see `assets/fonts/jetbrains-mono/OFL.txt`.

## Icons

`assets/icons/tabler/` holds unchanged SVGs from Tabler Icons 3.46.0
([tabler/tabler-icons](https://github.com/tabler/tabler-icons)), the set
buzz-app's icon gateway exports. MIT License, Copyright (c) 2020-2026 Paweł
Kuna; see `assets/icons/tabler/LICENSE`.

`assets/icons/custom/` holds artwork from buzz-app's design system
(Apache-2.0, © Block, Inc.): the "Send to channel" and filled notification
glyphs from `src/shared/design-system/icons/`, and the avatar squircle and
presence-cutout shapes from `src/shared/design-system/styles/`, redrawn with
`currentColor` fills.

## Sounds

`assets/sounds/` holds the twelve notification alert sounds (MP3) from
buzz-app's `public/sounds/` (Apache-2.0, © Block, Inc.), unchanged. buzz-app
shares them with Block Buzz's `desktop/public/sounds/`
([block/buzz](https://github.com/block/buzz), Apache-2.0).

`assets/recipient-removal/` holds the five-frame "poof" sprite and the
`plop.m4a` sound the composer plays when an attachment is removed, from
buzz-app's `public/recipient-removal/`, unchanged. They are MIT
© 2023 Emerge Tools, Inc.; the licence is beside them in `LICENSE.txt`.

## Harness brand marks

The marks in `assets/icons/brand/` identify agent harnesses on the Agents
page. They are reused from buzz-app's
`src/shared/design-system/icons/HarnessLogos.tsx`, which documents their
origins in its `NOTICE.md`:

- `goose.svg`: Block Goose mark from
  [block/goose](https://github.com/block/goose),
  `documentation/static/img/goose.svg`. Apache-2.0 © Block, Inc. The fill is
  `currentColor`.
- `pi.svg`: Pi favicon from
  [earendil-works/pi-website](https://github.com/earendil-works/pi-website),
  `src/favicon.svg`, unchanged. MIT License, Copyright (c) 2026 Earendil Inc.
  and contributors.
- `claude.png`: Claude Code mark, reused unchanged from Block Buzz. Claude is
  a trademark of Anthropic.
- `hermes.png`: Hermes Agent mark from
  [NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent),
  as cropped and resized by Block Buzz. MIT License, Copyright (c) 2025 Nous
  Research.
- `codex.svg`: the OpenAI Blossom mark, from buzz-app's `CodexLogoArtwork`,
  which takes it from OpenAI's brand kit
  (https://cdn.openai.com/brand/openai-logos.zip,
  `SVGs/OAI_OpenAI-Blossom_Black.svg`) to identify OpenAI's Codex harness.
  The fill is `currentColor` and the viewBox is cropped to the mark. OpenAI
  and the Blossom logo are trademarks of OpenAI.

The MIT License terms for Pi and Hermes:

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
