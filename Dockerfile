# Stage 1: audio.cpp (Vulkan, PocketTTS) - built with glslc from source
FROM audiocpp-vk-build AS audiocpp-builder
# binaries land in /audio.cpp/build/linux-vulkan-release/bin/

# Stage 2: parakeet.cpp prebuilt Vulkan binaries + C-API
FROM ubuntu:24.04 AS parakeet-stage
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends curl ca-certificates \
 && curl -sL https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-lib-linux-vulkan-x64.tar.gz | tar xz \
 && curl -sL https://github.com/mudler/parakeet.cpp/releases/download/v0.5.0/parakeet-v0.5.0-bin-linux-vulkan-x64.tar.gz | tar xz \
 && rm -rf /var/lib/apt/lists/*

# Stage 3: runtime
FROM ubuntu:26.04
# Userspace deps for the host RADV driver + runtime
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    python3 python3-pip ca-certificates libgomp1 \
    libdrm-amdgpu1 libdrm2 libelf1t64 libexpat1 libffi8 liblzma5 libudev1 \
    libwayland-client0 libx11-xcb1 libxau6 libxcb1 libxcb-dri3-0 \
    libxcb-present0 libxcb-randr0 libxcb-shm0 libxcb-sync1 libxcb-xfixes0 \
    libxshmfence1 libxml2-16 zlib1g libzstd1 \
 && pip3 install --break-system-packages --no-cache-dir wyoming numpy \
 && rm -rf /var/lib/apt/lists/*

COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_cli /usr/local/bin/audiocpp_cli
COPY --from=audiocpp-builder /audio.cpp/build/linux-vulkan-release/bin/audiocpp_server /usr/local/bin/audiocpp_server
COPY --from=parakeet-stage /parakeet-v0.5.0-lib-linux-vulkan-x64/libparakeet.so /usr/lib/libparakeet.so
COPY --from=parakeet-stage /parakeet-v0.5.0-bin-linux-vulkan-x64/parakeet-cli /usr/local/bin/parakeet-cli

# Host RADV driver + its Fedora-soname deps, baked (matches this host)
COPY vk-libs/ /opt/vk/
COPY radeon_icd.container.json /opt/vk/radeon_icd.container.json
RUN echo /opt/vk > /etc/ld.so.conf.d/vk.conf && ldconfig 2>/dev/null || true
ENV VK_ICD_FILENAMES=/opt/vk/radeon_icd.container.json

# audio.cpp model specs (needed at runtime by audiocpp_server)
COPY model_specs /app/model_specs

# Models baked in: English STT (Parakeet CTC 0.6B q8) + TTS (PocketTTS q8)
COPY models/stt /models/stt
COPY models/pocket-tts /models/pocket-tts

COPY wyoming_stt.py wyoming_tts.py server.json entrypoint.sh /app/
RUN chmod +x /app/entrypoint.sh

EXPOSE 10300 10301
ENTRYPOINT ["/app/entrypoint.sh"]