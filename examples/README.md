# Examples

Working Home Assistant configuration for a voice setup with `assistant-hub`. The files come from a real home and were checked with Home Assistant 2026.10. Replace the entity IDs, the host and the agent name with your own.

[docs/home-assistant.md](../docs/home-assistant.md) explains each step. [AGENTS.md](../AGENTS.md) tells an AI agent how to install them through the Home Assistant API.

| File | Type | What it does |
|---|---|---|
| [home-assistant/prompt.txt](home-assistant/prompt.txt) | Agent prompt | A tested prompt for a small local model. Paste it into the options of the conversation agent. |
| [home-assistant/rest_commands.yaml](home-assistant/rest_commands.yaml) | REST commands | `/youtube/search` and `/web/search` for scripts |
| [home-assistant/scripts/lights_on.yaml](home-assistant/scripts/lights_on.yaml) | Tool | Which lights are on, by floor and room |
| [home-assistant/scripts/weather_forecast.yaml](home-assistant/scripts/weather_forecast.yaml) | Tool | Forecast for today, tomorrow or the week, in parts of the day |
| [home-assistant/scripts/play_youtube.yaml](home-assistant/scripts/play_youtube.yaml) | Tool | Plays a YouTube video on an Android TV box over ADB and turns the TV on |
| [home-assistant/scripts/tv_off.yaml](home-assistant/scripts/tv_off.yaml) | Script | Turns the TV off so that HDMI-CEC does not wake it again |
| [home-assistant/automations/voice_good_night.yaml](home-assistant/automations/voice_good_night.yaml) | Sentence trigger | "Good night": lights, music and TV off, weather for tomorrow, no LLM |
| [home-assistant/automations/voice_play_video.yaml](home-assistant/automations/voice_play_video.yaml) | Sentence trigger | "Play ... on the TV" goes to the LLM, which calls `play_youtube` |
| [home-assistant/automations/motion_night_light.yaml](home-assistant/automations/motion_night_light.yaml) | Automation | Motion light in amber at night and white by day |
| [home-assistant/templates/night_light.yaml](home-assistant/templates/night_light.yaml) | Template sensor | "Night" from 23:00 until sunrise, between 06:00 and 08:00 |

To add a script, go to Settings > Automations & scenes > Scripts > Add script > Edit in YAML, and paste the file. Then expose the script to Assist (Settings > Voice assistants > Expose), so the LLM can call it. Automations work the same way.
