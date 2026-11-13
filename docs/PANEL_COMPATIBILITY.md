# Calibration panel: model-independent discovery

The panel uses the local Luna backend's topology, not private speaker addresses
or a browser-side list of supported models. If AirPlay is absent or has no
available LED output, the page automatically opens the native Luna scope.
No preview is started automatically.

## Profile reference

Counts/order are taken from `stations/*/device.rs`. The ring and logo are
separate outputs; the page sends exactly the selected output's frame length.
RGB/BGR packing and stock calibration remain in the backend, never in JS.

| Profile | Ring | Logo | Backend order |
|---|---:|---:|---|
| Mini / yandexmini | — | 6 | BGR |
| Mini 2 / yandexmini_2 | 6 | — | RGB + stock calibration |
| Mini 3 / bergamot / yandexmini3 | 6 | — | BGR |
| Lite / yandexmicro | 3 | — | RGB |
| Lite 2 / plum / yandexplum (clock and no-clock) | 3 | — | RGB |
| Midi / cucumber / yandexmidi | 24 | 6 | RGB |
| Street / mango | 4 | 3 | RGB |
| Mini 3 Pro / pickle | 16 | 6 | RGB |
| Station 3 / orion | 36 | — | RGB |
| Max / yandexstation_max | 24 | — | BGR, experimental framebuffer backend |
| Station 2 | 89 | included in framebuffer | qualified AirPlay RGBW preview |

Station 2's native Luna RGBX implementation is not assumed safe for independent
RGBW calibration. It explicitly advertises a reason to use the AirPlay backend.
No framebuffer layout, maind ownership or channel packing was guessed/changed
to make a support checkbox green. Other framebuffer backends retain their
geometry and foreign-frame checks; a busy/error backend is not forced.

An unlisted future board works when its backend advertises valid topology.
With no qualified backend, the panel explains the limitation and disables
preview. That is not support for arbitrary brands/firmware without a Luna API.

Native gains currently apply to all Luna LED outputs, not separately per zone,
and do not recalibrate maind itself. Native preview still requires both Alice
and volume observers; without them it fails closed.

The standalone Luna preview does not enable AirPlay artwork on models where
the AirPlay profile deliberately disables it (notably orion).

## Checks

- `cargo test --workspace -- --test-threads=1`: all 11 station wrappers build;
  the core plus station hardware/clock fixtures pass (332 passed, 1 ignored).
  Max needed its missing `led_calibration: None` initializer restored.
- AirPlay Go race/vet pass, including backend-derived reset defaults.
- Browser fixtures: logo-only Mini; separate ring/logo Midi, pickle and mango;
  orion, Max, Station 2, an unlisted 12+2 board, and a board without LED output.
- Fixture logo requests contain `zone: "logo"` and the exact logo count.

These are software/fixture checks, not new physical or emulator fleet tests.
Existing old emulator tables (e.g. Station 2 = 88 LEDs) are historical; the
current source profile uses 89. Physical colour/ownership must still be checked
on each firmware family before a release is described as hardware-verified.

Changes are local source changes; deployed dev.1 packages and archived builds
do not receive them until rebuilt and explicitly installed. The release keeps
fast_audio / -1.7 as the default on every model.
