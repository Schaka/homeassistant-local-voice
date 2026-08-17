import asyncio, sys, wave
from wyoming.client import AsyncTcpClient
from wyoming.tts import Synthesize
from wyoming.audio import AudioChunk
from wyoming.event import Event

async def main():
    async with AsyncTcpClient("127.0.0.1", 10301) as client:
        await client.write_event(Event(type="describe"))
        ev = await client.read_event()
        print("describe ->", ev.type)
        await client.write_event(Synthesize(text=sys.argv[1], voice=None).event())
        chunks = []; rate=width=channels=0
        while True:
            ev = await client.read_event()
            if ev is None: break
            print("event:", ev.type)
            if AudioChunk.is_type(ev.type):
                c = AudioChunk.from_event(ev); rate,width,channels=c.rate,c.width,c.channels; chunks.append(c.audio)
            if ev.type == "synthesize-stop": break
        if chunks:
            w = wave.open(sys.argv[2],"wb"); w.setframerate(rate); w.setsampwidth(width); w.setnchannels(channels); w.writeframes(b"".join(chunks)); w.close()
            print("saved", sys.argv[2], rate, width*8, channels, len(b"".join(chunks)), "bytes")
asyncio.run(main())
