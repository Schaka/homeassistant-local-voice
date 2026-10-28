"""Smoke-test the conversation agent (llama-server, OpenAI-compatible /v1).

Sends one chat completion with a Home Assistant style HassTurnOn tool, the way
the llama.cpp integration does, and prints the reply / tool calls + timing.

    python3 scripts/test/llm_client.py "Turn on the kitchen light."
    python3 scripts/test/llm_client.py "What can you do?" --key anything
"""
import argparse, json, sys, time, urllib.request

TOOLS = [{
    "type": "function",
    "function": {
        "name": "HassTurnOn",
        "description": "Turns on/opens a device or entity",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "area": {"type": "string"},
                "domain": {"type": "array", "items": {"type": "string"}},
            },
        },
    },
}]

SYSTEM = (
    "You are a voice assistant for a smart home. Answer in one short sentence. "
    "Devices: light.kitchen 'Kitchen Light' (area: Kitchen) = off; "
    "light.bedroom 'Bedroom Light' (area: Bedroom) = on."
)

def filler_devices(n):
    """n extra entity lines, so the prompt is as long as a real Assist prompt."""
    lines = [f"sensor.room_{i}_temperature 'Room {i} Temperature' (area: Room {i}) = {18 + i % 7}.5 °C;"
             f"unit_of_measurement=°C;device_class=temperature" for i in range(n)]
    return "\n" + "\n".join(lines) if lines else ""

def main():
    p = argparse.ArgumentParser()
    p.add_argument("prompt")
    p.add_argument("--url", default="http://127.0.0.1:8080/v1")
    p.add_argument("--key", default="", help="sent as Bearer token; any value works unless LLM_API_KEY is set")
    p.add_argument("--devices", type=int, default=0, help="extra fake entities (~40 tokens each) to grow the prompt")
    args = p.parse_args()

    headers = {"Content-Type": "application/json"}
    if args.key:
        headers["Authorization"] = f"Bearer {args.key}"

    with urllib.request.urlopen(urllib.request.Request(f"{args.url}/models", headers=headers), timeout=10) as resp:
        model = json.load(resp)["data"][0]["id"]
    print("model:", model)

    body = {
        "model": model,
        "messages": [{"role": "system", "content": SYSTEM + filler_devices(args.devices)},
                     {"role": "user", "content": args.prompt}],
        "tools": TOOLS,
    }
    req = urllib.request.Request(f"{args.url}/chat/completions", data=json.dumps(body).encode(), headers=headers, method="POST")
    t0 = time.monotonic()
    with urllib.request.urlopen(req, timeout=600) as resp:
        out = json.load(resp)
    elapsed = time.monotonic() - t0

    msg = out["choices"][0]["message"]
    if msg.get("reasoning_content"):
        print(f"reasoning: {len(msg['reasoning_content'])} chars (not spoken)")
    print("content:", msg.get("content"))
    for call in msg.get("tool_calls") or []:
        print("tool_call:", call["function"]["name"], call["function"]["arguments"])
    timings = out.get("timings", {})
    print(f"wall {elapsed:.2f}s, prompt {timings.get('prompt_per_second', 0):.1f} tok/s, "
          f"gen {timings.get('predicted_per_second', 0):.1f} tok/s, usage {out.get('usage')}")

if __name__ == "__main__":
    sys.exit(main())
