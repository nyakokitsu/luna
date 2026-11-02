#[path = "clock.rs"]
mod clock;
#[path = "hardware.rs"]
mod hardware;

use luna::{InputAction, InputGesture, LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.3",
    "led.pattern.v1",
    "display.clock.segment.v1",
    "microphone.capture",
];

// The three capacitive zones sit on the top panel and are wired to the
// AW933xx touch controller, which registers its evdev device under the name of
// the sensor chip (`aw9610x`). The mute key and the microphone mute switch are
// separate gpio-keys lines.
const INPUT_ACTIONS: &[InputAction] = &[
    InputAction {
        name: "volume_up",
        device: "aw9610x",
        kind: 1,
        code: 115,
    },
    InputAction {
        name: "volume_down",
        device: "aw9610x",
        kind: 1,
        code: 114,
    },
    InputAction {
        name: "activate",
        device: "aw9610x",
        kind: 1,
        code: 582,
    },
    InputAction {
        name: "mute",
        device: "gpio-keys",
        kind: 1,
        code: 113,
    },
    InputAction {
        name: "mute_switch",
        device: "gpio-keys",
        kind: 5,
        code: 14,
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
        model: "yandexplum",
        led_profile: "plum",
        led_count: 3,
        led_topology: LedTopology {
            ring_count: 3,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: luna::create_quasar_microphone_backend,
        create_clock_backend: clock::create_clock_backend,
        capabilities: CAPABILITIES,
        input_actions: INPUT_ACTIONS,
        input_gestures: INPUT_GESTURES,
        gpio_inputs: &[],
    }
}
