#!/usr/bin/env bash
# Builds llama-server of ik_llama.cpp at a pinned commit as one binary for any
# CPU with AVX2 and FMA. The Dockerfile and CI both call it, so they build the
# same program.
#
#   scripts/build-ik-llama.sh COMMIT FOLDER     # result: FOLDER/build/bin/llama-server
#
# -march=native bakes the instruction set of the build machine into the binary.
# A binary built on a CPU with AVX-512 dies with SIGILL on a CPU without it.
# So the build names the instruction sets: AVX2, FMA, F16C and BMI2. ik_llama.cpp
# chooses its fastest code at compile time, so a build for more instruction sets
# is a separate build.
set -euo pipefail
commit=${1:?commit of ik_llama.cpp}
dir=${2:?folder for the source and the build}
mkdir -p "$dir"
if [ ! -d "$dir/.git" ]; then
    git -C "$dir" init -q
    git -C "$dir" remote add origin https://github.com/ikawrakow/ik_llama.cpp
fi
git -C "$dir" fetch -q --depth 1 origin "$commit"
git -C "$dir" checkout -q FETCH_HEAD
cmake -S "$dir" -B "$dir/build" -DCMAKE_BUILD_TYPE=Release \
    -DGGML_NATIVE=OFF -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_BMI2=ON -DGGML_AVX512=OFF \
    -DLLAMA_CURL=OFF -DBUILD_SHARED_LIBS=OFF
cmake --build "$dir/build" --config Release -j "$(nproc)" --target llama-server
test -x "$dir/build/bin/llama-server"
