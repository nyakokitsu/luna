#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

// Station 3 exposes its 36 LED ring through sysfs (`orion:{red,green,blue}:ringN`).
// The 21x12 front panel matrix and the 12x2 ambilight are separate devices and
// are not modelled here yet, so no clock or ambilight capability is advertised.
const CAPABILITIES: &[&str] = &["led.ring.rgb", "led.ring.36", "led.pattern.v1"];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexstation_3",
        led_profile: "station3",
        led_count: 36,
        led_topology: LedTopology {
            ring_count: 36,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
