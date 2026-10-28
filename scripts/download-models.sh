#!/usr/bin/env bash
# Stage the English STT + TTS models into ./models/ -- for two use cases:
#   1. Pre-seeding a host directory that gets mounted over /models at runtime
#      (swap models without rebuilding the image; see README), and
#   2. local builds that copy models in instead of downloading them in-stage.
# The Dockerfile downloads the same files itself (with the same URLs as
# defaults), so this script is optional for the normal build path.
# Sizes are verified.
#
# Optional conversation agent: pass the LLM gguf filename (the same value as
# LLM_MODEL_FILENAME at runtime) to also stage it under models/llm/, e.g.
#   ./scripts/download-models.sh MiniCPM5-1B-Q4_K_M.gguf
set -eu
cd "$(dirname "$0")/.."

LLM_FILE="${1:-}"

mkdir -p models/stt models/pocket-tts/embeddings models/llm

HF="https://huggingface.co"

# LLM: official repos only (third-party Ling 3.0 quants were reported
# malformed in llama.cpp PR #26608).
case "$LLM_FILE" in
    "") ;;
    MiniCPM5-1B-Q4_K_M.gguf)   LLM_REPO=openbmb/MiniCPM5-1B-GGUF;       LLM_SIZE=688065920 ;;
    MiniCPM5-1B-Q8_0.gguf)     LLM_REPO=openbmb/MiniCPM5-1B-GGUF;       LLM_SIZE=1153529216 ;;
    MiniCPM5-2B-Q4_K_M.gguf)   LLM_REPO=openbmb/MiniCPM5-2B-GGUF;       LLM_SIZE=1561318368 ;;
    MiniCPM5-2B-Q8_0.gguf)     LLM_REPO=openbmb/MiniCPM5-2B-GGUF;       LLM_SIZE=2679710688 ;;
    Ling-3.0-tiny-Q4_K_M.gguf) LLM_REPO=inclusionAI/Ling-3.0-tiny-GGUF; LLM_SIZE=4823894944 ;;
    Ling-3.0-tiny-Q5_K_M.gguf) LLM_REPO=inclusionAI/Ling-3.0-tiny-GGUF; LLM_SIZE=5635443616 ;;
    Ling-3.0-tiny-Q6_K.gguf)   LLM_REPO=inclusionAI/Ling-3.0-tiny-GGUF; LLM_SIZE=6499262368 ;;
    Ling-3.0-tiny-Q8_0.gguf)   LLM_REPO=inclusionAI/Ling-3.0-tiny-GGUF; LLM_SIZE=8408187808 ;;
    *)
        echo "ERROR: unknown LLM file '$LLM_FILE' -- see the list in $0" >&2
        exit 1
        ;;
esac

# STT: NVIDIA Parakeet CTC 0.6B, q8_0 (English). WER 0 vs NeMo.
STT_URL="$HF/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf"
STT_OUT="models/stt/parakeet-ctc-0.6b-q8_0.gguf"
STT_SIZE=875449920

# TTS: PocketTTS-100M English, q8_0 + the 'alba' voice embedding.
TTS_URL="$HF/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf"
TTS_OUT="models/pocket-tts/pocket-tts-english-q8_0.gguf"
TTS_SIZE=127867968  # upstream re-published 2026-10-27
VOICE_URL="$HF/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/embeddings/alba.safetensors"
VOICE_OUT="models/pocket-tts/embeddings/alba.safetensors"
VOICE_SIZE=6194424

fetch() { # url out expected_size
    local url="$1" out="$2" want="$3"
    echo "==> $out"
    if [ -f "$out" ] && [ "$(stat -c%s "$out")" = "$want" ]; then
        echo "    already present, size ok"
        return
    fi
    curl -sL --retry 3 -o "$out" "$url"
    local got
    got=$(stat -c%s "$out")
    if [ "$got" != "$want" ]; then
        echo "    ERROR: size $got != expected $want" >&2
        exit 1
    fi
    echo "    ok (${got} bytes)"
}

fetch "$STT_URL"  "$STT_OUT"  "$STT_SIZE"
fetch "$TTS_URL"  "$TTS_OUT"  "$TTS_SIZE"
fetch "$VOICE_URL" "$VOICE_OUT" "$VOICE_SIZE"
if [ -n "$LLM_FILE" ]; then
    fetch "$HF/$LLM_REPO/resolve/main/$LLM_FILE" "models/llm/$LLM_FILE" "$LLM_SIZE"
fi

echo
echo "Models staged under ./models:"
du -sh models/*