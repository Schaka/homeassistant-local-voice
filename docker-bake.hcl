# Build graph for the Wyoming voice image. One target (`final`), one
# Dockerfile. Bake exists so every version-shaped knob -- registry, tags,
# cache refs, and the model build args -- lives here, and CI (and local
# `docker buildx bake`) just hands variables over, same pattern as the
# rocm-migraphx-ort-builder pipeline.
#
# Local use:
#   docker buildx bake final                          # build, default models
#   docker buildx bake final --set '*.args.STT_MODEL_URL=...'   # bigger STT
#   docker buildx bake --print final                  # resolved graph, no build
#
# CI use: set the variables below from the environment and name the target.
# REGISTRY_CACHE is CI-only (exporting needs write access to the packages).

# ---------------------------------------------------------------------------
# Registry / naming
# ---------------------------------------------------------------------------

variable "REGISTRY" { default = "ghcr.io" }

# Registry build cache is CI-only. Set REGISTRY_CACHE=true in CI; leave local.
variable "REGISTRY_CACHE" { default = "false" }

# The component this run is actually building. Exactly one cache export per
# job: only the named target exports, anything it builds as a dependency only
# reads.
variable "CACHE_TARGET" { default = "" }

function "cache_ref" {
  params = [component]
  result = "${REGISTRY}/${OWNER}/homeassistant-local-voice:cache-${component}"
}

function "cache_from" {
  params = [component]
  result = REGISTRY_CACHE == "true" ? ["type=registry,ref=${cache_ref(component)}"] : []
}

function "cache_to" {
  params = [component]
  result = REGISTRY_CACHE == "true" && CACHE_TARGET == component ? ["type=registry,ref=${cache_ref(component)},mode=max"] : []
}

# Lowercased repository owner. CI passes ${GITHUB_REPOSITORY_OWNER,,}.
variable "OWNER" { default = "schaka" }

# YYYYMMDD, for the dated nightly-style tags. Empty (local default) publishes
# no dated tag.
variable "DATE" { default = "" }

# Stable pointer tag (e.g. "v1.0.0"), published in addition to latest+dated.
# Empty = not published.
variable "RELEASE_TAG" { default = "" }

# ---------------------------------------------------------------------------
# Model build args -- the "bigger models" lever. Defaults are the 2 GB-GPU
# friendly set verified on an R7 250. Override to bake any other gguf /
# embedding from the same repos (README: "Using bigger models").
# ---------------------------------------------------------------------------

variable "STT_MODEL_URL" { default = "https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-0.6b-q8_0.gguf" }
variable "STT_MODEL_FILENAME" { default = "parakeet-ctc-0.6b-q8_0.gguf" }
variable "TTS_MODEL_URL" { default = "https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/pocket-tts-english-q8_0.gguf" }
variable "TTS_MODEL_FILENAME" { default = "pocket-tts-english-q8_0.gguf" }
variable "TTS_VOICE_URL" { default = "https://huggingface.co/audio-cpp/audio.cpp-gguf/resolve/main/PocketTTS-GGUF/english/embeddings/alba.safetensors" }
variable "TTS_VOICE_FILENAME" { default = "alba.safetensors" }
variable "TTS_LANGUAGE" { default = "english" }
variable "TTS_MODEL_ID" { default = "pocket-tts" }
variable "TTS_VOICE_ID" { default = "alba" }
variable "STT_MODEL_NAME" { default = "parakeet-ctc-0.6b" }

# ---------------------------------------------------------------------------
# Target
# ---------------------------------------------------------------------------

target "final" {
  context = "."
  dockerfile = "Dockerfile"
  tags = concat(
    ["${REGISTRY}/${OWNER}/homeassistant-local-voice:latest"],
    DATE != "" ? ["${REGISTRY}/${OWNER}/homeassistant-local-voice:${DATE}"] : [],
    RELEASE_TAG != "" ? ["${REGISTRY}/${OWNER}/homeassistant-local-voice:${RELEASE_TAG}"] : []
  )
  args = {
    STT_MODEL_URL      = "${STT_MODEL_URL}"
    STT_MODEL_FILENAME = "${STT_MODEL_FILENAME}"
    TTS_MODEL_URL      = "${TTS_MODEL_URL}"
    TTS_MODEL_FILENAME = "${TTS_MODEL_FILENAME}"
    TTS_VOICE_URL      = "${TTS_VOICE_URL}"
    TTS_VOICE_FILENAME = "${TTS_VOICE_FILENAME}"
    TTS_LANGUAGE       = "${TTS_LANGUAGE}"
    TTS_MODEL_ID       = "${TTS_MODEL_ID}"
    TTS_VOICE_ID       = "${TTS_VOICE_ID}"
    STT_MODEL_NAME     = "${STT_MODEL_NAME}"
  }
  cache-from = cache_from("final")
  cache-to = cache_to("final")
  platforms = ["linux/amd64"]
}