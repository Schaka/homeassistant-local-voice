# STT cleanup with Hush

`STT_DENOISE=hush` cleans the audio of each STT request before Parakeet hears it. [Hush](https://github.com/pulp-vision/Hush) by Weya removes noise and a competing speaker, for example a TV in the room. It is off by default.

## What it does

A satellite streams the speech after the wake word to the hub. With a TV on, that audio holds the command and the TV voices. Parakeet then writes the TV words into the command, or it finds no command at all. Hush is a [DeepFilterNet3](https://github.com/Rikorose/DeepFilterNet) model that Weya trained on mixes with a second speaker at 12 to 24 dB below the first one. It keeps the main voice and lowers the rest. It works for every language, because it works on the sound and not on the words.

Hush does not make Parakeet understand other languages, and it cannot remove a background voice that is louder than the user. Its model card says that very loud competing speakers may stay.

## How it runs

1. At start, the hub loads `libweya_nc.so` and the model bundle once (`src/denoise.rs`). A failure leaves STT on the raw audio and writes an error to the log.
2. At `audio-stop`, the hub converts the request to 16 kHz mono and gives it to Hush in 10 ms frames of 160 samples. Each request gets its own session, so two satellites never wait for each other.
3. Hush runs on the CPU, before the GPU lock. TTS never waits for it.
4. DeepFilterNet delays its output by one frame. The hub removes that delay, so the cleaned audio has the length and the timing of the input.
5. If Hush fails on a request, STT hears the raw audio of that request.

The log shows each run, for example `STT cleanup: 2.4 s of audio in <time> ms`. `/health` says `"stt_denoise": true` when Hush is loaded.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `STT_DENOISE` | `off` | `hush` turns the cleanup on |
| `STT_DENOISE_ATTEN_DB` | `100` | The most that Hush may lower any part of the audio, in dB. 100 is no limit. A lower value, for example 30, leaves more background and changes the voice less. |
| `STT_DENOISE_MODEL` | `/opt/hush/hush-dfnet16k-onnx.tar.gz` | The model bundle |
| `STT_DENOISE_LIB` | `/usr/lib/libweya_nc.so` | The library |

The image contains both files outside `/models`, so a mounted model folder does not hide them.

## Check it on your rooms

Hush changes what STT hears, so test it with your own satellites before you leave it on.

1. Set `STT_DEBUG_DIR` and `STT_DENOISE=hush`. Each request then gives three files: the raw audio (`*.wav`), the audio that STT heard (`*-clean.wav`) and the transcript.
2. Speak the usual commands with the TV on and with the TV off.
3. Listen to the pairs and compare the transcripts. Run a raw file through STT with `assistant-hub probe stt FILE.wav` to see the transcript without Hush.
4. If Hush removes parts of the command, set `STT_DENOISE_ATTEN_DB=30`. If the TV still comes through, it is louder than Hush can handle at that distance. Then turn the TV down while the satellite listens, for example with an automation that mutes the receiver.
5. Turn `STT_DEBUG_DIR` off again. The recordings hold everything that people say after the wake word.

## Build

The Dockerfile stage `hush-builder` builds `libweya_nc.so` from `native/weya_nc_build/weya_nc` of the Hush commit `HUSH_COMMIT`. The library wraps DeepFilterNet's `df` crate, which runs the ONNX model with [tract](https://github.com/sonos/tract) in pure Rust. The image needs no ONNX Runtime and no Python for it. The stage downloads the bundle `onnx/advanced_dfnet16k_model_best_onnx.tar.gz` of `weya-ai/hush` at `HUSH_MODEL_REVISION` and checks it against `HUSH_MODEL_SHA256`.

`scripts/bump-pins.sh` updates `HUSH_COMMIT`. A new model revision needs a new SHA-256 and a new test on recordings, so a person changes it.

## Tests

1. Unit tests in `src/denoise.rs` check the frames and the removal of the delay with a fake filter, and the PCM conversion.
2. The ignored test `hush_cleans_a_recording` runs the real library over a WAV file and prints the time:

```bash
HUSH_LIB=/usr/lib/libweya_nc.so HUSH_MODEL=/opt/hush/hush-dfnet16k-onnx.tar.gz \
HUSH_IN=in.wav HUSH_OUT=out.wav \
cargo test --release hush_cleans_a_recording -- --ignored --nocapture
```

## Why this design

1. A library in the process, as for parakeet.cpp. A filter in front of STT needs no own port and no second container (rule 1 of `AGENTS.md`).
2. The upstream C wrapper of Hush, built from its source at a pinned commit. Its own `Cargo.lock` keeps the older `tract` crates out of the hub's dependencies.
3. On the hub and not on each satellite. One copy cleans the audio of every satellite, and the satellites stay small (Atom Echo, thin clients).
4. Off by default, because a speech filter can remove parts of a command, and only recordings from the user's rooms show whether it helps there.
