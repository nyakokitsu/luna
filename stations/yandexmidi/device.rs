#[path = "../common/segment_clock.rs"]
mod segment_clock;

#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.24",
    "led.logo",
    "led.logo.6",
    "led.pattern.v1",
    "display.clock.segment.v1",
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexmidi",
        led_profile: "midi",
        led_count: 24,
        led_topology: LedTopology {
            ring_count: 24,
            has_logo: true,
            logo_count: 6,
        },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || segment_clock::create_clock_backend(segment_clock::Layout::Midi),
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
