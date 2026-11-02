# Control an Android TV box without root

An Android TV or Google TV box is a good player for a voice assistant. Home Assistant can start apps on it, wake it, put it in standby and send keys. None of this needs root. This page explains the two ways in, what each one can do, and the traps.

The scripts [play_youtube.yaml](../examples/home-assistant/scripts/play_youtube.yaml) and [play_jellyfin.yaml](../examples/home-assistant/scripts/play_jellyfin.yaml) use this method. It works the same for any app that has a link or a package name.

## The two ways in

| Integration | Port | Use it to | Needs |
|---|---|---|---|
| Android TV Remote | 6466, 6467 | Wake the box, put it in standby, send keys, see the playing app | A PIN that the TV shows once |
| Android Debug Bridge (ADB) | 5555 | Start a named app with a link (`am start`), check installed apps (`pm path`), send keys (`input keyevent`), read the state (`dumpsys`) | Debugging on in the developer options, one approval on the TV |

ADB gives a shell with the user `shell` (uid 2000). This user is enough for `am start`, `pm path`, `input` and `dumpsys`. It cannot write to system files. So an action that needs root, such as the HDMI-CEC commands in `/sys/class/cec/cmd` of an Amlogic box, works only on boxes that give a root shell.

Use both integrations on one box if you want both abilities. The Android TV Remote wakes the box and puts it in standby. ADB opens the app.

## Turn on ADB on the box

1. Open Settings > System > About and press "Build" seven times. This turns on the developer options.
2. In the developer options, turn on "USB debugging". On Android 11 and later, also turn on "Wireless debugging" if the box offers it.
3. Check the port from another computer: `nc -vz <box address> 5555`. If the port is closed, start the listener from a computer that has a USB connection to the box: `adb tcpip 5555`. Test whether the port is open after a reboot of the box. On some boxes it closes at reboot.
4. Give the box a fixed address (a static lease in the router).

## Approve the key of Home Assistant

An ADB connection needs an approved RSA key. Home Assistant has its own key. A key that your computer approved does not help.

1. Turn on the TV and wake the box.
2. In Home Assistant, add the integration "Android Debug Bridge". Enter the address of the box, port 5555 and the device class "Android TV".
3. The TV shows "Allow USB debugging?". Choose "Always allow".

If the integration cannot connect, the box did not get the key approved yet. From a computer, `adb connect <address>:5555` then answers "failed to authenticate" and `adb devices` lists the box as `unauthorized`. The box shows the prompt only while it is awake.

Some boxes have `ro.adb.secure=0`. They give a shell to every host on the network without a key. Treat such a box as open to anyone on your LAN.

## Wake and standby

A box that sends HDMI-CEC wakes the TV when it wakes, and it puts the TV in standby when it goes to standby. This works with the "Android TV Remote" entity:

```yaml
- action: remote.turn_on
  target:
    entity_id: remote.bedroom_player
- action: remote.turn_off
  target:
    entity_id: remote.bedroom_player
```

If an AV receiver sits between the box and the TV, turn the receiver on first and choose its input. The box does not do this.

## Start an app with a link

`am start` opens a link in a chosen app. The flag `-f 0x10000000` starts a new task, so it works from a shell. Try the apps in a list, and use the first one that is installed:

```sh
u='https://www.youtube.com/watch?v=VIDEO_ID'
for p in io.gh.reisxd.tizentube.cobalt org.smarttube.stable com.google.android.youtube.tv; do
  if pm path $p >/dev/null 2>&1; then
    am start -f 0x10000000 -a android.intent.action.VIEW -d "$u" -p $p
    break
  fi
done
```

`-p <package>` makes the choice. Without it, the box may open the stock YouTube app, or it may ask the user. The reply "Warning: Activity not started, intent has been delivered to currently running top-most instance" is normal when the app already runs. The video plays.

Other apps have their own links. The Jellyfin client Wholphin plays an item with `wholphin://play?itemId=<id>&position=<ms>&userId=<id>&serverId=<id>`. The official Jellyfin app opens the item with `am start -n org.jellyfin.androidtv/.ui.startup.StartupActivity -a android.intent.action.VIEW -d <item id>`, and a play key (`input keyevent KEYCODE_MEDIA_PLAY`) starts it.

To see which apps a box has, run `pm list packages | grep -i <name>`. To see the app in front, run `dumpsys window | grep mCurrentFocus`.

## Run the commands from Home Assistant

The action `androidtv.adb_command` runs a shell command. The reply is in the attribute `adb_response` of the media player. A template in `entity_id` lets one script serve several rooms:

```yaml
variables:
  player: "{{ 'media_player.bedroom_adb' if room == 'bedroom' else 'media_player.living_room_adb' }}"
...
- action: androidtv.adb_command
  data:
    entity_id: "{{ player }}"
    command: ...
```

A box in standby needs time to answer. Wait for it before the first command:

```yaml
- wait_template: "{{ states('media_player.bedroom_adb') not in ['unavailable', 'unknown', 'off'] }}"
  timeout: 00:00:20
  continue_on_timeout: true
```

## Traps

1. A new entity is exposed to Assist by default. The ADB player is no tool for the agent. Turn its exposure off, because each exposed entity adds tokens to every request.
2. Give the ADB entities their own names (`media_player.bedroom_adb`). The Android TV Remote entities of the same box use similar default names.
3. Wake a box in standby with the Android TV Remote first. Then wait for the ADB entity, as in the example above.

## Tested

| Box | Android | Result |
|---|---|---|
| Nokia Streaming Box 8010 | 14 | ADB with an approved key. Wake and standby with Android TV Remote. TizenTube, SmartTube, Wholphin and the Jellyfin app start with `am start`. No root. |
| Ugoos AM6B+ | 9 | ADB without a key (`ro.adb.secure=0`) with a root shell. Same app starts. HDMI-CEC from the box through `/sys/class/cec/cmd`. |
