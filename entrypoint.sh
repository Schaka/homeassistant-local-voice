#!/usr/bin/env bash
# Container entrypoint: two processes, one public Wyoming port (10300).
#   audiocpp_server  (TTS model resident, REST on 127.0.0.1:8100)
#   wyoming_voice    (STT + TTS, Wyoming on 0.0.0.0:10300)
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

# Reject a truncated/corrupt model before it reaches the native loader -- a
# short download still has a syntactically valid GGUF header (metadata sits
# at the front of the file), and parakeet.cpp/audio.cpp trust that header's
# declared tensor sizes rather than the actual file length.
if ! python3 /app/verify_gguf.py \
    "/models/stt/${STT_MODEL_FILENAME}" \
    "${TTS_DIR}/${TTS_MODEL_FILENAME}"; then
  echo "FATAL: model file failed integrity check (see above) - refusing to start" >&2
  exit 1
fi

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

# On SIGTERM (incl. GPU-fault self-restart from wyoming_voice.py), stop the
# whole container so the orchestrator's restart policy brings it back fresh.
trap 'kill 0 2>/dev/null; exit 0' INT TERM
wait