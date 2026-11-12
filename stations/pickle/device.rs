#[path = "clock.rs"]
mod clock;
#[path = "hardware.rs"]
mod hardware;
use luna::{InputAction, LedTopology, StationSpec};
// Retail .50 exposes two independent RGB zones; Mini 3 / bergamot is different.
const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.16",
    "led.logo",
    "led.logo.6",
    "led.pattern.v1",
    "display.clock.segment.v1",
    "microphone.capture",
];
const INPUT_ACTIONS: &[InputAction] = &[
    InputAction {
        name: "volume_up",
        device: "aw933xx",
        kind: 1,
        code: 115,
    },
    InputAction {
        name: "volume_down",
        device: "aw933xx",
        kind: 1,
        code: 114,
    },
    InputAction {
        name: "activate",
        device: "aw933xx",
        kind: 1,
        code: 582,
    },
    InputAction {
        name: "mute",
        device: "mute",
        kind: 1,
        code: 113,
    },
];
pub fn spec() -> StationSpec {
    StationSpec {
        // Retain the model ID already used by legacy .yap compatibility lists.
        model: "yandexmini_3_pro",
        led_profile: "pickle",
        led_count: 16,
        led_topology: LedTopology {
            ring_count: 16,
            has_logo: true,
            logo_count: 6,
        },
        color_order: luna::ColorOrder::Rgb,
        led_calibration: None,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || luna::create_quasar_microphone_backend_with_channels(3),
        create_clock_backend: clock::create_clock_backend,
        capabilities: CAPABILITIES,
        input_actions: INPUT_ACTIONS,
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
