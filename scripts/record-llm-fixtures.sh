#!/usr/bin/env bash
# Starts one real llama-server (mainline or ik_llama.cpp) on a model and records
# the HTTP answers that assistant-hub depends on into tests/fixtures/llm/<engine>/.
# The unit tests replay these files, so a new engine version that changes a
# shape shows up as a fixture diff. Run it after each bump of an engine.
#
#   scripts/record-llm-fixtures.sh llamacpp /opt/llama.cpp/llama-server /models/llm/Ling-3.0-tiny-Q4_K_M.gguf
#   scripts/record-llm-fixtures.sh ik       /path/to/ik/llama-server    /models/llm/Ling-3.0-tiny-Q4_K_M.gguf
#
# Needs curl and python3. The model decides the speed, any model with a
# thinking template and tool calls works (Ling-3.0-tiny is the reference).
set -euo pipefail
engine=${1:?engine: llamacpp or ik}
bin=${2:?path to llama-server}
model=${3:?path to a .gguf file}
port=${PORT:-18082}
out="$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures/llm/$engine"
work=$(mktemp -d "${TMPDIR:-/var/tmp}/llm-fixtures.XXXXXX")
mkdir -p "$out"
# Never `kill 0`: that signals the whole process group.
trap 'if [ -n "${pid:-}" ]; then kill "$pid" 2>/dev/null || true; fi; rm -rf "$work"' EXIT

common=(-m "$model" --host 127.0.0.1 --port "$port" -c 8192 --jinja --reasoning on --reasoning-budget 256
    -n 512 --slot-save-path "$work" --temp 0.1)
case $engine in
    llamacpp) extra=(--device none -ngl 0 --no-ui) ;;
    ik) extra=() ;;
    *) echo "unknown engine $engine" >&2; exit 1 ;;
esac

# Option names of the command line: the surface that the hub's arguments use.
{ "$bin" --help 2>&1 || true; } | python3 -c '
import re,sys
names=set()
for line in sys.stdin:
    s=line.strip()
    if not s.startswith("-"): continue
    # The option names come first, the description follows after two spaces.
    left=re.split(r"\s{2,}", re.sub(r",\s+", ", ", s), 1)[0]
    names.update(re.findall(r"(?<![\w-])--?[A-Za-z][\w-]*", left))
print("\n".join(sorted(names)))' > "$out/help_flags.txt"

"$bin" "${common[@]}" "${extra[@]}" > "$work/server.log" 2>&1 &
pid=$!
base=http://127.0.0.1:$port

# The first answers while the model loads (503 on both engines), then ready.
loading=
for _ in $(seq 1 300); do
    code=$(curl -s -o "$work/h.json" -w '%{http_code}' "$base/health" || true)
    if [ "$code" = 200 ]; then break; fi
    if [ "$code" != 000 ] && [ -z "$loading" ]; then loading="$code $(cat "$work/h.json")"; fi
    kill -0 "$pid" 2>/dev/null || { tail -20 "$work/server.log" >&2; exit 1; }
    sleep 0.1
done
[ "$code" = 200 ] || { echo "server did not become ready" >&2; exit 1; }
printf '%s\n' "${loading:-none}" > "$out/health_loading.txt"

req='{"messages":[{"role":"system","content":"You are a voice assistant for a smart home. Answer in one short sentence. Devices: light.kitchen (Kitchen Light) = off; light.bedroom (Bedroom Light) = on."},{"role":"user","content":"Turn on the kitchen light."}],"tools":[{"type":"function","function":{"name":"HassTurnOn","description":"Turns on a device","parameters":{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}}}]'
curl -s "$base/health" > "$out/health_ok.json"
curl -s "$base/v1/models" > "$out/models.json"
curl -s "$base/v1/chat/completions" -H 'Content-Type: application/json' -d "$req,\"temperature\":0.1}" > "$out/chat_tool_call.json"
curl -sN "$base/v1/chat/completions" -H 'Content-Type: application/json' \
    -d "$req,\"temperature\":0.1,\"stream\":true}" > "$out/chat_stream.sse"
curl -s "$base/slots" > "$out/slots_idle.json"

# The slot that served the tool request last (the one with the highest task id).
slot=$(python3 -c 'import json,sys; s=json.load(open(sys.argv[1])); print(max(s,key=lambda x:x.get("id_task",-1))["id"])' "$out/slots_idle.json")
curl -s -X POST "$base/slots/$slot?action=save" -H 'Content-Type: application/json' -d '{"filename":"fixture.bin"}' > "$out/slot_save.json"
curl -s -X POST "$base/slots/$slot?action=restore" -H 'Content-Type: application/json' -d '{"filename":"fixture.bin"}' > "$out/slot_restore.json"

# A slot while a request runs: the request asks for a long reply.
long='{"messages":[{"role":"user","content":"Count from 1 to 400 in words."}],"max_tokens":400,"temperature":0.1}'
curl -s --max-time 120 "$base/v1/chat/completions" -H 'Content-Type: application/json' -d "$long" > /dev/null &
cpid=$!
for _ in $(seq 1 50); do
    sleep 0.2
    curl -s "$base/slots" > "$work/s.json"
    if python3 -c 'import json,sys; sys.exit(0 if any(s.get("is_processing") or s.get("state",0)!=0 for s in json.load(open(sys.argv[1]))) else 1)' "$work/s.json"; then
        cp "$work/s.json" "$out/slots_busy.json"
        break
    fi
done
wait "$cpid" || true

# ik never answers a restore of a missing file (the status is 000 after the timeout).
curl -s --max-time 5 -o /dev/null -w '%{http_code}\n' -X POST "$base/slots/$slot?action=restore" -H 'Content-Type: application/json' \
    -d '{"filename":"missing.bin"}' > "$out/slot_restore_missing_status.txt" || true

# The long fields only slow the tests down. Keep the keys and shortened text.
python3 - "$out" <<'PY'
import json,sys,pathlib,re
d=pathlib.Path(sys.argv[1])
def shorten(v):
    if isinstance(v,str):
        # No paths of the recording machine in the fixtures.
        if v.endswith(".gguf"): v=v.rsplit("/",1)[-1]
        return v[:160]+"..." if len(v)>160 else v
    if isinstance(v,list): return [shorten(x) for x in v[:40]]
    if isinstance(v,dict): return {k:shorten(x) for k,x in v.items()}
    return v
for f in d.glob("*.json"):
    text=f.read_text()
    # No paths of the recording machine in the fixtures.
    text=re.sub(r'"[^"]*/([^/"]*\.gguf)"',r'"\1"',text)
    try: data=json.loads(text)
    except Exception:
        f.write_text(text)
        continue
    f.write_text(json.dumps(shorten(data),indent=1,sort_keys=True)+"\n")
sse=d/"chat_stream.sse"
lines=[re.sub(r'"model":"[^"]*/([^/"]*\.gguf)"',r'"model":"\1"',l) for l in sse.read_text().splitlines()]
sse.write_text("\n".join(lines[:12]+["..."]+lines[-6:])+"\n")
PY
echo "recorded $out"
