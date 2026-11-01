#!/usr/bin/env bash
# Build the image locally, the same way CI does (docker-bake.hcl).
set -eu
cd "$(dirname "$0")/.."

docker buildx bake final --set final.tags=localhost/assistant-hub:latest --load

echo
echo "Built: localhost/assistant-hub:latest"
echo "Run it with: ./scripts/run.sh"
