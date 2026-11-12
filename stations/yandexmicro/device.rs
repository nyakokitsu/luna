#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &["led.ring.rgb", "led.ring.3", "led.pattern.v1"];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexmicro",
        led_profile: "micro",
        led_count: 3,
        led_topology: LedTopology {
            ring_count: 3,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Rgb,
        led_calibration: None,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
