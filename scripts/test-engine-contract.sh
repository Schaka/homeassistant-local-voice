#!/usr/bin/env bash
# Runs the contract tests of tests/llm_contract.rs (tiny tier) against the
# llama-server that the image contains. No GPU and no big download: a 1 MB test
# model, checked by its hash.
#
#   scripts/test-engine-contract.sh            # llama.cpp (LLAMACPP_BUILD in docker-bake.hcl)
#   scripts/test-engine-contract.sh ik         # ik_llama.cpp (IKLLAMA_COMMIT), built from source
#
# Files go to target/engine-contract. For another engine build or a real
# model, see the top of tests/llm_contract.rs.
set -euo pipefail
cd "$(dirname "$0")/.."
engine=${1:-llamacpp}
pin() { sed -n "s/^variable \"$1\" { default = \"\\(.*\\)\" }/\\1/p" docker-bake.hcl; }
dir="target/engine-contract"
mkdir -p "$dir"

# GitHub and Hugging Face answer 504 for short periods. Five tries, 15 seconds
# apart, wait out such a period. curl prints only the status, so the message
# names the URL. A failed download leaves no file, because the model check
# below takes any file as already downloaded.
download() {
    curl -fsSL --retry 5 --retry-delay 15 -o "$2" "$1" \
        || { rm -f "$2"; echo "download failed: $1" >&2; exit 1; }
}

case $engine in
llamacpp)
    build=$(pin LLAMACPP_BUILD)
    bin="$PWD/$dir/llama-$build/llama-server"
    if [ ! -x "$bin" ]; then
        download "https://github.com/ggml-org/llama.cpp/releases/download/$build/llama-$build-bin-ubuntu-x64.tar.gz" \
            "$dir/llama.tar.gz"
        tar xzf "$dir/llama.tar.gz" -C "$dir"
        rm "$dir/llama.tar.gz"
    fi
    ;;
ik)
    commit=$(pin IKLLAMA_COMMIT)
    bin="$PWD/$dir/ik-$commit/build/bin/llama-server"
    [ -x "$bin" ] || ./scripts/build-ik-llama.sh "$commit" "$PWD/$dir/ik-$commit"
    ;;
*)
    echo "unknown engine $engine (llamacpp or ik)" >&2
    exit 1
    ;;
esac

model="$dir/stories260K.gguf"
want=270cba1bd5109f42d03350f60406024560464db173c0e387d91f0426d3bd256d
if [ ! -f "$model" ]; then
    download https://huggingface.co/ggml-org/models/resolve/main/tinyllamas/stories260K.gguf "$model"
fi
echo "$want  $model" | sha256sum --check --status || { echo "unexpected hash of $model" >&2; exit 1; }

LLM_CONTRACT_TINY=1 \
LLM_CONTRACT_ENGINE="$engine" \
LLM_CONTRACT_BIN="$bin" \
LLM_CONTRACT_MODEL="$PWD/$model" \
    cargo test --locked --test llm_contract -- --ignored --test-threads=1
