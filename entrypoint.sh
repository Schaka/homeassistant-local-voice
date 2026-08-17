#!/usr/bin/env bash
# Container entrypoint: one image, three services.
#   audiocpp_server  (TTS model resident, REST on 127.0.0.1:8100)
#   wyoming_stt      (STT, tcp 0.0.0.0:10300)
#   wyoming_tts      (TTS, tcp 0.0.0.0:10301)
set -u
cd /app

export VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/opt/vk/radeon_icd.container.json}"
export PARAKEET_DEVICE="${PARAKEET_DEVICE:-Vulkan0}"

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
run_one "wyoming-stt" python3 /app/wyoming_stt.py \
  --model /models/stt/parakeet-ctc-0.6b-q8_0.gguf \
  --lib /usr/lib/libparakeet.so \
  --uri tcp://0.0.0.0:10300 --device "${PARAKEET_DEVICE}" &
run_one "wyoming-tts" python3 /app/wyoming_tts.py \
  --uri tcp://0.0.0.0:10301 \
  --audio-cpp-url http://127.0.0.1:8100 --model-id pocket-tts --voice alba &

trap 'kill 0' INT TERM
wait