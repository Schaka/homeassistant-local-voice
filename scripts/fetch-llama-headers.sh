#!/usr/bin/env bash
# Downloads the llama.cpp headers of LLAMACPP_BUILD (docker-bake.hcl) to
# target/llama.cpp-<build>, where build.rs finds them. Needed once for a
# local build or test outside the image. The image build downloads the same
# headers itself (Dockerfile, llamacpp-stage).
set -euo pipefail
cd "$(dirname "$0")/.."
build=$(sed -n 's/^variable "LLAMACPP_BUILD" { default = "\(.*\)" }/\1/p' docker-bake.hcl)
dir="target/llama.cpp-$build"
if [ -f "$dir/include/llama.h" ]; then
    echo "$dir"
    exit 0
fi
mkdir -p "$dir"
curl -fsSL "https://github.com/ggml-org/llama.cpp/archive/refs/tags/$build.tar.gz" \
    | tar xz -C "$dir" --strip-components=1 --wildcards "llama.cpp-$build/include/*.h" "llama.cpp-$build/ggml/include/*.h"
echo "$dir"
