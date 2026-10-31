#!/usr/bin/env bash
# Container entrypoint: two processes, one public Wyoming port (10300), plus
# an optional third one when an LLM model is configured.
#   audiocpp_server  (TTS model resident, REST on 127.0.0.1:8100)
#   wyoming_voice    (STT + TTS, Wyoming on 0.0.0.0:10300)
#   llama-server     (conversation agent, OpenAI-compatible on 0.0.0.0:8080/v1;
#                     only if LLM_MODEL_FILENAME or LLM_HF_REPO is set)
#
# Everything is env-driven: the baked-in defaults match the models the
# Dockerfile downloads; override any of them at runtime to swap models without
# rebuilding (e.g. mount a models volume and point STT_MODEL_FILENAME at a
# bigger gguf). All values fall back to the image defaults.
set -u
cd /app

export PARAKEET_DEVICE="${PARAKEET_DEVICE:-Vulkan0}"
export STT_MODEL_FILENAME="${STT_MODEL_FILENAME:-parakeet-ctc-0.6b-q8_0.gguf}"
export STT_MODEL_NAME="${STT_MODEL_NAME:-parakeet-ctc-0.6b}"
export TTS_DIR="${TTS_DIR:-/models/pocket-tts}"
export TTS_LANGUAGE="${TTS_LANGUAGE:-english}"
export TTS_MODEL_ID="${TTS_MODEL_ID:-pocket-tts}"
export TTS_VOICE_ID="${TTS_VOICE_ID:-alba}"
export TTS_LANGUAGES_BCP="${TTS_LANGUAGES_BCP:-en}"
export VK_DEVICE="${VK_DEVICE:-0}"
export TTS_THREADS="${TTS_THREADS:-2}"

# Conversation agent (optional). Two mutually exclusive ways to pick a model;
# neither set = no llama-server:
#   LLM_MODEL_FILENAME  a gguf already under /models/llm (baked or mounted)
#   LLM_HF_REPO         "<user>/<repo>[:QUANT]" -- llama-server downloads it
#                       on first start into /models/llm/hf (mount /models/llm
#                       to keep it across container re-creates)
export LLM_MODEL_FILENAME="${LLM_MODEL_FILENAME:-}"
export LLM_HF_REPO="${LLM_HF_REPO:-}"
if [ -n "${LLM_MODEL_FILENAME}" ] && [ -n "${LLM_HF_REPO}" ]; then
  echo "FATAL: set LLM_MODEL_FILENAME or LLM_HF_REPO, not both" >&2
  exit 1
fi
llm_default_name="${LLM_MODEL_FILENAME%.gguf}"
if [ -n "${LLM_HF_REPO}" ]; then
  # inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M -> Ling-3.0-tiny-Q4_K_M
  llm_repo="${LLM_HF_REPO##*/}"
  llm_default_name="${llm_repo%%:*}"
  llm_default_name="${llm_default_name%-GGUF}"
  if [[ "${llm_repo}" == *:* ]]; then
    llm_default_name="${llm_default_name}-${llm_repo#*:}"
  fi
fi
export LLM_MODEL_NAME="${LLM_MODEL_NAME:-${llm_default_name}}"
export LLM_FAMILY="${LLM_FAMILY:-minicpm5}"
export LLM_DEVICE="${LLM_DEVICE:-Vulkan0}"      # "none" = CPU only
export LLM_GPU_LAYERS="${LLM_GPU_LAYERS:-auto}" # auto = fit to free VRAM
export LLM_CTX="${LLM_CTX:-64k}"                # 32k | 64k | 128k | 256k
export LLM_REASONING="${LLM_REASONING:-on}"     # on | off (thinking mode)
export LLM_REASONING_BUDGET="${LLM_REASONING_BUDGET:-512}" # max thinking tokens, -1 = no cap
export LLM_API_KEY="${LLM_API_KEY:-}"           # empty = accept any/no key
export LLM_EXTRA_ARGS="${LLM_EXTRA_ARGS:-}"
export LLM_SLOT_CACHE="${LLM_SLOT_CACHE:-on}"     # on | off (keep the prompt cache across restarts)
export LLM_THINKING_GUARD="${LLM_THINKING_GUARD:-on}" # on | off (retry an empty reply once without thinking)

# Web search (optional): SearXNG on 127.0.0.1 plus a one-tool MCP server
# (web_search_mcp.py) for the Home Assistant "Model Context Protocol"
# integration at http://<host>:${SEARCH_MCP_PORT}/mcp.
export SEARCH_ENABLED="${SEARCH_ENABLED:-off}"   # on | off
export SEARCH_MCP_PORT="${SEARCH_MCP_PORT:-8090}"
export SEARCH_LANGUAGE="${SEARCH_LANGUAGE-en}"   # result language, e.g. en or de; empty = all
export SEARXNG_PORT_INTERNAL=8888

# Reject a truncated/corrupt model before it reaches the native loader -- a
# short download still has a syntactically valid GGUF header (metadata sits
# at the front of the file), and parakeet.cpp/audio.cpp trust that header's
# declared tensor sizes rather than the actual file length.
models=("/models/stt/${STT_MODEL_FILENAME}" "${TTS_DIR}/${TTS_MODEL_FILENAME}")
if [ -n "${LLM_MODEL_FILENAME}" ]; then
  models+=("/models/llm/${LLM_MODEL_FILENAME}")
fi
if ! python3 /app/verify_gguf.py "${models[@]}"; then
  echo "FATAL: model file failed integrity check (see above) - refusing to start" >&2
  exit 1
fi

# Per-family llama-server defaults. Both families are hybrid reasoning models.
# Thinking stays on by default (the models' own default); LLM_REASONING=off
# trades answer quality for a faster spoken reply. llama-server returns the
# thinking in reasoning_content, so it never reaches TTS. Sampling follows
# each model card (MiniCPM5 has one set per mode, and warns that llama.cpp's
# default min-p 0.05 causes repetition). MTP speculative decoding
# (--spec-type) stays off: it hangs on RADV for Ling 3.0 (llama.cpp PR
# #26608). LLM_EXTRA_ARGS comes last, so it overrides any of these.
# A small model can think for 1500+ tokens on one command (MiniCPM5-1B did,
# 75 s on CPU), so thinking is capped; llama-server then forces the answer.
case "${LLM_REASONING}" in
  on)  llm_reasoning=(--reasoning on --reasoning-budget "${LLM_REASONING_BUDGET}") ;;
  off) llm_reasoning=(--reasoning off) ;;
  *)
    echo "FATAL: unknown LLM_REASONING=${LLM_REASONING} (expected on or off)" >&2
    exit 1
    ;;
esac
case "${LLM_FAMILY}" in
  minicpm5)
    if [ "${LLM_REASONING}" = on ]; then
      llm_preset=(--temp 0.9 --top-p 0.95 --min-p 0)
    else
      llm_preset=(--temp 0.7 --top-p 0.95 --min-p 0)
    fi
    ;;
  ling3) llm_preset=(--temp 1.0 --top-p 0.95 --top-k 20)
         # Patched chat template (templates/ling3.jinja): after a tool result the
         # turn starts with a closed <think></think>, so the answer is never filed
         # as reasoning. LLM_TEMPLATE_FIX=off uses the template from the GGUF.
         if [ "${LLM_TEMPLATE_FIX:-on}" = on ]; then
           llm_preset+=(--chat-template-file /app/templates/ling3.jinja)
         fi ;;
  *)
    echo "FATAL: unknown LLM_FAMILY=${LLM_FAMILY} (expected minicpm5 or ling3)" >&2
    exit 1
    ;;
esac

# Context size. Both families are trained to 128k. 256k needs YaRN rope
# scaling, which only Ling 3.0 documents (factor 2 over the 128k base).
case "${LLM_CTX}" in
  32k)  llm_ctx=(-c 32768) ;;
  64k)  llm_ctx=(-c 65536) ;;
  128k) llm_ctx=(-c 131072) ;;
  256k)
    if [ "${LLM_FAMILY}" != ling3 ]; then
      echo "FATAL: LLM_CTX=256k is only supported for LLM_FAMILY=ling3 (max 128k)" >&2
      exit 1
    fi
    llm_ctx=(-c 262144 --rope-scaling yarn --rope-scale 2 --yarn-orig-ctx 131072)
    ;;
  *)
    echo "FATAL: unknown LLM_CTX=${LLM_CTX} (expected 32k, 64k, 128k or 256k)" >&2
    exit 1
    ;;
esac

# Render the audiocpp_server config from env (the runtime image ships no jq;
# python3 is already present for wyoming).
python3 - <<'PY'
import json, os
cfg = {
    "host": "127.0.0.1",
    "port": 8100,
    "backend": "vulkan",
    "device": int(os.environ["VK_DEVICE"]),
    "threads": int(os.environ["TTS_THREADS"]),
    "lazy_load": True,
    "models": [
        {
            "id": os.environ["TTS_MODEL_ID"],
            "family": "pocket_tts",
            "path": os.environ["TTS_DIR"],
            "task": "tts",
            "mode": "offline",
            "load_options": {"language": os.environ["TTS_LANGUAGE"]},
            "session_options": {"language": os.environ["TTS_LANGUAGE"]},
            "default_voice_preset": {"voice_id": os.environ["TTS_VOICE_ID"]},
        }
    ],
}
with open("/app/server.json", "w") as f:
    json.dump(cfg, f, indent=2)
PY

run_one() {
  local name="$1"; shift
  while true; do
    echo "[$name] starting: $*"
    "$@" &
    local pid=$!
    wait "$pid"
    echo "[$name] exited ($?) - restarting in 2s"
    sleep 2
  done
}

run_one "audiocpp" /usr/local/bin/audiocpp_server --config /app/server.json &
run_one "wyoming-voice" python3 /app/wyoming_voice.py \
  --uri tcp://0.0.0.0:10300 \
  --stt-model "/models/stt/${STT_MODEL_FILENAME}" \
  --stt-lib /usr/lib/libparakeet.so --stt-device "${PARAKEET_DEVICE}" \
  --stt-model-name "${STT_MODEL_NAME}" \
  --audio-cpp-url http://127.0.0.1:8100 --tts-model-id "${TTS_MODEL_ID}" \
  --tts-voice "${TTS_VOICE_ID}" --tts-language "${TTS_LANGUAGES_BCP}" &

# The key goes through llama-server's own env var, not argv: run_one logs the
# full command line. Unset (not empty) when no key is wanted, so the server
# skips auth and accepts any Authorization header.
if [ -n "${LLM_MODEL_FILENAME}" ] || [ -n "${LLM_HF_REPO}" ]; then
  if [ -n "${LLM_API_KEY}" ]; then
    export LLAMA_API_KEY="${LLM_API_KEY}"
  else
    unset LLAMA_API_KEY
  fi
  if [ -n "${LLM_HF_REPO}" ]; then
    # llama.cpp's downloader resumes partial files and checks the ETag, so
    # verify_gguf is not needed here. HF_TOKEN works for gated repos.
    export LLAMA_CACHE=/models/llm/hf
    mkdir -p "${LLAMA_CACHE}"
    llm_model=(-hf "${LLM_HF_REPO}" --no-mmproj)
    # Once cached, never go online again: when upstream re-publishes a file
    # (new ETag), llama.cpp deletes the cached copy and downloads it anew.
    # Models are never deleted by this image. No tag = llama.cpp's default.
    llm_want="${LLM_HF_REPO,,}"
    if [[ "${llm_want##*/}" != *:* ]]; then
      llm_want="${llm_want}:q4_k_m"
    fi
    if /opt/llama.cpp/llama-server --cache-list 2>/dev/null \
        | awk '/^ *[0-9]+\. /{print tolower($2)}' | grep -qxF "${llm_want}"; then
      echo "[llama-server] ${LLM_HF_REPO} is cached, starting offline"
      llm_model+=(--offline)
    fi
  else
    llm_model=(-m "/models/llm/${LLM_MODEL_FILENAME}")
  fi
  # Prompt cache across restarts: the first Assist request after a start
  # prefills the full system prompt (about 35 s for 4.8k tokens on a CPU).
  # llm_slot_cache.py saves the KV cache of the last Assist conversation to
  # /models/llm/slots and restores it when llama-server comes up (0.3 s).
  # The file name holds the model and context size, because a cache from
  # another model or context size cannot be restored.
  llm_slot_args=()
  llm_slot_dir=/models/llm/slots
  llm_slot_file="${LLM_MODEL_NAME}-${LLM_CTX}.slot"
  llm_slot_file="${llm_slot_file//[^A-Za-z0-9._-]/_}"
  if [ "${LLM_SLOT_CACHE}" = on ]; then
    if mkdir -p "${llm_slot_dir}" 2>/dev/null && [ -w "${llm_slot_dir}" ]; then
      llm_slot_args=(--slot-save-path "${llm_slot_dir}")
    else
      echo "[llama-server] ${llm_slot_dir} is not writable, prompt cache is not kept across restarts" >&2
      LLM_SLOT_CACHE=off
    fi
  fi
  read -ra llm_extra <<< "${LLM_EXTRA_ARGS}"
  # Thinking guard: with thinking on, a model now and then puts its whole answer
  # into the thinking block, and Home Assistant gets an empty reply. llm_guard.py
  # then takes port 8080 and retries such a reply once without thinking.
  # llama-server moves to an internal port.
  llm_host=0.0.0.0; llm_port=8080
  if [ "${LLM_REASONING}" = on ] && [ "${LLM_THINKING_GUARD}" = on ]; then
    llm_host=127.0.0.1; llm_port=8081
    run_one "llm-guard" python3 /app/llm_guard.py --listen 0.0.0.0:8080 --backend 127.0.0.1:8081 &
  fi
  export LLM_INTERNAL_URL="http://127.0.0.1:${llm_port}"
  run_one "llama-server" /opt/llama.cpp/llama-server \
    "${llm_model[@]}" --alias "${LLM_MODEL_NAME}" \
    --host "${llm_host}" --port "${llm_port}" --no-ui \
    --device "${LLM_DEVICE}" -ngl "${LLM_GPU_LAYERS}" "${llm_ctx[@]}" \
    --jinja "${llm_reasoning[@]}" "${llm_slot_args[@]}" \
    "${llm_preset[@]}" "${llm_extra[@]}" &
  if [ "${LLM_SLOT_CACHE}" = on ]; then
    python3 /app/llm_slot_cache.py daemon "${llm_slot_dir}" "${llm_slot_file}" &
  fi
else
  LLM_SLOT_CACHE=off
fi

case "${SEARCH_ENABLED}" in
  on)
    # SearXNG wants a secret for its session cookies. Nothing here is
    # persistent, so a new random one per start is fine. The JSON format is
    # what the MCP server reads; the limiter and the image proxy are off
    # because only the local MCP server talks to it.
    mkdir -p /tmp/searxng
    cat > /tmp/searxng/settings.yml <<YAML
use_default_settings: true
general:
  instance_name: "voice search"
server:
  bind_address: "127.0.0.1"
  port: ${SEARXNG_PORT_INTERNAL}
  secret_key: "$(python3 -c 'import secrets; print(secrets.token_hex(32))')"
  limiter: false
  image_proxy: false
search:
  safe_search: 1
  formats: [html, json]
YAML
    run_one "searxng" env SEARXNG_SETTINGS_PATH=/tmp/searxng/settings.yml \
      PYTHONPATH=/opt/searxng/src /opt/searxng/venv/bin/python -m searx.webapp &
    run_one "web-search-mcp" python3 /app/web_search_mcp.py \
      --searxng "http://127.0.0.1:${SEARXNG_PORT_INTERNAL}" \
      --port "${SEARCH_MCP_PORT}" --language "${SEARCH_LANGUAGE}" &
    ;;
  off) ;;
  *)
    echo "FATAL: unknown SEARCH_ENABLED=${SEARCH_ENABLED} (expected on or off)" >&2
    exit 1
    ;;
esac

# On SIGTERM (incl. GPU-fault self-restart from wyoming_voice.py), stop the
# whole container so the orchestrator's restart policy brings it back fresh.
# Save the prompt cache first: it takes about 0.3 s, well inside docker's
# 10 s stop timeout.
on_stop() {
  if [ "${LLM_SLOT_CACHE}" = on ]; then
    timeout 8 python3 /app/llm_slot_cache.py save "${llm_slot_dir}" "${llm_slot_file}"
  fi
  kill 0 2>/dev/null
  exit 0
}
trap on_stop INT TERM
wait