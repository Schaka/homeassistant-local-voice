# Set up Home Assistant

This page connects Home Assistant to `assistant-hub` and shows a working voice setup: speech, the local agent, tools, sentence triggers and automations. The files in [examples/home-assistant/](../examples/home-assistant/) are tested versions of everything on this page.

You need Home Assistant 2026.8 or later for the llama.cpp integration. For speech only, any version with the Wyoming Protocol integration works.

## 1. Start the container

Pick the models for your hardware in [hardware.md](hardware.md), then start the container as in the [README](../README.md#quick-start). Make sure that it answers:

```bash
curl http://<host>:10300/health
# {"status":"ok","stt":true,"tts":true,"llm":"ok","web_search":true,"youtube":true}
```

## 2. Speech: the Wyoming Protocol integration

1. Go to Settings > Devices & services > Add Integration > Wyoming Protocol.
2. Enter the host of the container and the port `10300`.

You get two services: `parakeet` (speech to text) and `audiocpp` (text to speech). One integration entry is enough, because both use the same port.

## 3. The local agent: the llama.cpp integration

If you use a cloud model, skip this step. [llm.md](llm.md) compares the options.

1. Go to Settings > Devices & services > Add Integration > llama.cpp.
2. Set the URL to `http://<host>:10300/llm/v1`. If you set `LLM_API_KEY`, enter it as the API key.
3. Add a conversation agent and select the model.
4. Open the options of the agent:
   1. Turn off "Recommended model settings".
   2. Set the temperature and top-p of your model ([llm.md](llm.md#set-the-sampling-in-home-assistant)). For Ling-3.0-tiny, 0.7 and 1.0 worked best.
   3. Under "Control Home Assistant", select "Assist". If you use the MCP server of step 4, add it too.
   4. Replace the prompt with [examples/home-assistant/prompt.txt](../examples/home-assistant/prompt.txt) and adapt it to your home.

The first request after each change of the prompt or the exposed entities is slow (30 s to 3 min, see [hardware.md](hardware.md)). Later requests use the prompt cache.

## 4. Web search for the agent (optional)

1. Set `SEARCH_ENABLED=on` on the container.
2. Go to Settings > Devices & services > Add Integration > Model Context Protocol.
3. Set the URL to `http://<host>:10300/mcp`.
4. Add the new server to "Control Home Assistant" of the agent, next to "Assist".

[search.md](search.md) explains the tool.

## 5. The voice assistant (pipeline)

1. Go to Settings > Voice assistants > Add assistant.
2. Select the conversation agent of step 3, `parakeet` for speech to text and `audiocpp` for text to speech.
3. Turn on "Prefer handling commands locally". Home Assistant then runs simple commands ("turn off the kitchen light") without the LLM, in less than a second.
4. Make it the preferred assistant.

## 6. Satellites

Any Home Assistant voice satellite works, for example the Home Assistant Voice Preview Edition or an M5Stack Atom Echo with ESPHome.

1. Wake word: use the wake word on the device (microWakeWord), or the openWakeWord add-on for devices without one.
2. Set "Finished speaking detection" of each satellite to "default". With "aggressive", requests were cut after 1 s.
3. Give each satellite an area. A request without a room then acts in the room of the satellite.

## 7. Expose the right entities

The LLM sees only exposed entities. Each one adds about 40 tokens to the prompt, so expose only what you control by voice.

1. Create floors and areas, and give every exposed device an area. "Turn off the lights downstairs" works only with floors.
2. Expose single devices, not groups that contain them. Otherwise the model sees each light twice.
3. Give devices names that people say: "Kitchen ceiling light", not "0xa4c1...". Add aliases for other names.
4. If only automations need an entity, for example a helper toggle, do not expose it.

Settings > Voice assistants > Expose shows the list. An agent can set it through the API, see [AGENTS.md](../AGENTS.md).

## 8. Scripts as tools

Every exposed script is a tool for the LLM. Small models make mistakes with raw data, so a good tool script prepares the answer:

1. The description says when to use the tool. The model reads it with every request.
2. The fields have a description and an example.
3. The script returns short English data or a finished sentence with `stop` and `response_variable`.

| Example | What it does |
|---|---|
| [scripts/lights_on.yaml](../examples/home-assistant/scripts/lights_on.yaml) | Lists the lights that are on, grouped by floor and room. Small models count badly from the raw state list. |
| [scripts/weather_forecast.yaml](../examples/home-assistant/scripts/weather_forecast.yaml) | High and low, then morning, afternoon and evening values for today, tomorrow or the week |
| [scripts/play_youtube.yaml](../examples/home-assistant/scripts/play_youtube.yaml) | Plays a YouTube video on an Android TV box. Uses `/youtube/search`. |
| [scripts/play_jellyfin.yaml](../examples/home-assistant/scripts/play_jellyfin.yaml) | Plays the next episode of a series, or a movie, from Jellyfin. Uses `/jellyfin/search`. |

Write tool descriptions in plain sentences. Add one line to the prompt for each tool that the model must not miss, for example "For future weather, call weather_forecast."

## 9. Sentence triggers

An automation with a conversation trigger answers a fixed sentence without the LLM. It is the fastest path, also on old hardware. Use it for frequent requests.

```yaml
triggers:
  - trigger: conversation
    command:
      - good night
      - (I'm | I am | we're | we are) going to bed
```

[automations/voice_good_night.yaml](../examples/home-assistant/automations/voice_good_night.yaml) turns off the lights and the TV and says the weather for tomorrow.

A trigger can also hand the sentence to the LLM. [automations/voice_play_video.yaml](../examples/home-assistant/automations/voice_play_video.yaml) catches "play ... on the TV" and gives the whole sentence to the agent, which then calls `play_youtube`. This is simpler and more reliable than parsing the sentence with templates.

## 10. YouTube on an Android TV box

`/youtube/search?q=` returns one video to play ([search.md](search.md#youtube-search)). The script [play_youtube.yaml](../examples/home-assistant/scripts/play_youtube.yaml) calls it through a REST command and opens the video on the box with ADB (Android Debug Bridge, a remote shell for Android).

1. On the box, turn on network debugging (ADB) in the developer options.
2. Add the integration "Android Debug Bridge" in Home Assistant with the address of the box.
3. Copy [rest_commands.yaml](../examples/home-assistant/rest_commands.yaml) into your configuration and add `rest_command: !include rest_commands.yaml` to `configuration.yaml`. Restart Home Assistant.
4. Copy the script, set the entity of your box and expose the script to Assist.

The script tries TizenTube, SmartTube and the YouTube app, in this order. Boxes with an Amlogic chip (for example the Ugoos AM6B+) can also turn the TV on and off over HDMI-CEC from the box. The script has these commands as an optional step. [scripts/tv_off.yaml](../examples/home-assistant/scripts/tv_off.yaml) turns the TV off in an order that keeps it off.

[android-tv-control.md](android-tv-control.md) explains how to control a box without root: the approval of the key, wake and standby, starting a named app, and one script for several rooms.

## 11. Ideas for automations

| Idea | How | Example |
|---|---|---|
| Night light in amber | Motion turns lights on at 25 % in warm amber at night, normal white by day. A template sensor ends the night at sunrise, between 06:00 and 08:00. | [motion_night_light.yaml](../examples/home-assistant/automations/motion_night_light.yaml), [night_light.yaml](../examples/home-assistant/templates/night_light.yaml) |
| Good night | One sentence turns off lights, music and the TV, then says the weather for tomorrow | [voice_good_night.yaml](../examples/home-assistant/automations/voice_good_night.yaml) |
| Morning briefing | "Good morning" says the weather, what to wear and the bins for today | Sentence trigger plus a script that builds the text |
| Everyone left | When the last phone leaves home, turn off lights, music and the TV | Trigger on the `zone.home` count reaching 0 |
| Waste collection | A push the evening before a pickup, and a tool for "when are the bins collected" | Calendar integration of your waste company |
| Dehumidifier | A smart plug turns a dehumidifier on above 60 % humidity and off below 55 %, only in the daytime | Numeric state triggers with a time condition |
| Weekly device report | A push on Sunday with low batteries and devices that are unavailable for more than a day | Template over `integration_entities()` |
| TV mode | When the TV turns on, pause the music and dim the room | State trigger on the TV or the receiver |

## 12. Test and troubleshoot

1. Settings > Voice assistants > (your assistant) > Debug shows each request: the transcript, the tool calls and the answer.
2. `docker exec <name> assistant-hub probe tts "Hello." /tmp/a.wav` and `... probe stt /tmp/a.wav` test speech without Home Assistant.
3. The container log shows every transcript (`STT: ...`), every TTS request, the searches and the prompt cache.

| Problem | Cause and fix |
|---|---|
| The first answer takes minutes | The full prompt is prefilled once after each prompt change. Later answers use the cache. Make the prompt shorter: fewer exposed entities and tools. |
| The answer is empty | Keep `LLM_TEMPLATE_FIX=on` (the default). With a model from `LLM_HF_REPO`, the fix starts with the second start of the container, after the download. |
| The satellite stays in "listening" | Set "Finished speaking detection" to "default". Check the container log for an `STT` line. |
| The model reads markdown aloud | The server removes most markdown before TTS. Add "no markdown, no lists" to the prompt. |
| Wrong words in the transcript | Use a TDT model ([voice.md](voice.md#measured-stt-accuracy)). Set `STT_DEBUG_DIR` to keep the recordings and compare models. |
| `/health` shows `"status": "degraded"` | STT did not load, TTS does not answer, or llama-server does not answer (then `"llm": "unavailable"`). The container log names the cause. The hub probes the child processes every 30 s and restarts a hung one. `"llm": "busy"` is no fault: llama-server works on a long prompt and answers the probe late. |
