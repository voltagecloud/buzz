#!/usr/bin/env bash
# Prove the walkthrough-video toolchain works as the agent user with an empty
# HOME, like the provider volume: install the browser that matches a pinned
# Playwright, capture a page, narrate one line with Piper, and mux an MP4.
# Needs network access for the npm package and the browser download.
set -euo pipefail
IMAGE="${1:-buzz-agent:local}"
docker run --rm --tmpfs /home/agent:uid=10001,gid=10001,mode=0700,exec \
  --entrypoint /bin/bash "$IMAGE" -ceu '
    work=$(mktemp -d)
    cd "$work"
    # Outside a project, the install prints a warning box; failures still exit nonzero.
    npx --yes playwright@1.64.1 install chromium >/dev/null 2>&1
    npx --yes playwright@1.64.1 screenshot --viewport-size=1280,720 \
      "data:text/html,<h1>walkthrough check</h1>" frame.png
    echo "This is a walkthrough narration check." \
      | "$WALKTHROUGH_CACHE/venv/bin/piper" \
          -m "$WALKTHROUGH_CACHE/voices/en_US-lessac-medium.onnx" -f line.wav
    ffmpeg -loglevel error -loop 1 -i frame.png -i line.wav \
      -c:v libx264 -pix_fmt yuv420p -c:a aac -shortest check.mp4
    ffprobe -v error -show_entries stream=codec_type -of csv=p=0 check.mp4 | sort | tr "\n" " " \
      | grep -qx "audio video "
  '
printf 'PASS: walkthrough-video toolchain (%s)\n' "$IMAGE"
