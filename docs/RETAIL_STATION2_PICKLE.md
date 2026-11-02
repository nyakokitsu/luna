# Station 2 and Mini 3 Pro on retail .50

Tested on AArch64 retail firmware 0.369.1.50 (2026-10-21 builds), without flashing or rebooting. Use the dedicated `luna-yandexstation_2` and `luna-pickle` executables. Mini 3 Pro is **pickle**, not bergamot or cucumber; its canonical plugin model is `yandexmini_3_pro`.

| Function | Station 2 | Mini 3 Pro |
|---|---|---|
| Ring | fb0, 89x1, 32-bit BGRX | 16 RGB sysfs LEDs |
| Logo | none | 6 RGB sysfs LEDs |
| Clock | none | 36 sysfs segments |
| Buttons | midi_touch: 114/115/582; gpio-keys:113 | aw933xx:114/115/582; mute:113 |
| Shared microphone capture | yio_audio, four raw channels | yio_audio, three raw channels |
| Playback PCM | dmix_spk_plug | speakers |

The PCM default belongs to the station launcher and can be overridden with `LUNA_AUDIO_PCM`. Playback validates WAV input, waits for aplay completion and reports nonzero exit status; a bounded deadline terminates only its own child. Other profiles retain their existing playback route. No additional audio daemon or exclusive microphone capture is installed.

The microphone parser accepts both legacy direct IOAudioChannelsData and modern payload/type envelopes, with their different RAW enum values. Only raw microphone channels are captured; speaker and VQE streams are excluded. Requests use the shared yio_audio service, not an exclusive ALSA microphone device.

Pickle LED zones snapshot selected triggers and brightness, and restore them on release only while still owned. Clock release similarly preserves a newer writer. Station 2 keeps cooperative framebuffer ownership and encodes 32-bit pixels as BGRX; existing 24-bit behavior is retained.

## Read-only firmware and coexistence

Legacy YAM can already occupy port 8099. The installers now default to loopback port 8100 and allow an explicit listen override. For retail squashfs use `install.sh --no-boot-hook --listen 127.0.0.1:8100` or PowerShell `-NoBootHook -Listen 127.0.0.1:8100`. This explicit mode does not remount root or write /etc/init.d. Select the correct station asset and provide persistent startup through your existing /data owner bootstrap if desired. NoBootHook does not promise restart after reboot.

The profile is source support until a release includes its asset. Existing YAM postinstall boot hooks still require writable init paths; they are not a supported retail-squashfs persistence path. Generic launcher dispatch and OTA behavior are outside this change.

## Validation

Workspace tests (serial), installer detection and mocked read-only installation; static musl builds for both profiles. On-device LED channel/framebuffer readback, trigger restoration, clock release, evdev volume-up events and shared microphone PCM were checked. Playback through both documented PCMs was heard by the owner. Sensor, Bluetooth, voice recognition and reboot persistence have not been qualified by this matrix. HTTP success alone is not evidence of audible playback.
