#[path = "hardware.rs"]
mod hardware;
#[path = "clock.rs"]
mod clock;
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
        // Owner-observed on Max: bytes in fb5 are interpreted as B, G, R.
        color_order: luna::ColorOrder::Bgr,
		led_calibration: None,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: clock::create_clock_backend,
        capabilities: &["led.ring.rgb", "led.ring.24", "led.pattern.v1"],
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ring_uses_owner_observed_bgr_order() {
        assert!(matches!(super::spec().color_order, luna::ColorOrder::Bgr));
    }
}
