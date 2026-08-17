#!/usr/bin/env bash
# Run the Wyoming STT (10300) + TTS (10301) voice container.
set -eu
cd "$(dirname "$0")/.."

podman run -d --name wyoming-voice \
  --device /dev/dri/renderD128 \
  --group-add 105 \
  --security-opt seccomp:unconfined \
  --security-opt label=disable \
  --ipc host \
  -p 10300:10300 \
  -p 10301:10301 \
  --restart unless-stopped \
  localhost/wyoming-voice:latest

echo "Started. STT on port 10300, TTS on port 10301."
echo "Logs: podman logs -f wyoming-voice"