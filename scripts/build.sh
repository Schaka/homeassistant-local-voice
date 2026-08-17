#!/usr/bin/env bash
# Build both images. Run after ./scripts/collect-vk-libs.sh and
# ./scripts/download-models.sh on the GPU host.
set -eu
cd "$(dirname "$0")/.."

echo "==> [1/2] building audiocpp-vk-build (audio.cpp + glslc from source) =="
[ -f ./radeon_icd.container.json ] || { echo "run ./scripts/collect-vk-libs.sh first" >&2; exit 1; }
[ -f ./models/stt/parakeet-ctc-0.6b-q8_0.gguf ] || { echo "run ./scripts/download-models.sh first" >&2; exit 1; }
[ -f ./models/pocket-tts/pocket-tts-english-q8_0.gguf ] || { echo "run ./scripts/download-models.sh first" >&2; exit 1; }

podman build -t audiocpp-vk-build -f Dockerfile.build-audiocpp .

echo "==> [2/2] building wyoming-voice =="
podman build -t wyoming-voice -f Dockerfile .

echo
echo "Built: localhost/wyoming-voice:latest"
echo "Deploy with: ./scripts/run.sh"