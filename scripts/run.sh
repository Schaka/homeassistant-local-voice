#!/usr/bin/env bash
# Run the Wyoming voice container (STT + TTS on port 10300).
# Auto-detects the first /dev/dri render node and its group; no host driver
# install or other host setup is required.
set -eu
cd "$(dirname "$0")/.."

RENDER_DEV="${RENDER_DEV:-$(ls /dev/dri/renderD* 2>/dev/null | head -1)}"
if [ -z "$RENDER_DEV" ]; then
    echo "ERROR: no /dev/dri/renderD* found -- is a GPU present?" >&2
    echo "       (CPU-only llvmpipe is supported; see README.)" >&2
    exit 1
fi
RENDER_GROUP="$(stat -c %g "$RENDER_DEV")"

podman run -d --name wyoming-voice \
  --device "$RENDER_DEV" \
  --group-add "$RENDER_GROUP" \
  --security-opt seccomp:unconfined \
  --security-opt label=disable \
  --ipc host \
  -p 10300:10300 \
  --restart unless-stopped \
  localhost/wyoming-voice:latest

echo "Started. STT + TTS on port 10300."
echo "Logs: podman logs -f wyoming-voice"
echo "Swap models at runtime: see README 'Using bigger models'."