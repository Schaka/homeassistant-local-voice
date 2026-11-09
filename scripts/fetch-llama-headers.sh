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
# GitHub answers 504 for short periods. Five tries, 15 seconds apart, wait out
# such a period. curl writes to a file and not into a pipe, so a retry after a
# broken transfer starts the archive anew instead of feeding tar a second copy.
url="https://github.com/ggml-org/llama.cpp/archive/refs/tags/$build.tar.gz"
archive="$dir.tar.gz"
curl -fsSL --retry 5 --retry-delay 15 -o "$archive" "$url" \
    || { rm -f "$archive"; echo "download failed: $url" >&2; exit 1; }
tar xzf "$archive" -C "$dir" --strip-components=1 --wildcards "llama.cpp-$build/include/*.h" "llama.cpp-$build/ggml/include/*.h"
rm "$archive"
echo "$dir"
