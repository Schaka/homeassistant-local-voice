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

# Optional conversation agent (LLM_FAMILY: minicpm5|ling3), either
#   LLM_MODEL_FILENAME  a gguf staged by ./scripts/download-models.sh, or
#   LLM_HF_REPO         e.g. inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M, downloaded
#                       on first start.
# Both live in ./models/llm (writable: it holds the download cache).
LLM_ARGS=()
if [ -n "${LLM_MODEL_FILENAME:-}${LLM_HF_REPO:-}" ]; then
    mkdir -p models/llm
    LLM_ARGS=(
        -v "$PWD/models/llm:/models/llm"
        -e LLM_MODEL_FILENAME="${LLM_MODEL_FILENAME:-}"
        -e LLM_HF_REPO="${LLM_HF_REPO:-}"
        -e LLM_FAMILY="${LLM_FAMILY:-minicpm5}"
    )
fi

podman run -d --name wyoming-voice \
  --device "$RENDER_DEV" \
  --group-add "$RENDER_GROUP" \
  --security-opt seccomp:unconfined \
  --security-opt label=disable \
  --ipc host \
  -p 10300:10300 \
  -p 8080:8080 \
  "${LLM_ARGS[@]}" \
  --restart unless-stopped \
  localhost/wyoming-voice:latest

echo "Started. STT + TTS on port 10300."
if [ -n "${LLM_MODEL_FILENAME:-}${LLM_HF_REPO:-}" ]; then
    echo "Conversation agent (llama-server) on http://<host>:8080/v1."
fi
echo "Logs: podman logs -f wyoming-voice"
echo "Swap models at runtime: see README 'Using bigger models'."