#!/usr/bin/env python3
"""Single Wyoming server advertising BOTH STT and TTS on one port.

- STT: parakeet.cpp (ggml, Vulkan) via its flat C API, model loaded once
  (resident in VRAM), full-utterance transcription.
- TTS: audio.cpp (ggml, Vulkan) via audiocpp_server's OpenAI-style REST
  endpoint (model resident after first request).

Speaks the Wyoming protocol so Home Assistant registers both a Speech-to-Text
and a Text-to-Speech service from ONE integration.

Run:
    wyoming_voice.py --uri tcp://0.0.0.0:10300 \
        --stt-model /models/stt/parakeet-ctc-0.6b-q8_0.gguf \
        --stt-lib /usr/lib/libparakeet.so \
        --audio-cpp-url http://127.0.0.1:8100 --tts-model-id pocket-tts
"""
import argparse
import asyncio
import ctypes
import io
import json
import logging
import os
import threading
import urllib.request
import wave
from pathlib import Path

import numpy as np

from wyoming.asr import Transcribe, Transcript
from wyoming.audio import AudioChunk, AudioChunkConverter, AudioStart, AudioStop
from wyoming.event import Event
from wyoming.info import (
    AsrModel,
    AsrProgram,
    Attribution,
    Describe,
    Info,
    TtsProgram,
    TtsVoice,
)
from wyoming.server import AsyncEventHandler, AsyncServer
from wyoming.tts import Synthesize, SynthesizeStop

_LOGGER = logging.getLogger(__name__)

_RATE = 16000
_WIDTH = 2
_CHANNELS = 1
_CHUNK_SAMPLES = 4096


# ---------------------------------------------------------------------------
# STT: parakeet.cpp
# ---------------------------------------------------------------------------
class ParakeetSTT:
    """Thread-safe wrapper around a single resident parakeet.cpp context."""

    def __init__(self, lib_path: Path, model_path: Path, device: str):
        os.environ["PARAKEET_DEVICE"] = device
        if not lib_path.is_file():
            raise RuntimeError(f"libparakeet not found: {lib_path}")
        if not model_path.is_file():
            raise RuntimeError(f"model not found: {model_path}")

        self._lib = ctypes.CDLL(str(lib_path))
        lib = self._lib

        lib.parakeet_capi_load.argtypes = [ctypes.c_char_p]
        lib.parakeet_capi_load.restype = ctypes.c_void_p
        lib.parakeet_capi_free.argtypes = [ctypes.c_void_p]
        lib.parakeet_capi_free.restype = None
        lib.parakeet_capi_transcribe_pcm.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_float),
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
        ]
        lib.parakeet_capi_transcribe_pcm.restype = ctypes.c_void_p
        lib.parakeet_capi_free_string.argtypes = [ctypes.c_void_p]
        lib.parakeet_capi_free_string.restype = None
        lib.parakeet_capi_last_error.argtypes = [ctypes.c_void_p]
        lib.parakeet_capi_last_error.restype = ctypes.c_char_p

        self._ctx = lib.parakeet_capi_load(str(model_path).encode())
        if not self._ctx:
            raise RuntimeError(f"parakeet_capi_load failed for {model_path}")
        _LOGGER.info("STT model loaded: %s", model_path)

        self._lock = threading.Lock()

    def transcribe(self, pcm_f32: np.ndarray) -> str:
        samples = np.ascontiguousarray(pcm_f32, dtype=np.float32)
        if samples.size == 0:
            return ""
        with self._lock:
            result = self._lib.parakeet_capi_transcribe_pcm(
                self._ctx,
                samples.ctypes.data_as(ctypes.POINTER(ctypes.c_float)),
                len(samples),
                _RATE,
                0,
            )
            if not result:
                err = self._lib.parakeet_capi_last_error(self._ctx)
                raise RuntimeError((err or b"").decode(errors="replace"))
            try:
                return ctypes.string_at(result).decode(errors="replace").strip()
            finally:
                self._lib.parakeet_capi_free_string(result)


# ---------------------------------------------------------------------------
# TTS: audio.cpp via audiocpp_server REST
# ---------------------------------------------------------------------------
class AudioCppTTS:
    def __init__(self, base_url: str, model_id: str, default_voice: str):
        self._base_url = base_url.rstrip("/")
        self._model_id = model_id
        self._default_voice = default_voice
        self._health_url = f"{self._base_url}/health"
        self._speech_url = f"{self._base_url}/v1/audio/speech"

    def health(self) -> bool:
        try:
            with urllib.request.urlopen(self._health_url, timeout=5) as resp:
                return resp.status == 200
        except Exception:
            return False

    def synthesize(self, text: str, voice: str | None) -> bytes:
        body = json.dumps(
            {"model": self._model_id, "input": text, "voice": voice or self._default_voice}
        ).encode()
        req = urllib.request.Request(
            self._speech_url,
            data=body,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=120) as resp:
            return resp.read()


def _parse_wav(data: bytes) -> tuple[int, int, int, bytes]:
    if data[:4] != b"RIFF":
        raise ValueError("TTS response is not a RIFF/WAV file")
    with wave.open(io.BytesIO(data), "rb") as w:
        return w.getframerate(), w.getsampwidth(), w.getnchannels(), w.readframes(w.getnframes())


# ---------------------------------------------------------------------------
# Wyoming handler
# ---------------------------------------------------------------------------
class VoiceEventHandler(AsyncEventHandler):
    def __init__(
        self,
        stt: ParakeetSTT,
        tts: AudioCppTTS,
        info_event: Event,
        *args,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self._stt = stt
        self._tts = tts
        self._info_event = info_event
        self._converter = AudioChunkConverter(rate=_RATE, width=_WIDTH, channels=_CHANNELS)
        self._audio = bytearray()
        self._have_audio_start = False

    async def handle_event(self, event: Event) -> bool:
        if Describe.is_type(event.type):
            await self.write_event(self._info_event)
            return True

        # --- STT path ---
        if Transcribe.is_type(event.type):
            return True  # English-only model; language selection ignored

        if AudioStart.is_type(event.type):
            self._audio = bytearray()
            self._have_audio_start = True
            return True

        if AudioChunk.is_type(event.type):
            chunk = AudioChunk.from_event(event)
            chunk = self._converter.convert(chunk)
            self._audio.extend(chunk.audio)
            return True

        if AudioStop.is_type(event.type):
            try:
                if self._audio:
                    pcm = (
                        np.frombuffer(bytes(self._audio), dtype=np.int16).astype(np.float32)
                        / 32768.0
                    )
                    text = await asyncio.to_thread(self._stt.transcribe, pcm)
                    if text:
                        _LOGGER.info("STT: %s", text)
                        await self.write_event(Transcript(text=text).event())
            except Exception as err:
                _LOGGER.error("STT failed: %s", err)
                return False
            finally:
                self._audio = bytearray()
                self._have_audio_start = False
            return True

        # --- TTS path ---
        if Synthesize.is_type(event.type):
            synth = Synthesize.from_event(event)
            text = (synth.text or "").strip()
            if not text:
                _LOGGER.warning("empty synthesis text")
                return True
            voice_name = None
            if synth.voice is not None:
                voice_name = synth.voice.name or synth.voice.language
            try:
                wav = await asyncio.to_thread(self._tts.synthesize, text, voice_name)
                rate, width, channels, payload = await asyncio.to_thread(_parse_wav, wav)
                _LOGGER.info(
                    "TTS: %d chars -> %s Hz/%d-bit/%dch (voice=%s)",
                    len(text), rate, width * 8, channels, voice_name,
                )
                chunk_bytes = _CHUNK_SAMPLES * width * channels
                for i in range(0, len(payload), chunk_bytes):
                    await self.write_event(
                        AudioChunk(
                            rate=rate, width=width, channels=channels,
                            audio=payload[i : i + chunk_bytes],
                        ).event()
                    )
                await self.write_event(SynthesizeStop().event())
            except Exception as err:
                _LOGGER.error("TTS failed: %s", err)
                return False
            return True

        return True

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        self._audio = bytearray()
        self._have_audio_start = False


def make_info() -> Info:
    return Info(
        asr=[
            AsrProgram(
                name="parakeet",
                description="parakeet.cpp ASR (ggml, Vulkan)",
                attribution=Attribution(
                    name="parakeet.cpp", url="https://github.com/mudler/parakeet.cpp"
                ),
                installed=True,
                version="0.5.0",
                models=[
                    AsrModel(
                        name="parakeet-ctc-0.6b",
                        description="NVIDIA Parakeet CTC 0.6B (English)",
                        attribution=Attribution(
                            name="NVIDIA NeMo", url="https://github.com/NVIDIA-NeMo/NeMo"
                        ),
                        installed=True,
                        languages=["en"],
                        version="0.5.0",
                    )
                ],
            )
        ],
        tts=[
            TtsProgram(
                name="audiocpp",
                description="audio.cpp PocketTTS (ggml, Vulkan)",
                attribution=Attribution(
                    name="audio.cpp", url="https://github.com/0xShug0/audio.cpp"
                ),
                installed=True,
                version="0.6.0",
                supports_synthesize_streaming=False,
                voices=[
                    TtsVoice(
                        name="alba",
                        description="alba",
                        installed=True,
                        languages=["en"],
                        version=None,
                        attribution=Attribution(
                            name="Kyutai", url="https://github.com/Kyutai-Labs/pocket-tts"
                        ),
                    )
                ],
            )
        ],
    )


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--uri", default="tcp://0.0.0.0:10300")
    parser.add_argument("--stt-model", required=True, help="Path to parakeet .gguf")
    parser.add_argument("--stt-lib", required=True, help="Path to libparakeet.so")
    parser.add_argument(
        "--stt-device", default="Vulkan0", help="parakeet.cpp device name"
    )
    parser.add_argument("--audio-cpp-url", default="http://127.0.0.1:8100")
    parser.add_argument("--tts-model-id", default="pocket-tts")
    parser.add_argument("--tts-voice", default="alba")
    parser.add_argument("--debug", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    stt = ParakeetSTT(Path(args.stt_lib), Path(args.stt_model), args.stt_device)
    tts = AudioCppTTS(args.audio_cpp_url, args.tts_model_id, args.tts_voice)

    for _ in range(60):
        if tts.health():
            break
        _LOGGER.info("waiting for audiocpp_server...")
        await asyncio.sleep(1)
    else:
        raise RuntimeError("audiocpp_server never became healthy")

    info_event = make_info().event()
    server = AsyncServer.from_uri(args.uri)
    server_task = asyncio.create_task(
        server.run(
            lambda *a, **kw: VoiceEventHandler(stt, tts, info_event, *a, **kw)
        )
    )
    try:
        await server_task
    except asyncio.CancelledError:
        pass


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass