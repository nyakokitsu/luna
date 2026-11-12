#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

// Station Street (mango) exposes four RGB ring LEDs and three RGB logo LEDs
// through sysfs. Both zones carry a firmware trigger (`ledfb-ring` /
// `ledfb-logo`), so they are only advertised together with the backend that
// detaches and restores those triggers.
const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.4",
    "led.logo",
    "led.logo.3",
    "led.pattern.v1",
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "mango",
        led_profile: "mango",
        led_count: 4,
        led_topology: LedTopology {
            ring_count: 4,
            has_logo: true,
            logo_count: 3,
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
