# Generic Luna LED calibration

`/calibration` is a native Luna LED tool, independent of any music plugin.
It uses Luna's existing web listener and browser guard; mutations additionally
require `X-Luna-Local: 1`. There is no shell execution, extra listener, token
extraction, plugin command transport, timing control or EQ.

## Contract

- `GET /api/led-panel/status`: backend-derived ring/logo topology,
  availability reason, defaults and preview ownership.
- `GET/PUT /api/led-panel/settings`: `gain_r`, `gain_g`, `gain_b`, each 0.05..4.
  Stored atomically as `led.calibration_gain` in Luna settings.
- `POST/DELETE /api/led-panel/preview`: a four-second renewable session lease.
  Frames must contain exactly the selected zone's LED count in `#RRGGBB`.
- `GET/PUT /api/led-panel/presets`: at most 16 named RGB-gain objects, 16 KiB.
  Stored separately in `led-calibration-presets.json`; no paths/commands/audio
  configuration are accepted.

The browser supports solid/RGB/white/grey/off patterns, HEX, ring/logo selection,
gain preview, explicit save/reload/reset, named presets and JSON import/export.
It never starts preview automatically. Loading/importing a preset changes
only the form; explicit Save is required to persist gains.

## Ownership and limitations

The lease temporarily suppresses other Luna LED/off writes. A second session
cannot renew or stop it. It draws only while BOTH assistant-idle and volume
observers are fresh; missing state fails closed. Speech and volume changes
yield output. Stop/expiry/shutdown restore the backend trigger without painting
a blank over native indication. Losing the tab/renewal expires safely.

RGB gains apply globally to Luna LED clients, not native maind or independent
hardware writers. Those writers are NOT coordinated by this lease. Avoid
concurrent independent LED tests. Client-side correction can compound with
Luna gains. Preview applies form gains once; backend stock matrix and RGB/BGR
packing are unchanged.

Topologies come from backend capabilities, not a browser model whitelist.
Logo-only and separate ring/logo configurations are supported. Station 2's
native framebuffer ownership remains unqualified and explicitly disables
independent preview; no layout or ownership is guessed to claim support.
The missing Max `StationSpec.led_calibration` initializer is supplied so that
all station wrappers can compile with the existing interface.

## Qualification

Rust checks cover gain/preset HTTP validation, persistence without changing
binary-execution opt-in, body limits, session ownership/frame validation,
topology discovery, single gain application and fail-closed observer gating.
Browser fixtures are software tests, not proof of physical colour accuracy.

This isolated replacement has not been deployed to hardware. Earlier combined
panel experiments are not qualification of this new standalone page.
Physical colour accuracy, assistant/volume handover and backend restoration
across firmware families still require qualification; keep the PR draft.
The existing upstream Lite2 firmware assertion is unrelated and must be
reported separately rather than hidden as a passing full suite.
