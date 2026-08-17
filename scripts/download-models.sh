#!/usr/bin/env bash
# Download the English STT + TTS models into ./models/ (baked into the image).
# Re-run after switching quantizations. Sizes are verified.
set -eu
cd "$(dirname "$0")/.."

mkdir -p models/stt models/pocket-tts/embeddings

HF="https://huggingface.co"

# STT: NVIDIA Parakeet CTC 0.6B, q8_0 (English). WER 0 vs NeMo.
STT_URL="$HF/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf"
STT_OUT="models/stt/parakeet-ctc-0.6b-q8_0.gguf"
STT_SIZE=875449920

# TTS: PocketTTS-100M English, q8_0 + the 'alba' voice embedding.
TTS_URL="$HF/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf"
TTS_OUT="models/pocket-tts/pocket-tts-english-q8_0.gguf"
TTS_SIZE=127856704
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

echo
echo "Models staged under ./models:"
du -sh models/*