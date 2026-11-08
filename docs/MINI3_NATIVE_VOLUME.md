# Mini 3 native volume qualification

## Problem and change

On the tested Mini 3, Luna fell back to ALSA because its firmware was absent
from the native-volume allowlist. ALSA writes changed playback loudness without
updating Quasar's logical volume, so the first hardware volume-button press
could jump to Quasar's retained level.

Qualify only firmware `0.369.1.50.4794652521.20261021.117` with the exact
NUL-separated device-tree entry `yandex,bergamot`. Similar board names and
different firmware remain rejected. The existing protocol is unchanged:
read logical state from `stereo_pair`, send VolumeSet through
`iohub_clients`, and wait for the logical-state acknowledgement.

## Validation on 7 November 2026

Device: Mini 3, model `yandexmini3`, DT entries
`yandex,bergamot-evt1`, `yandex,bergamot`, and `amlogic, axg`.
Configured IPC ports were 9921 for state and 9910 for control.
The older `volume_manager` port 9922 had no listener.

Before the change, Luna reported ALSA's 13% while the independent Quasar
state stream reported 5%. The existing native protocol successfully
acknowledged a direct 5 → 0 → 5% round trip.

The regression test was observed failing without the qualification and passing
with it. `cargo test --workspace --locked` passed 136 tests with one existing
ignored performance audit, `packaged_manifest_read_cost`.

Built `cargo build --release --locked -p luna-yandexmini3` with Rust 1.99.1
in an AArch64 Alpine Podman container. Use the station-specific binary:
the generic executable does not identify this board automatically.

After installation, independent state-stream reads matched both API responses
and subsequent API GETs:

| Requested | API set | API GET | Quasar logical |
|---|---|---|---|
| 5% | 5% | 5% | 5% |
| 0% | 0% | 0% | 0% |
| 5% | 5% | 5% | 5% |

AirPlay restarted with native controls enabled. Final level was 5%.
After installation the owner reported that everything appeared to work
normally. This overall acceptance does not establish a separately recorded
first-button sequence or a cold-boot test.
Other firmware builds and sustained or high-volume operation were not tested.

The local binary was installed with a backup. Release auto-update was disabled
through `[updates] enabled = false` because it replaced the custom build;
this is a local validation prerequisite, not a change to update defaults.
The existing service initialization was preserved.

## Architecture and contribution scope

This qualifies an existing shared Quasar volume backend, rather than adding
an endpoint, capability, or station-specific hardware implementation.
The existing stations/yandexmini3 crate still provides model, LED, clock,
and input metadata. No model switch is added to src/gateway.rs.

EXTENDING_STATION.md requires using existing Quasar IPC for sound, and
DEVICE_GUIDE.md §9 explicitly says not to duplicate volume API or Quasar IPC
in station crates. Keeping firmware qualification beside the existing common
protocol follows that boundary; no new trait or copied station backend is
needed for this correction.

Discovery commands used on the station over SSH:

```sh
cat /system/vendor/quasar/version_full
tr '\000' '\n' < /proc/device-tree/compatible
wget -qO- http://127.0.0.1:8099/api/device
wget -qO- http://127.0.0.1:8099/api/volume
netstat -lnt
```

Observed firmware and DT entries are recorded above. Before and after the
change, /api/device reported model yandexmini3, architecture aarch64,
led.ring.rgb, led.ring.6, led.pattern.v1, display.clock.segment.v1 and ring
count 6. StationSpec features, HTTP volume paths and response shapes did
not change.

Local rollback: stop Luna, restore the saved original station binary and
settings, then start through the original service with its YAM arguments.
No service initialization or update-default change is part of this patch.
