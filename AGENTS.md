# Instructions for AI agents

This file is for AI coding and operations agents. People start with [README.md](README.md).

The repository builds one container image: `assistant-hub`, a Rust server on one port (10300) for a local Home Assistant voice setup. It serves Wyoming STT and TTS, an optional local LLM under `/llm/v1`, MCP tools on `/mcp`, and web and YouTube search.

You get one of two tasks. Read the matching section.

1. Change the code of the image: section "Work on the code".
2. Set up a voice assistant for a user: section "Set up a home".

## Work on the code

### Read first

1. [docs/architecture.md](docs/architecture.md): processes, routes, source files.
2. [docs/configuration.md](docs/configuration.md): every environment variable.
3. The module comment at the top of the file that you change.

### Commands

The crate links Python through PyO3 (for the embedded SearXNG). Install the Python development headers. If the default Python is not 3.13, set `PYO3_PYTHON` to an installed Python 3. `build.rs` needs libclang and the llama.cpp headers.

```bash
./scripts/fetch-llama-headers.sh   # once, llama.cpp headers to target/
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
docker buildx bake final          # the image, the same as CI
```

CI runs the first three, a release build and `cargo deny check all`. All must pass.

### Rules

1. Keep one port and one server. Do not add a web server, a sidecar process or a second container. A new HTTP API is a route in `src/server.rs`. A new LLM tool is an entry in `src/mcp.rs`.
2. Embed upstream code instead of a rewrite. SearXNG runs as a Python library, llama-server and audiocpp_server run as child processes.
3. Never start a second llama-server. One model needs 2 to 9 GB.
4. Keep tool descriptions and tool results short. Each token costs prefill time on a CPU. A search result has 150 to 300 tokens.
5. Pin every version: crates in `Cargo.lock`, the toolchain in `rust-toolchain.toml` and `docker-bake.hcl`, actions by commit SHA, native engines by release or commit in `docker-bake.hcl`.
6. `build.rs` generates the llama.cpp bindings at build time from the headers of `LLAMACPP_BUILD` (docker-bake.hcl), the same build as the library in the image. Nothing from llama.cpp is in the repository. Outside the image, run `scripts/fetch-llama-headers.sh` once. After a bump of `LLAMACPP_BUILD`, a changed struct fails the build through the layout checks of bindgen.
7. A new environment variable needs a default, a line in `src/config.rs` and a row in `docs/configuration.md`.
8. Write documentation in plain English: short sentences, active voice, no semicolons, no em-dashes.
9. The target hardware includes 10-year-old office PCs ([docs/hardware.md](docs/hardware.md)). Do not add work to the request path that a 4th-generation Core i5 cannot do fast.

### Skills

`.claude/skills/` has step-by-step procedures. Claude Code loads them by itself. Other agents read them as documents.

| Skill | Use it to |
|---|---|
| `add-hub-feature` | Add a route, an MCP tool, a search, a background service or a native engine |
| `bump-native-engine` | Update llama.cpp, parakeet.cpp, audio.cpp, SearXNG, Rust or cargo-chef |

### Lessons that cost time before

1. Measure on real data before you trust a design. The media index first ranked episodes and series together, and a "Planet Earth" episode beat "Dune" for "desert planet with sand worms". Only the evaluation on a real library showed it.
2. A threshold needs negatives in its test set: queries that must find nothing. Without them, every gate looks good.
3. Native libraries with their own ggml must load with `RTLD_LOCAL`, and only one backend each. `assistant-hub` asks `llama-server --list-devices` for GPUs, because Mesa's software driver is no device for ggml.
4. Background work that uses the CPU runs on its own thread at nice 10, never on the tokio blocking pool.
5. Local builds link the Python that PyO3 finds. If that is not the system Python, set `LD_LIBRARY_PATH` to its `lib` folder to run `target/debug/assistant-hub`.

## Set up a home

The goal: speech, the agent and useful tools work in the user's Home Assistant. [docs/home-assistant.md](docs/home-assistant.md) is the same procedure for people.

### Rules for this task

1. Ask the user before you switch devices, play audio, restart Home Assistant or delete anything.
2. Ask for a long-lived access token of Home Assistant (Profile > Security). Do not write it into a file in a repository or into a log.
3. Change one thing at a time and test it before the next step.
4. Before you change an entity, an automation or a script, read its current state.

### Step 1: inspect the host

Run these commands on the Docker host:

```bash
grep -o -w -E 'avx2|fma' /proc/cpuinfo | sort -u   # must print avx2 and fma
nproc; free -g                                      # cores and RAM
ls /dev/dri/                                        # renderD128 means a GPU
docker --version
```

Without `avx2` and `fma`, the image does not run. Tell the user and stop.

### Step 2: choose the models

Use the table "Recommendations" in [docs/hardware.md](docs/hardware.md). In short:

| RAM | GPU | Settings |
|---|---|---|
| 8 GB | none | `STT_MODEL_FILENAME=parakeet-tdt-0.6b-v3-q4_k.gguf` (mount it), `LLM_HF_REPO=bartowski/Ling-3.0-tiny-GGUF:Q3_K_M`, `LLM_CTX=32k`, or a cloud agent |
| 16 GB or more | none or small | Default speech models, `LLM_HF_REPO=inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M`, `LLM_CTX=32k` |
| 16 GB or more | 8 GB or more | As above, plus `LLM_DEVICE=Vulkan0` and `LLM_EXTRA_ARGS=-fa off` |

Without a GPU, the server runs STT and TTS on the CPU by itself.

### Step 3: start the container

```bash
docker run -d --name assistant-hub --restart unless-stopped \
  -p 10300:10300 \
  -v /srv/assistant-hub/llm:/models/llm \
  -e LLM_HF_REPO=inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M -e LLM_CTX=32k \
  -e SEARCH_ENABLED=on \
  ghcr.io/schaka/homeassistant-local-voice:latest
# With a GPU, add:  --device /dev/dri/renderD128 --group-add "$(stat -c %g /dev/dri/renderD128)"
```

The first start downloads the LLM (2 to 5 GB). Then check it:

```bash
curl -s http://<host>:10300/health      # "status":"ok", "stt":true, "tts":true
docker exec assistant-hub assistant-hub probe tts "Hello." /tmp/a.wav
docker exec assistant-hub assistant-hub probe stt /tmp/a.wav   # prints "hello"
docker exec assistant-hub assistant-hub probe llm "Turn on the kitchen light."
```

### Step 4: connect Home Assistant through its API

`$HA` is the Home Assistant URL, `$T` the token. REST calls use `Authorization: Bearer $T`. WebSocket calls go to `$HA/api/websocket`: send `{"type":"auth","access_token":"<token>"}` first, then messages with a rising `id`.

Config flows. Start a flow, read the fields in `data_schema` of the answer, then send them:

```bash
curl -s -H "Authorization: Bearer $T" -X POST $HA/api/config/config_entries/flow \
  -d '{"handler":"wyoming"}'
curl -s -H "Authorization: Bearer $T" -X POST $HA/api/config/config_entries/flow/<flow_id> \
  -d '{"host":"<host>","port":10300}'
```

| Integration | `handler` | Data |
|---|---|---|
| Wyoming Protocol (STT and TTS) | `wyoming` | `host`, `port` 10300 |
| llama.cpp (the agent) | `llama_cpp` | The URL `http://<host>:10300/llm/v1`. Read the field names from `data_schema`. |
| Model Context Protocol (web search) | `mcp` | The URL `http://<host>:10300/mcp` |

The llama.cpp entry holds the agents as subentries. Add one with `POST $HA/api/config/config_entries/subentries/flow` and `{"handler":["<entry_id>","conversation"]}`. To change the prompt or the options later, add `"subentry_id":"<id>","source":"reconfigure"`. Set these options:

1. Recommended settings off, temperature 0.7, top-p 1.0 (Ling-3.0-tiny).
2. LLM APIs: `assist`, plus the MCP entry if you added it.
3. The prompt: [examples/home-assistant/prompt.txt](examples/home-assistant/prompt.txt), adapted to the home.

WebSocket messages for the rest:

```json
{"type": "assist_pipeline/pipeline/create", "name": "Local voice", "language": "en",
 "conversation_engine": "conversation.<agent>", "conversation_language": "*",
 "stt_engine": "stt.parakeet", "stt_language": "en",
 "tts_engine": "tts.audiocpp", "tts_language": "en", "tts_voice": "alba",
 "wake_word_entity": null, "wake_word_id": null, "prefer_local_intents": true}
{"type": "assist_pipeline/pipeline/set_preferred", "pipeline_id": "<id>"}
{"type": "homeassistant/expose_entity/list"}
{"type": "homeassistant/expose_entity", "assistants": ["conversation"],
 "entity_ids": ["light.kitchen"], "should_expose": true}
{"type": "config/floor_registry/create", "name": "Downstairs", "level": 0}
{"type": "config/area_registry/create", "name": "Kitchen", "floor_id": "downstairs"}
{"type": "config/device_registry/update", "device_id": "<id>", "area_id": "kitchen"}
{"type": "validate_config", "triggers": [], "actions": []}
```

Scripts and automations: validate the config with `validate_config` first, then save it. Both reload by themselves.

```bash
curl -s -H "Authorization: Bearer $T" -X POST $HA/api/config/script/config/lights_on -d @lights_on.json
curl -s -H "Authorization: Bearer $T" -X POST $HA/api/config/automation/config/voice_good_night -d @voice_good_night.json
```

The body is the YAML of the example as JSON. Expose each tool script with `homeassistant/expose_entity`.

### Step 5: test

```bash
curl -s -H "Authorization: Bearer $T" -X POST $HA/api/conversation/process \
  -d '{"text":"Which lights are on?","agent_id":"conversation.<agent>","language":"en"}'
```

The first request prefills the full prompt and can take minutes on old hardware. Run it again: the second answer shows the real speed. Settings > Voice assistants > Debug shows the tool calls of each request.

### Step 6: offer automations

Look at the devices of the home and suggest automations from the table "Ideas for automations" in [docs/home-assistant.md](docs/home-assistant.md#11-ideas-for-automations). The [examples](examples/) are tested starting points. Good first ones:

1. A "good night" sentence trigger, if the home has lights and a TV.
2. A motion light with amber at night, if the home has motion sensors.
3. `lights_on` and `weather_forecast` as tools, for every home.
4. `play_youtube`, if the home has an Android TV box with network ADB.

Ask the user before you add an automation that switches devices on its own.
