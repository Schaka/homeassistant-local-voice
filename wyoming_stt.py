#!/usr/bin/env python3
"""Wyoming STT server backed by parakeet.cpp (ggml, Vulkan).

Loads the model ONCE into a process-global context (resident in VRAM) and
transcribes each full utterance via the flat C API. Speaks the Wyoming protocol
so Home Assistant's Voice Assistant pipeline can register it as an STT engine.

Run:
    wyoming_stt.py --model /models/stt.gguf --lib /usr/lib/libparakeet.so \
        --uri tcp://0.0.0.0:10300 --device Vulkan0
"""
import argparse
import asyncio
import ctypes
import logging
import os
import threading
from pathlib import Path

import numpy as np

from wyoming.asr import Transcribe, Transcript
from wyoming.audio import AudioChunk, AudioChunkConverter, AudioStart, AudioStop
from wyoming.event import Event
from wyoming.info import AsrModel, AsrProgram, Attribution, Describe, Info
from wyoming.server import AsyncEventHandler, AsyncServer

_LOGGER = logging.getLogger(__name__)

_RATE = 16000
_WIDTH = 2
_CHANNELS = 1


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
        _LOGGER.info("parakeet model loaded: %s", model_path)

        self._lock = threading.Lock()

    def transcribe(self, pcm_f32: np.ndarray, language: str | None = None) -> str:
        """Transcribe mono float32 PCM (any sample rate; lib resamples to 16k)."""
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

    def close(self) -> None:
        if self._ctx:
            self._lib.parakeet_capi_free(self._ctx)
            self._ctx = None


class STTEventHandler(AsyncEventHandler):
    def __init__(self, stt: ParakeetSTT, info_event: Event, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._stt = stt
        self._info_event = info_event
        self._converter = AudioChunkConverter(
            rate=_RATE, width=_WIDTH, channels=_CHANNELS
        )
        self._audio = bytearray()
        self._language: str | None = None
        self._have_audio_start = False

    async def handle_event(self, event: Event) -> bool:
        if Describe.is_type(event.type):
            await self.write_event(self._info_event)
            return True

        if Transcribe.is_type(event.type):
            transcribe = Transcribe.from_event(event)
            if transcribe.language:
                self._language = transcribe.language
            return True

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
                if not self._have_audio_start:
                    self._audio = bytearray()
                if self._audio:
                    pcm = (
                        np.frombuffer(bytes(self._audio), dtype=np.int16).astype(
                            np.float32
                        )
                        / 32768.0
                    )
                    text = await asyncio.to_thread(
                        self._stt.transcribe, pcm, self._language
                    )
                    if text:
                        _LOGGER.info("transcript: %s", text)
                        await self.write_event(Transcript(text=text).event())
            except Exception as err:
                _LOGGER.error("transcription failed: %s", err)
                return False
            finally:
                self._audio = bytearray()
                self._have_audio_start = False
            return True

        return True

    async def __aexit__(self, exc_type, exc_val, exc_tb) -> None:
        self._audio = bytearray()
        self._have_audio_start = False


def make_info(model_name: str, model_description: str) -> Info:
    return Info(
        asr=[
            AsrProgram(
                name="parakeet",
                description="parakeet.cpp ASR (ggml, Vulkan)",
                attribution=Attribution(
                    name="parakeet.cpp",
                    url="https://github.com/mudler/parakeet.cpp",
                ),
                installed=True,
                version="0.5.0",
                models=[
                    AsrModel(
                        name=model_name,
                        description=model_description,
                        attribution=Attribution(
                            name="NVIDIA NeMo",
                            url="https://github.com/NVIDIA-NeMo/NeMo",
                        ),
                        installed=True,
                        languages=["en"],
                        version="0.5.0",
                    )
                ],
            )
        ],
    )


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, help="Path to .gguf model")
    parser.add_argument("--lib", required=True, help="Path to libparakeet.so")
    parser.add_argument("--uri", default="tcp://0.0.0.0:10300")
    parser.add_argument(
        "--device",
        default="Vulkan0",
        help="parakeet.cpp device name (e.g. Vulkan0, cpu)",
    )
    parser.add_argument("--model-name", default="parakeet-ctc-0.6b")
    parser.add_argument(
        "--model-description", default="NVIDIA Parakeet CTC 0.6B (English)"
    )
    parser.add_argument("--debug", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    stt = ParakeetSTT(Path(args.lib), Path(args.model), args.device)
    info_event = make_info(args.model_name, args.model_description).event()

    server = AsyncServer.from_uri(args.uri)
    server_task = asyncio.create_task(
        server.run(
            lambda *a, **kw: STTEventHandler(stt, info_event, *a, **kw)
        )
    )

    try:
        await server_task
    except asyncio.CancelledError:
        pass
    finally:
        stt.close()


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass