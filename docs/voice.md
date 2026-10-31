# Speech to text and text to speech

One Wyoming server (`wyoming_voice.py`) on port 10300 offers both services. Home Assistant registers both from one Wyoming Protocol integration.

| Service | Engine | Default model | Name in Home Assistant |
|---|---|---|---|
| STT | parakeet.cpp (`libparakeet.so`) | Parakeet CTC 0.6B q8_0 | `parakeet` |
| TTS | audio.cpp (`audiocpp_server` on 127.0.0.1:8100) | PocketTTS English q8_0, voice `alba` | `audiocpp` |

Both models stay loaded. STT returns lowercase text without punctuation, which is enough for Home Assistant.

## How the container protects the GPU

1. STT and TTS use two Vulkan contexts in two processes. A file lock lets only one of them use the GPU at a time. Two jobs at the same time can reset an old GPU.
2. If the GPU fails three times in a row ("device lost" or HTTP 500), `wyoming_voice.py` stops the container. Your restart policy then starts it again with a clean GPU state.
3. Before TTS, the server removes markdown from the text (stars, list dashes, headings, links) and says "degrees" for the degree sign. Small LLMs send markdown now and then, against the prompt.

The GPU lock does not cover llama-server. If two satellites talk at the same time, the LLM and the speech jobs can overlap on the GPU.

## Choose models

With more GPU memory, you can use larger models. The memory need is about the size of the STT file plus the TTS file plus 0.5 GB.

| GPU memory | STT | TTS |
|---|---|---|
| 2 GB | `ctc-0.6b-q8_0` (875 MB) | `pocket-tts-english-q8_0` (128 MB) |
| 4 GB | `ctc-1.1b-q8_0` (1.53 GB) or `tdt-0.6b-v3-q8_0` (941 MB, streaming) | `pocket-tts-english-bf16` (219 MB) |
| 6 GB or more | `tdt-1.1b-q8_0` (1.55 GB, streaming) or `ctc-0.6b-f16` (1.37 GB) | `bf16`, any language |

STT models come from [mudler/parakeet-cpp-gguf](https://huggingface.co/mudler/parakeet-cpp-gguf):

| Family | Type | q8_0 size |
|---|---|---|
| `ctc-0.6b`, `ctc-1.1b` | Classic ASR (default: 0.6b) | 875 MB, 1.53 GB |
| `tdt-0.6b-v3`, `tdt-1.1b` | Streaming ASR | 941 MB, 1.55 GB |
| `nemotron-3.5-asr-streaming-0.6b` | Streaming ASR | 984 MB |
| `realtime_eou_120m` | End-of-utterance detection | 176 MB |

Each family has the quants `f16`, `q8_0`, `q6_k`, `q5_k` and `q4_k`. A smaller quant lets a bigger family fit.

TTS models come from [audio-cpp/audio.cpp-gguf](https://huggingface.co/audio-cpp/audio.cpp-gguf). PocketTTS has about 30 English voices under `PocketTTS-GGUF/english/embeddings/`. It also has the languages `german`, `italian`, `portuguese` and `spanish`.

## Change models without a rebuild

1. Put the model files in a host folder.
2. Mount the folder over `/models`, or mount `/models/stt` alone.
3. Set the variables for the new files.

```bash
mkdir -p /srv/voice-models/stt
curl -L -o /srv/voice-models/stt/parakeet-ctc-1.1b-q8_0.gguf \
  https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-1.1b-q8_0.gguf

docker run -d --name wyoming-voice \
  -v /srv/voice-models/stt:/models/stt \
  -e STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf \
  -e STT_MODEL_NAME=parakeet-ctc-1.1b \
  -e TTS_VOICE_ID=anna \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 ghcr.io/schaka/homeassistant-local-voice:latest
```

`./scripts/download-models.sh` downloads the default models into `./models` and tests their size.

For another TTS language, set `TTS_LANGUAGE` (for example `german`), `TTS_LANGUAGES_BCP` (for example `de`) and the matching model and voice files.

Before each start, `verify_gguf.py` makes sure that every model file is complete. A truncated download stops the container with a clear error, instead of a crash in the native loader.

## Bake other models into the image

The Dockerfile downloads the models in a separate stage. Set the build arguments, see [configuration.md](configuration.md#build-arguments):

```bash
docker buildx bake final \
  --set '*.args.STT_MODEL_URL=https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-1.1b-q8_0.gguf' \
  --set '*.args.STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf' \
  --set '*.args.STT_MODEL_NAME=parakeet-ctc-1.1b'
```

## Podman

Rootless podman needs some more flags. Replace `105` with the group ID of `/dev/dri/renderD128` on your host (`stat -c %g /dev/dri/renderD128`):

```bash
podman run -d --name wyoming-voice \
  --device /dev/dri/renderD128 --group-add 105 \
  --security-opt seccomp:unconfined --security-opt label=disable --ipc host \
  -p 10300:10300 --restart unless-stopped \
  ghcr.io/schaka/homeassistant-local-voice:latest
```

## GPU selection

The image finds the GPU through Mesa. `PARAKEET_DEVICE` (default `Vulkan0`) selects the GPU for STT, and `VK_DEVICE` (default `0`) selects it for TTS. Without a GPU, Vulkan device 0 is llvmpipe on the CPU.

## Test without Home Assistant

```bash
# STT: send a 16 kHz mono wav, print the text
python3 scripts/test/wyoming_stt_client.py clip16k.wav
# TTS: write a sentence to a wav file
python3 scripts/test/wyoming_tts_client.py "Hello from the voice assistant." out.wav
```

Both clients connect to port 10300.
