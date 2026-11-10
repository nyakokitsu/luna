mod hardware;
use luna::{LedTopology, StationSpec};

pub fn preflight() -> anyhow::Result<()> {
    hardware::preflight()
}

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexstation_max",
        led_profile: "stationmax",
        led_count: 24,
        led_topology: LedTopology { ring_count: 24, ..LedTopology::default() },
        color_order: luna::ColorOrder::Rgb,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: &["led.ring.rgb", "led.ring.24", "led.pattern.v1"],
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
