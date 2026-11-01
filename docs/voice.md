# Speech to text and text to speech

`assistant-hub` offers both services over Wyoming on port 10300, the same port as its HTTP routes. Home Assistant registers both from one Wyoming Protocol integration.

| Service | Engine | Default model | Name in Home Assistant |
|---|---|---|---|
| STT | parakeet.cpp (`libparakeet.so`, inside `assistant-hub`) | Parakeet CTC 0.6B q8_0 | `parakeet` |
| TTS | audio.cpp (`audiocpp_server` on 127.0.0.1:8100) | PocketTTS English q8_0, voice `alba` | `audiocpp` |

Both models stay loaded. STT returns lowercase text without punctuation, which is enough for Home Assistant.

## How the container protects the GPU

1. STT and TTS use two Vulkan contexts: STT in `assistant-hub`, TTS in `audiocpp_server`. Both jobs start in `assistant-hub`, and a lock there lets only one of them use the GPU at a time. Two jobs at the same time can reset an old GPU.
2. If the GPU fails three times in a row ("device lost" or HTTP 500), `assistant-hub` exits with an error. Your restart policy then starts it again with a clean GPU state.
3. Before TTS, the server removes markdown from the text (stars, list dashes, headings, links) and says "degrees" for the degree sign. Small LLMs send markdown now and then, against the prompt.

The GPU lock does not cover llama-server. If two satellites talk at the same time, the LLM and the speech jobs can overlap on the GPU.

## Choose models

With more GPU memory, you can use larger models. The memory need is about the size of the STT file plus the TTS file plus 0.5 GB.

| GPU memory | STT | TTS |
|---|---|---|
| 2 GB | `ctc-0.6b-q8_0` (875 MB) | `pocket-tts-english-q8_0` (128 MB) |
| 4 GB | `ctc-1.1b-q8_0` (1.53 GB) or `tdt-0.6b-v3-q8_0` (941 MB, streaming) | `pocket-tts-english-bf16` (219 MB) |
| 6 GB or more | `tdt-1.1b-q8_0` (1.55 GB, streaming) or `ctc-0.6b-f16` (1.37 GB) | `bf16`, any language |

### Measured STT accuracy

Word error rate on 10 voice commands (music names, lights, weather, shopping list), spoken by PocketTTS. "Noisy" adds a narrow microphone band and noise at 12 dB, like a small speaker in a room:

| Model | Clean | Noisy | File |
|---|---|---|---|
| `ctc-0.6b-q4_k` | 10 % | 19 % | 610 MB |
| `ctc-0.6b-q8_0` | 11 % | 21 % | 875 MB |
| `ctc-1.1b-q8_0` | 11 % | 21 % | 1.53 GB |
| `tdt-0.6b-v2-q8_0` | 13 % | 11 % | 904 MB |
| `tdt-0.6b-v3-q8_0` | 11 % | 10 % | 941 MB |
| `tdt-1.1b-q8_0` | 7 % | 8 % | 1.55 GB |

The CTC models often lose the first word in noise ("lay" for "play"). For satellites with small microphones, use a TDT model. `tdt-1.1b-q8_0` was the only model that wrote "Linkin Park" correctly. To test on your own voice, set `STT_DEBUG_DIR`, speak some commands, and run the recordings through the models.

STT models come from [mudler/parakeet-cpp-gguf](https://huggingface.co/mudler/parakeet-cpp-gguf):

| Family | Type | q8_0 size |
|---|---|---|
| `ctc-0.6b`, `ctc-1.1b` | Classic ASR (default: 0.6b) | 875 MB, 1.53 GB |
| `tdt-0.6b-v3`, `tdt-1.1b` | Streaming ASR | 941 MB, 1.55 GB |
| `nemotron-3.5-asr-streaming-0.6b` | Streaming ASR | 984 MB |
| `realtime_eou_120m` | End-of-utterance detection | 176 MB |

Each family has the quants `f16`, `q8_0`, `q6_k`, `q5_k` and `q4_k`. A smaller quant lets a bigger family fit.

TTS models come from [audio-cpp/audio.cpp-gguf](https://huggingface.co/audio-cpp/audio.cpp-gguf). PocketTTS has about 30 English voices under `PocketTTS-GGUF/english/embeddings/`. It also has the languages `french`, `german`, `italian`, `portuguese` and `spanish`.

## Change models without a rebuild

1. Put the model files in a host folder.
2. Mount the folder over `/models`, or mount `/models/stt` alone.
3. Set the variables for the new files.

```bash
mkdir -p /srv/voice-models/stt
curl -L -o /srv/voice-models/stt/parakeet-ctc-1.1b-q8_0.gguf \
  https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-1.1b-q8_0.gguf

docker run -d --name assistant-hub \
  -v /srv/voice-models/stt:/models/stt \
  -e STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf \
  -e STT_MODEL_NAME=parakeet-ctc-1.1b \
  -e TTS_VOICE_ID=anna \
  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)" \
  -p 10300:10300 ghcr.io/schaka/homeassistant-local-voice:latest
```

`./scripts/download-models.sh` downloads the default models into `./models` and tests their size.

For another TTS language, set `TTS_LANGUAGE` (for example `german`), `TTS_LANGUAGES_BCP` (for example `de`) and the matching model and voice files.

Before each start, `assistant-hub` makes sure that every model file is complete (`assistant-hub verify-gguf FILE` runs the same check by hand). A truncated download stops the container with a clear error, instead of a crash in the native loader.

## Bake other models into the image

The Dockerfile downloads the models in a separate stage. Set the build arguments, see [configuration.md](configuration.md#build-arguments):

```bash
docker buildx bake final \
  --set '*.args.STT_MODEL_URL=https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/ctc-1.1b-q8_0.gguf' \
  --set '*.args.STT_MODEL_FILENAME=parakeet-ctc-1.1b-q8_0.gguf' \
  --set '*.args.STT_MODEL_NAME=parakeet-ctc-1.1b'
```

## GPU or CPU

At start, the server asks llama.cpp for the Vulkan GPUs (`llama-server --list-devices`). The log shows the result:

```text
INFO no Vulkan GPU found, STT and TTS run on the CPU
INFO STT device cpu, TTS backend cpu with 4 threads
```

With a GPU, STT uses the first one (`Vulkan0`) and TTS uses Vulkan device 0. Without a GPU, both use the CPU backend of ggml. Mesa's software driver llvmpipe is no option: ggml does not use it, and audio.cpp fails without a Vulkan device. `PARAKEET_DEVICE`, `TTS_BACKEND`, `VK_DEVICE` and `TTS_THREADS` override the choice ([configuration.md](configuration.md#text-to-speech-tts)).

Speech is fast on a CPU with AVX2: 0.15 to 0.25 s for STT and 0.7 s for TTS per sentence on 4 cores ([hardware.md](hardware.md#measured-speech-on-the-cpu)).

## Test without Home Assistant

The image has its own Wyoming client, so the test needs nothing else:

```bash
# Wyoming info: the services and models that Home Assistant sees
docker exec assistant-hub assistant-hub probe describe
# TTS: write a sentence to a wav file in the container
docker exec assistant-hub assistant-hub probe tts "Hello from the voice assistant." /tmp/out.wav
# STT: transcribe a wav file (any rate, it is converted to 16 kHz mono)
docker exec assistant-hub assistant-hub probe stt /tmp/out.wav
```

`--addr host:port` points the client at another server.
