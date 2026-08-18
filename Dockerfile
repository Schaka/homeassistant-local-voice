# syntax=docker/dockerfile:1
#
# Self-contained Wyoming STT + TTS for Home Assistant, GPU-accelerated via
# Vulkan. Ships its own Mesa Vulkan drivers (radv for AMD -- including ancient
# GCN1 like the R7 250 -- plus llvmpipe for CPU-only hosts), so there is no
# dependency on ROCm, CUDA, or a host-installed driver. Only the GPU device
# node and render group are passed through at runtime.
#
# The image is the single buildable artifact: `docker buildx bake final`
# reproduces exactly what CI publishes to ghcr.io (see docker-bake.hcl and
# .github/workflows/build.yml).

# ============================================================================
# Stage 1: audio.cpp (ggml, Vulkan backend, PocketTTS) built from source.
# ggml-vulkan needs glslc (shaderc); Ubuntu ships no binary, so it is built
# from shaderc source first. Slow (~30-60 min) -- CI's registry cache is what
# makes this a no-op on rebuilds.
# ============================================================================
FROM ubuntu:24.04 AS audiocpp-builder
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        git cmake ninja-build gcc g++ python3 curl ca-certificates spirv-headers \
        libvulkan-dev python3-venv \
    && rm -rf /var/lib/apt/lists/*
RUN git clone --depth 1 https://github.com/google/shaderc /shaderc \
    && cd /shaderc \
    && ./utils/git-sync-deps \
    && cmake -GNinja -B build -DCMAKE_BUILD_TYPE=Release \
        -DSHADERC_SKIP_TESTS=ON -DSHADERC_SKIP_EXAMPLES=ON -DSHADERC_SKIP_DOCS=ON \
    && ninja -C build glslc_exe \
    && install -m755 build/glslc/glslc /usr/local/bin/glslc \
    && glslc --version | head -1
RUN git clone --depth 1 https://github.com/0xShug0/audio.cpp /audio.cpp
WORKDIR /audio.cpp
RUN ./scripts/build_linux.sh --backend vulkan --model-set custom --models pocket_tts \
        --target audiocpp_cli --target audiocpp_server

# ============================================================================
# Stage 2: parakeet.cpp prebuilt Vulkan binary + C-API from the GitHub release.
# ============================================================================
FROM ubuntu:24.04 AS parakeet-stage
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        curl ca-certificates \
    && curl -sL https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-lib-linux-vulkan-x64.tar.gz | tar xz \
    && curl -sL https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-bin-linux-vulkan-x64.tar.gz | tar xz \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Stage 3: model download. The ARGs are the "bigger models" lever -- override
# them (CLI, docker-bake.hcl, or the CI workflow) to bake any other gguf /
# embedding from the same repos into the image. See README.
# ============================================================================
FROM ubuntu:24.04 AS models-stage
ARG STT_MODEL_URL=https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_VOICE_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/embeddings/alba.safetensors
ARG TTS_VOICE_FILENAME=alba.safetensors
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        curl ca-certificates \
    && mkdir -p /models/stt /models/pocket-tts/embeddings \
    && curl -sL --retry 3 -o "/models/stt/${STT_MODEL_FILENAME}" "${STT_MODEL_URL}" \
    && curl -sL --retry 3 -o "/models/pocket-tts/${TTS_MODEL_FILENAME}" "${TTS_MODEL_URL}" \
    && curl -sL --retry 3 -o "/models/pocket-tts/embeddings/${TTS_VOICE_FILENAME}" "${TTS_VOICE_URL}" \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Stage 4: runtime. Ubuntu 26.04 is required: current Mesa builds need
# GLIBC_ABI_GNU2_TLS (glibc >= 2.41); 24.04 cannot load them.
# ============================================================================
FROM ubuntu:26.04
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_LANGUAGE=english
ARG TTS_MODEL_ID=pocket-tts
ARG TTS_VOICE_ID=alba
ARG STT_MODEL_NAME=parakeet-ctc-0.6b
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        python3 python3-pip ca-certificates curl mesa-vulkan-drivers libgomp1 \
    && pip3 install --break-system-packages --no-cache-dir wyoming numpy \
    && rm -rf /var/lib/apt/lists/*

COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_cli /usr/local/bin/audiocpp_cli
COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_server /usr/local/bin/audiocpp_server
COPY --from=parakeet-stage /parakeet-v0.5.0-lib-linux-vulkan-x64/libparakeet.so /usr/lib/libparakeet.so
COPY --from=parakeet-stage /parakeet-v0.5.0-bin-linux-vulkan-x64/parakeet-cli /usr/local/bin/parakeet-cli
COPY --from=models-stage /models /models

COPY model_specs /app/model_specs
COPY wyoming_voice.py verify_gguf.py entrypoint.sh /app/
RUN chmod +x /app/entrypoint.sh

# Defaults, all overridable at runtime (env) -- see entrypoint.sh.
ENV STT_MODEL_FILENAME=${STT_MODEL_FILENAME:-parakeet-ctc-0.6b-q8_0.gguf} \
    TTS_MODEL_FILENAME=${TTS_MODEL_FILENAME:-pocket-tts-english-q8_0.gguf} \
    TTS_LANGUAGE=${TTS_LANGUAGE} \
    TTS_MODEL_ID=${TTS_MODEL_ID} \
    TTS_VOICE_ID=${TTS_VOICE_ID} \
    STT_MODEL_NAME=${STT_MODEL_NAME}

EXPOSE 10300
ENTRYPOINT ["/app/entrypoint.sh"]