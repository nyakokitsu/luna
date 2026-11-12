#[path = "hardware.rs"]
mod hardware;

use luna::{LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.6",
    "led.pattern.v1",
    // Luna applies the stock `red` calibration and emits logical RGB. A
    // plugin can use these capabilities to stop pre-swapping or double-applying
    // the matrix.
    "led.calibration.v1",
    "led.color.logical-rgb.v1",
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexmini_2",
        led_profile: "mini2",
        led_count: 6,
        led_topology: LedTopology {
            ring_count: 6,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Rgb,
        led_calibration: Some("red"),
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: || None,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mini2_owns_logical_rgb_and_stock_calibration() {
        let spec = spec();
        assert_eq!(spec.color_order, luna::ColorOrder::Rgb);
        assert_eq!(spec.led_calibration, Some("red"));
        for capability in ["led.calibration.v1", "led.color.logical-rgb.v1"] {
            assert!(spec.capabilities.contains(&capability), "{capability}");
        }
    }
}
