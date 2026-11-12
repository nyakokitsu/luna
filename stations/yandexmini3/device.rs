#[path = "clock.rs"]
mod clock;
#[path = "hardware.rs"]
mod hardware;

use luna::{InputAction, InputGesture, LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.6",
    "led.pattern.v1",
    "display.clock.segment.v1",
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
        device: "gpio-keys",
        kind: 1,
        code: 113,
    },
];

const INPUT_GESTURES: &[InputGesture] = &[
    InputGesture {
        name: "minus-to-logo",
        actions: &["volume_down", "activate"],
        max_gap_ms: 300,
    },
    InputGesture {
        name: "plus-to-logo",
        actions: &["volume_up", "activate"],
        max_gap_ms: 300,
    },
    InputGesture {
        name: "minus-to-plus",
        actions: &["volume_down", "activate", "volume_up"],
        max_gap_ms: 300,
    },
    InputGesture {
        name: "plus-to-minus",
        actions: &["volume_up", "activate", "volume_down"],
        max_gap_ms: 300,
    },
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexmini3",
        led_profile: "mini3",
        led_count: 6,
        led_topology: LedTopology {
            ring_count: 6,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Bgr,
        led_calibration: None,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: clock::create_clock_backend,
        capabilities: CAPABILITIES,
        input_actions: INPUT_ACTIONS,
        input_gestures: INPUT_GESTURES,
        gpio_inputs: &[],
    }
}
