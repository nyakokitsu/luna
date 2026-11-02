mod device;

fn main() -> anyhow::Result<()> {
    if std::env::var_os("LUNA_AUDIO_PCM").is_none() {
        std::env::set_var("LUNA_AUDIO_PCM", "dmix_spk_plug");
    }
    luna::run_with_station(Some(device::spec()))
}
