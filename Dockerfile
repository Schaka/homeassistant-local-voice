# syntax=docker/dockerfile:1
#
# Self-contained Wyoming STT + TTS for Home Assistant, GPU-accelerated via
# Vulkan, plus an optional local conversation agent (llama-server). Ships its
# own Mesa Vulkan drivers (radv for AMD -- including ancient
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
# --native-cpu OFF: the default (-march=native) bakes in the ISA of the build
# machine. A CI runner with AVX-512 produced a binary that died with SIGILL in
# ggml_cpu_init on CPUs without it (i5-12400T). OFF gives the portable ggml
# baseline (AVX2/FMA/F16C), which is enough for the small CPU part of TTS.
RUN ./scripts/build_linux.sh --backend vulkan --native-cpu OFF \
        --model-set custom --models pocket_tts \
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
# Stage 3: llama.cpp prebuilt Vulkan release, for the optional conversation
# agent (llama-server, OpenAI-compatible /v1). Every binary has RUNPATH
# $ORIGIN, so the directory moves as one unit and its own libggml never
# collides with the ggml inside libparakeet.so / audiocpp_server. Only
# llama-server and its libraries are kept.
# ============================================================================
FROM ubuntu:24.04 AS llamacpp-stage
ARG LLAMACPP_BUILD=b11966
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        curl ca-certificates \
    && curl -sL "https://github.com/ggml-org/llama.cpp/releases/download/${LLAMACPP_BUILD}/llama-${LLAMACPP_BUILD}-bin-ubuntu-vulkan-x64.tar.gz" | tar xz \
    && mv "/llama-${LLAMACPP_BUILD}" /llama.cpp \
    && find /llama.cpp -maxdepth 1 -type f \
        \( -name 'llama*' ! -name 'llama-server' -o -name 'ggml-rpc-server' \) -delete \
    && find /llama.cpp -maxdepth 1 -name 'libllama-*-impl.so' ! -name 'libllama-server-impl.so' -delete \
    && test -x /llama.cpp/llama-server \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Stage 4: model download. The ARGs are the "bigger models" lever -- override
# them (CLI, docker-bake.hcl, or the CI workflow) to bake any other gguf /
# embedding from the same repos into the image. See README. The LLM is
# optional: an empty LLM_MODEL_URL (default) bakes none.
# ============================================================================
FROM ubuntu:24.04 AS models-stage
ARG STT_MODEL_URL=https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_VOICE_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/embeddings/alba.safetensors
ARG TTS_VOICE_FILENAME=alba.safetensors
ARG LLM_MODEL_URL=
ARG LLM_MODEL_FILENAME=
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        curl ca-certificates \
    && mkdir -p /models/stt /models/pocket-tts/embeddings /models/llm \
    && curl -sL --retry 3 -o "/models/stt/${STT_MODEL_FILENAME}" "${STT_MODEL_URL}" \
    && curl -sL --retry 3 -o "/models/pocket-tts/${TTS_MODEL_FILENAME}" "${TTS_MODEL_URL}" \
    && curl -sL --retry 3 -o "/models/pocket-tts/embeddings/${TTS_VOICE_FILENAME}" "${TTS_VOICE_URL}" \
    && if [ -n "${LLM_MODEL_URL}" ]; then \
        curl -sL --retry 3 -o "/models/llm/${LLM_MODEL_FILENAME}" "${LLM_MODEL_URL}"; \
    fi \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Stage 4b: SearXNG (optional web search, SEARCH_ENABLED=on). Built on the
# runtime base so the venv's Python matches. Pinned commit, git metadata and
# caches removed. About 100 MB; it only runs when enabled.
# ============================================================================
FROM ubuntu:26.04 AS searxng-stage
ARG SEARXNG_COMMIT=ff8f5ddcc9f2c58d8b0276ec030f6a3adfa17525
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        python3 python3-venv git ca-certificates \
    && git clone --filter=blob:none https://github.com/searxng/searxng.git /opt/searxng/src \
    && git -C /opt/searxng/src checkout "${SEARXNG_COMMIT}" \
    && python3 -m venv /opt/searxng/venv \
    && /opt/searxng/venv/bin/pip install --no-cache-dir -r /opt/searxng/src/requirements.txt \
    && cd /opt/searxng/src && /opt/searxng/venv/bin/python -m searx.version freeze \
    && rm -rf /opt/searxng/src/.git \
    && SEARXNG_SECRET=build-check /opt/searxng/venv/bin/python -c "import searx.webapp" \
    && find /opt/searxng -name __pycache__ -prune -exec rm -rf {} + \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Stage 5: runtime. Ubuntu 26.04 is required: current Mesa builds need
# GLIBC_ABI_GNU2_TLS (glibc >= 2.41); 24.04 cannot load them.
# ============================================================================
FROM ubuntu:26.04
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_LANGUAGE=english
ARG TTS_MODEL_ID=pocket-tts
ARG TTS_VOICE_ID=alba
ARG STT_MODEL_NAME=parakeet-ctc-0.6b
ARG LLM_MODEL_FILENAME=
ARG LLM_FAMILY=minicpm5
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        python3 python3-pip ca-certificates curl mesa-vulkan-drivers libgomp1 \
    && pip3 install --break-system-packages --no-cache-dir wyoming numpy \
    && rm -rf /var/lib/apt/lists/*

COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_cli /usr/local/bin/audiocpp_cli
COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_server /usr/local/bin/audiocpp_server
COPY --from=parakeet-stage /parakeet-v0.5.0-lib-linux-vulkan-x64/libparakeet.so /usr/lib/libparakeet.so
COPY --from=parakeet-stage /parakeet-v0.5.0-bin-linux-vulkan-x64/parakeet-cli /usr/local/bin/parakeet-cli
COPY --from=llamacpp-stage /llama.cpp /opt/llama.cpp
# The release is built on an older Ubuntu; fail the build here, not at
# runtime, if a shared library it needs (libvulkan, libssl, ...) is missing.
RUN ! ldd /opt/llama.cpp/llama-server /opt/llama.cpp/*.so | grep 'not found'
COPY --from=models-stage /models /models
COPY --from=searxng-stage /opt/searxng /opt/searxng

COPY model_specs /app/model_specs
COPY wyoming_voice.py verify_gguf.py llm_slot_cache.py llm_guard.py web_search_mcp.py entrypoint.sh /app/
RUN chmod +x /app/entrypoint.sh

# Defaults, all overridable at runtime (env) -- see entrypoint.sh.
ENV STT_MODEL_FILENAME=${STT_MODEL_FILENAME:-parakeet-ctc-0.6b-q8_0.gguf} \
    TTS_MODEL_FILENAME=${TTS_MODEL_FILENAME:-pocket-tts-english-q8_0.gguf} \
    TTS_LANGUAGE=${TTS_LANGUAGE} \
    TTS_MODEL_ID=${TTS_MODEL_ID} \
    TTS_VOICE_ID=${TTS_VOICE_ID} \
    STT_MODEL_NAME=${STT_MODEL_NAME} \
    LLM_MODEL_FILENAME=${LLM_MODEL_FILENAME} \
    LLM_FAMILY=${LLM_FAMILY}

# 10300: Wyoming (STT + TTS). 8080: llama-server, only when an LLM is set.
# 8090: web search MCP endpoint, only with SEARCH_ENABLED=on.
EXPOSE 10300 8080 8090
ENTRYPOINT ["/app/entrypoint.sh"]