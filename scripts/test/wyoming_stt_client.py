import asyncio, sys, wave
from wyoming.client import AsyncTcpClient
from wyoming.audio import AudioStart, AudioChunk, AudioStop
from wyoming.asr import Transcript
from wyoming.event import Event

async def main():
    async with AsyncTcpClient("127.0.0.1", 10300) as client:
        await client.write_event(Event(type="describe"))
        ev = await client.read_event()
        print("describe ->", ev.type)
        w = wave.open(sys.argv[1], "rb")
        data = w.readframes(w.getnframes())
        await client.write_event(AudioStart(rate=16000, width=2, channels=1).event())
        for i in range(0, len(data), 32000):
            await client.write_event(AudioChunk(rate=16000, width=2, channels=1, audio=data[i:i+32000]).event())
        await client.write_event(AudioStop().event())
        while True:
            ev = await client.read_event()
            if ev is None:
                break
            print("event:", ev.type)
            if Transcript.is_type(ev.type):
                print("TRANSCRIPT:", Transcript.from_event(ev).text)
                break
asyncio.run(main())
