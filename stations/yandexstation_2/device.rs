#[path = "hardware.rs"]
mod hardware;

use luna::{InputAction, LedTopology, StationSpec};

// Station 2 has no sysfs LED ring. Its 89 LEDs are a framebuffer, so the
// framebuffer backend is the station-specific capability implementation. The
// device framebuffer is 32 bpp while the emulator exposes 24 bpp; the backend
// follows bits_per_pixel.
const CAPABILITIES: &[&str] = &[
    "led.ring.rgb",
    "led.ring.89",
    "led.pattern.v1",
    "microphone.capture",
];

const INPUT_ACTIONS: &[InputAction] = &[
    InputAction {
        name: "volume_up",
        device: "midi_touch",
        kind: 1,
        code: 115,
    },
    InputAction {
        name: "volume_down",
        device: "midi_touch",
        kind: 1,
        code: 114,
    },
    InputAction {
        name: "activate",
        device: "midi_touch",
        kind: 1,
        code: 582,
    },
    InputAction {
        name: "mute",
        device: "gpio-keys",
        kind: 1,
        code: 113,
    },
];

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
        create_microphone_backend: || luna::create_quasar_microphone_backend_with_channels(4),
        create_clock_backend: || None,
        capabilities: CAPABILITIES,
        input_actions: INPUT_ACTIONS,
        input_gestures: &[],
        gpio_inputs: &[],
    }
}
