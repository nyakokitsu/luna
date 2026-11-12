#[path = "hardware.rs"]
mod hardware;

use luna::{GpioInput, LedTopology, StationSpec};

const CAPABILITIES: &[&str] = &[
    "led.logo.rgb",
    "led.logo.6",
    "led.pattern.v1",
    "microphone.capture",
];

const GPIO_INPUTS: &[GpioInput] = &[
    GpioInput {
        name: "mute",
        path: "/sys/class/gpio/gpio414/value",
        active_level: 0,
        code: 113,
        action: "mute",
    },
    GpioInput {
        name: "unmute",
        path: "/sys/class/gpio/gpio505/value",
        active_level: 1,
        code: 113,
        action: "unmute",
    },
];

pub fn spec() -> StationSpec {
    StationSpec {
        model: "yandexmini",
        led_profile: "mini1",
        led_count: 6,
        led_topology: LedTopology {
            has_logo: true,
            logo_count: 6,
            ..LedTopology::default()
        },
        color_order: luna::ColorOrder::Bgr,
        led_calibration: None,
        create_led_backend: hardware::create_led_backend,
        create_microphone_backend: luna::create_quasar_microphone_backend,
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: &[],
        input_gestures: &[],
        gpio_inputs: GPIO_INPUTS,
    }
}
