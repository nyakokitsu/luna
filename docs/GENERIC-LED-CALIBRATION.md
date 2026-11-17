# Generic Luna LED calibration

The standalone `/calibration` web page has been removed from Luna. Colour
controls belonging to a music plugin should live in that plugin's own panel,
not in a second built-in Luna tab. The old page URL is no longer registered.

This is a UI removal, not a reset of LED settings. Stock model profiles,
`led.calibration_gain`, logical RGB/channel order, runtime LED APIs and the
guarded LED-panel HTTP API below remain unchanged. Existing settings and
presets are preserved; no device files are removed by this change.

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
The standalone page and its browser-only mock fixture have been removed;
API/backend tests remain.

Removing this page does not fix AirPlay startup errors or change physical
colour calibration on any station.
