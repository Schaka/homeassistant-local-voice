#!/usr/bin/env bash
# Build the self-contained Wyoming voice image. Nothing host-specific is
# needed up front -- the image builds its own Mesa Vulkan drivers and
# downloads its own models (see the Dockerfile ARGs to swap models).
set -eu
cd "$(dirname "$0")/.."

podman build -t wyoming-voice -f Dockerfile .

echo
echo "Built: localhost/wyoming-voice:latest"
echo "Deploy with: ./scripts/run.sh"
echo "Swap models: see README 'Using bigger models' (build ARGs or env overrides)."