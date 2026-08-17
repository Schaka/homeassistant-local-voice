#!/usr/bin/env python3
"""Wyoming TTS server backed by audio.cpp (ggml, Vulkan) via its OpenAI-style
REST endpoint.

The audio.cpp server (audiocpp_server) keeps the model and its session resident
after the first request, so both models (STT + TTS) stay in VRAM. This bridge
translates the Wyoming protocol to that REST call and streams the WAV back as
audio-chunk events.

Run:
    wyoming_tts.py --uri tcp://0.0.0.0:10301 --audio-cpp-url http://127.0.0.1:8100 \
        --model-id pocket-tts --voice alba
"""
import argparse
import asyncio
import io
import json
import logging
import urllib.request
import wave
from pathlib import Path

from wyoming.event import Event
from wyoming.info import (
    Attribution,
    Describe,
    Info,
    TtsProgram,
    TtsVoice,
)
from wyoming.server import AsyncEventHandler, AsyncServer
from wyoming.audio import AudioChunk
from wyoming.tts import Synthesize, SynthesizeStop

_LOGGER = logging.getLogger(__name__)

_CHUNK_SAMPLES = 4096


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
            {
                "model": self._model_id,
                "input": text,
                "voice": voice or self._default_voice,
            }
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
    """Return (rate, width, channels, pcm_payload) for a RIFF/WAVE stream."""
    if data[:4] != b"RIFF":
        # Not RIFF; try parsing as raw headerless audio if 44-byte RIFF absent.
        # audio.cpp returns RIFF by default, so treat any miss as an error.
        raise ValueError("TTS response is not a RIFF/WAV file")
    with wave.open(io.BytesIO(data), "rb") as w:
        rate = w.getframerate()
        width = w.getsampwidth()
        channels = w.getnchannels()
        payload = w.readframes(w.getnframes())
    return rate, width, channels, payload


class TTSEventHandler(AsyncEventHandler):
    def __init__(self, tts: AudioCppTTS, info_event: Event, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self._tts = tts
        self._info_event = info_event

    async def handle_event(self, event: Event) -> bool:
        if Describe.is_type(event.type):
            await self.write_event(self._info_event)
            return True

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
                wav = await asyncio.to_thread(
                    self._tts.synthesize, text, voice_name
                )
                rate, width, channels, payload = await asyncio.to_thread(
                    _parse_wav, wav
                )
                _LOGGER.info(
                    "synthesized %d chars -> %s Hz/%d-bit/%dch (voice=%s)",
                    len(text),
                    rate,
                    width * 8,
                    channels,
                    voice_name,
                )
                chunk_bytes = _CHUNK_SAMPLES * width * channels
                for i in range(0, len(payload), chunk_bytes):
                    chunk = payload[i : i + chunk_bytes]
                    await self.write_event(
                        AudioChunk(
                            rate=rate, width=width, channels=channels, audio=chunk
                        ).event()
                    )
                await self.write_event(SynthesizeStop().event())
            except Exception as err:
                _LOGGER.error("synthesis failed: %s", err)
                return False
            return True

        return True


def make_info(default_voice: str, voice_names: list[str]) -> Info:
    voices = [
        TtsVoice(
            name=name,
            description=name,
            installed=True,
            languages=["en"],
            version=None,
            attribution=Attribution(
                name="Kyutai", url="https://github.com/Kyutai-Labs/pocket-tts"
            ),
        )
        for name in voice_names
    ]
    return Info(
        tts=[
            TtsProgram(
                name="audiocpp",
                description="audio.cpp PocketTTS (ggml, Vulkan)",
                attribution=Attribution(
                    name="audio.cpp", url="https://github.com/0xShug0/audio.cpp"
                ),
                installed=True,
                voices=voices,
                version="0.6.0",
                supports_synthesize_streaming=False,
            )
        ],
    )


async def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--uri", default="tcp://0.0.0.0:10301")
    parser.add_argument(
        "--audio-cpp-url", default="http://127.0.0.1:8100", help="audiocpp_server URL"
    )
    parser.add_argument("--model-id", default="pocket-tts")
    parser.add_argument("--voice", default="alba", help="default voice")
    parser.add_argument("--voices", default="alba", help="comma-separated voice names")
    parser.add_argument("--debug", action="store_true")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )

    tts = AudioCppTTS(args.audio_cpp_url, args.model_id, args.voice)
    for _ in range(60):
        if tts.health():
            break
        _LOGGER.info("waiting for audiocpp_server...")
        await asyncio.sleep(1)
    else:
        raise RuntimeError("audiocpp_server never became healthy")

    voice_names = [v.strip() for v in args.voices.split(",") if v.strip()]
    info_event = make_info(args.voice, voice_names).event()

    server = AsyncServer.from_uri(args.uri)
    server_task = asyncio.create_task(
        server.run(lambda *a, **kw: TTSEventHandler(tts, info_event, *a, **kw))
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