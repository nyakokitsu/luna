#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

// Station 2 has no /sys/class/leds ring: its 89-LED ring is an 89x1 XRGB
// framebuffer. The ring capabilities are intentionally not advertised, matching
// the hardware report the existing AirPlay supervisor uses to choose its own
// gated framebuffer painter. Everything else (audio, input, sensors) comes from
// the same auto-detection YAM uses, so the feature list stays identical.
const CAPABILITIES: &[&str] = &[];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexstation_2",
        led_profile: "station2",
        led_count: 89,
        led_topology: LedTopology {
            ring_count: 89,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
    }
}
