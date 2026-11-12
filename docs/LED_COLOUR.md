# LED colour contract

Luna accepts logical `#RRGGBB` colours from the runtime API and the Lua
`alice.led` helpers. A station crate owns the two hardware steps between that
contract and the LEDs:

1. `StationSpec::color_order` (`Rgb` or `Bgr`) packs logical channels in the
   order the driver expects.
2. `StationSpec::led_calibration` optionally names a stock profile in
   `/system/vendor/quasar/ledconfigs/<profile>.json`. Luna applies that 3×3
   matrix before `color_order` and preserves the source pixel peak so a loud
   band does not clip into a flat frame.

Both steps run for every LED frame, including playback, pause and plugin
`/led` calls. A missing or invalid stock file falls back to a built-in matrix
for profiles Luna knows; it never silently drops the correction for a model
that declares one.

## Capabilities for plugins

A station that depends on the new contract advertises:

- `led.calibration.v1` — Luna applies the stock matrix. A plugin must not
  apply the same matrix again.
- `led.color.logical-rgb.v1` — Luna emits logical RGB for this model. A plugin
  must not pre-swap R/B.

Older Luna builds do not advertise these features. Plugins that support both
should keep their workaround behind the capability check and use it only when
the feature is absent.

Mini 2 (`yandexmini_2`) is the first user of both capabilities: it declares
the stock `red` profile and `ColorOrder::Rgb`. Before this change the station
was packed as BGR, which forced plugins to swap R/B and apply the matrix
themselves.
