# syntax=docker/dockerfile:1
#
# assistant-hub image: one server on one port (10300) for a local Home
# Assistant voice setup. Wyoming STT and TTS on Vulkan, an optional local LLM
# (llama-server) under /llm, MCP tools on /mcp, web search (SearXNG, embedded
# as a Python library) and YouTube search.
#
# The image ships its own Mesa Vulkan drivers (RADV, ANV). Only the GPU device
# node is passed through at runtime. Without a GPU, STT and TTS run on the
# CPU backend of ggml (the server picks it at start).
#
# `docker buildx bake final` reproduces exactly what CI publishes (see
# docker-bake.hcl and .github/workflows/image.yml).

ARG RUST_VERSION=1.99.1
ARG CARGO_CHEF_VERSION=0.1.78
ARG DEBIAN_VERSION=trixie

# ============================================================================
# audio.cpp (ggml, Vulkan, PocketTTS) from source, at a pinned commit.
# Debian ships glslc, so shaderc is not built from source.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim AS audiocpp-builder
ARG AUDIOCPP_COMMIT=c0cd77fa80285f8113883e0de06ffc781cf502f6
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        git cmake ninja-build g++ python3 python3-venv ca-certificates curl \
        glslc libvulkan-dev spirv-headers \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /audio.cpp
RUN git init -q . \
    && git remote add origin https://github.com/0xShug0/audio.cpp \
    && git fetch -q --depth 1 origin "${AUDIOCPP_COMMIT}" \
    && git checkout -q FETCH_HEAD
# --native-cpu OFF: -march=native bakes in the ISA of the build machine. A
# binary built on a CPU with AVX-512 dies with SIGILL on a CPU without it.
RUN ./scripts/build_linux.sh --backend vulkan --native-cpu OFF \
        --model-set custom --models pocket_tts --target audiocpp_server

# ============================================================================
# parakeet.cpp: prebuilt Vulkan C library from the GitHub release.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim AS parakeet-stage
ARG PARAKEET_VERSION=v0.6.1
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl ca-certificates \
    && curl -fsSL "https://github.com/mudler/parakeet.cpp/releases/download/${PARAKEET_VERSION}/parakeet-${PARAKEET_VERSION}-lib-linux-vulkan-x64.tar.gz" | tar xz \
    && mv "/parakeet-${PARAKEET_VERSION}-lib-linux-vulkan-x64/libparakeet.so" /libparakeet.so \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# llama.cpp: prebuilt Vulkan release, only llama-server and its libraries.
# The headers of the same tag go to /llama-include: build.rs generates the
# Rust bindings of libllama.so (media index) from them.
# Every binary has RUNPATH $ORIGIN, so its own libggml never collides with the
# ggml inside libparakeet.so or audiocpp_server.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim AS llamacpp-stage
ARG LLAMACPP_BUILD=b12032
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl ca-certificates \
    && curl -fsSL "https://github.com/ggml-org/llama.cpp/releases/download/${LLAMACPP_BUILD}/llama-${LLAMACPP_BUILD}-bin-ubuntu-vulkan-x64.tar.gz" | tar xz \
    && mv "/llama-${LLAMACPP_BUILD}" /llama.cpp \
    && find /llama.cpp -maxdepth 1 -type f \
        \( -name 'llama*' ! -name 'llama-server' -o -name 'ggml-rpc-server' \) -delete \
    && find /llama.cpp -maxdepth 1 -name 'libllama-*-impl.so' ! -name 'libllama-server-impl.so' -delete \
    && test -x /llama.cpp/llama-server \
    && mkdir /llama-include \
    && curl -fsSL "https://github.com/ggml-org/llama.cpp/archive/refs/tags/${LLAMACPP_BUILD}.tar.gz" \
        | tar xz -C /llama-include --strip-components=1 \
            --wildcards "llama.cpp-${LLAMACPP_BUILD}/include/*.h" "llama.cpp-${LLAMACPP_BUILD}/ggml/include/*.h" \
    && test -f /llama-include/include/llama.h \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# Default STT and TTS models. The ARGs are the "bigger models" lever.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim AS models-stage
ARG STT_MODEL_URL=https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_VOICE_URL=https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/embeddings/alba.safetensors
ARG TTS_VOICE_FILENAME=alba.safetensors
ARG LLM_MODEL_URL=
ARG LLM_MODEL_FILENAME=
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl ca-certificates \
    && mkdir -p /models/stt /models/pocket-tts/embeddings /models/llm \
    && curl -fsSL --retry 3 -o "/models/stt/${STT_MODEL_FILENAME}" "${STT_MODEL_URL}" \
    && curl -fsSL --retry 3 -o "/models/pocket-tts/${TTS_MODEL_FILENAME}" "${TTS_MODEL_URL}" \
    && curl -fsSL --retry 3 -o "/models/pocket-tts/embeddings/${TTS_VOICE_FILENAME}" "${TTS_VOICE_URL}" \
    && if [ -n "${LLM_MODEL_URL}" ]; then \
        curl -fsSL --retry 3 -o "/models/llm/${LLM_MODEL_FILENAME}" "${LLM_MODEL_URL}"; \
    fi \
    && rm -rf /var/lib/apt/lists/*

# ============================================================================
# SearXNG as a library: its source plus its packages in /opt/searxng/lib, for
# the Python of the runtime base (3.13 on trixie). Its web server never runs.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim AS searxng-stage
ARG SEARXNG_COMMIT=9ded3bec1ae0b92e6abe40d27e1ca38d1906abbe
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        python3 python3-venv git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN git clone -q --filter=blob:none https://github.com/searxng/searxng.git /opt/searxng/src \
    && git -C /opt/searxng/src checkout -q "${SEARXNG_COMMIT}" \
    && python3 -m venv /tmp/pip \
    && /tmp/pip/bin/pip install --no-cache-dir --no-compile --target /opt/searxng/lib -r /opt/searxng/src/requirements.txt \
    && cd /opt/searxng/src && PYTHONPATH=/opt/searxng/src:/opt/searxng/lib python3 -m searx.version freeze \
    && rm -rf /opt/searxng/src/.git /tmp/pip \
    && find /opt/searxng -name __pycache__ -prune -exec rm -rf {} +

# ============================================================================
# assistant-hub (Rust). cargo-chef builds the dependencies in their own layer,
# so a code change rebuilds only the crate itself.
# ============================================================================
FROM lukemathwalker/cargo-chef:${CARGO_CHEF_VERSION}-rust-${RUST_VERSION}-slim-${DEBIAN_VERSION} AS chef
WORKDIR /src

FROM chef AS planner
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS rust-builder
# PyO3 links against libpython3.13 of the runtime base. bindgen (build.rs)
# needs libclang and the llama.cpp headers of the image.
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends python3-dev libclang-dev \
    && rm -rf /var/lib/apt/lists/*
ENV PYO3_PYTHON=/usr/bin/python3.13
COPY --from=llamacpp-stage /llama-include /llama-include
ENV LLAMA_INCLUDE_DIR=/llama-include
COPY --from=planner /src/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
RUN cargo build --release --locked && install -m755 target/release/assistant-hub /assistant-hub

# ============================================================================
# Runtime: Debian trixie slim. libpython3.13 is the only Python part: the hub
# embeds it for SearXNG. tini reaps zombies and forwards signals.
# ============================================================================
FROM debian:${DEBIAN_VERSION}-slim
ARG STT_MODEL_FILENAME=parakeet-ctc-0.6b-q8_0.gguf
ARG TTS_MODEL_FILENAME=pocket-tts-english-q8_0.gguf
ARG TTS_LANGUAGE=english
ARG TTS_MODEL_ID=pocket-tts
ARG TTS_VOICE_ID=alba
ARG STT_MODEL_NAME=parakeet-ctc-0.6b
ARG LLM_MODEL_FILENAME=
ARG LLM_FAMILY=
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates tini mesa-vulkan-drivers libvulkan1 libgomp1 libpython3.13 libssl3t64 libcurl4t64 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_server /usr/local/bin/audiocpp_server
COPY --from=parakeet-stage /libparakeet.so /usr/lib/libparakeet.so
COPY --from=llamacpp-stage /llama.cpp /opt/llama.cpp
# Fail the build here, not at runtime, if a shared library is missing.
RUN ! ldd /opt/llama.cpp/llama-server /opt/llama.cpp/*.so /usr/local/bin/audiocpp_server /usr/lib/libparakeet.so \
        | grep 'not found'
COPY --from=models-stage /models /models
COPY --from=searxng-stage /opt/searxng /opt/searxng
COPY --from=rust-builder /assistant-hub /usr/local/bin/assistant-hub
# The model spec of the same audio.cpp commit as the binary.
COPY --from=audiocpp-builder /audio.cpp/model_specs/pocket_tts.json /app/model_specs/pocket_tts.json

ENV STT_MODEL_FILENAME=${STT_MODEL_FILENAME} \
    TTS_MODEL_FILENAME=${TTS_MODEL_FILENAME} \
    TTS_LANGUAGE=${TTS_LANGUAGE} \
    TTS_MODEL_ID=${TTS_MODEL_ID} \
    TTS_VOICE_ID=${TTS_VOICE_ID} \
    STT_MODEL_NAME=${STT_MODEL_NAME} \
    LLM_MODEL_FILENAME=${LLM_MODEL_FILENAME} \
    LLM_FAMILY=${LLM_FAMILY}

# audiocpp_server finds model_specs/ relative to the working directory.
WORKDIR /app
# Everything on one port: Wyoming (STT, TTS) and HTTP (/llm, /mcp, /web, /youtube).
EXPOSE 10300
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s CMD ["assistant-hub", "healthcheck"]
ENTRYPOINT ["tini", "--", "assistant-hub"]
CMD ["serve"]
