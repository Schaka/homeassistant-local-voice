#!/usr/bin/env bash
# Run the assistant-hub container built by ./scripts/build.sh. Everything is
# on port 10300: Wyoming STT and TTS, the LLM under /llm/v1, MCP tools on /mcp,
# /web/search, /youtube/search and /jellyfin/search. If the host has a GPU, the
# first /dev/dri render node is passed through. Without one, STT and TTS run
# on the CPU.
set -eu
cd "$(dirname "$0")/.."

GPU_ARGS=()
RENDER_DEV="${RENDER_DEV:-$(ls /dev/dri/renderD* 2>/dev/null | head -1)}"
if [ -n "$RENDER_DEV" ]; then
    GPU_ARGS=(--device "$RENDER_DEV" --group-add "$(stat -c %g "$RENDER_DEV")")
else
    echo "No /dev/dri render node: STT and TTS run on the CPU."
fi

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
  "${GPU_ARGS[@]}" \
  -p 10300:10300 \
  -e SEARCH_ENABLED="${SEARCH_ENABLED:-off}" \
  "${LLM_ARGS[@]}" \
  --restart unless-stopped \
  localhost/assistant-hub:latest

echo "Started on port 10300 (Wyoming and HTTP)."
echo "Health: curl http://localhost:10300/health"
echo "Logs:   docker logs -f assistant-hub"
