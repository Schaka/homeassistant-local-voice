#!/usr/bin/env bash
# Run the assistant-hub container. Everything is on port 10300: Wyoming STT
# and TTS, the LLM under /llm/v1, MCP tools on /mcp, /web/search and
# /youtube/search. Detects the first /dev/dri render node and its group.
set -eu
cd "$(dirname "$0")/.."

RENDER_DEV="${RENDER_DEV:-$(ls /dev/dri/renderD* 2>/dev/null | head -1)}"
if [ -z "$RENDER_DEV" ]; then
    echo "ERROR: no /dev/dri/renderD* found -- is a GPU present?" >&2
    echo "       (CPU-only llvmpipe is supported; see README.)" >&2
    exit 1
fi
RENDER_GROUP="$(stat -c %g "$RENDER_DEV")"

# Optional conversation agent (LLM_FAMILY: ling3|minicpm5|lfm3|qwen35|granite4, empty = detect), either
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
        -e LLM_FAMILY="${LLM_FAMILY:-}"
    )
fi

docker run -d --name assistant-hub \
  --device "$RENDER_DEV" \
  --group-add "$RENDER_GROUP" \
  -p 10300:10300 \
  -e SEARCH_ENABLED="${SEARCH_ENABLED:-off}" \
  "${LLM_ARGS[@]}" \
  --restart unless-stopped \
  localhost/assistant-hub:latest

echo "Started on port 10300 (Wyoming and HTTP)."
echo "Health: curl http://localhost:10300/health"
echo "Logs:   docker logs -f assistant-hub"
