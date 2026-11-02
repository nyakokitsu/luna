mod device;
fn main() -> anyhow::Result<()> {
    if std::env::var_os("LUNA_AUDIO_PCM").is_none() {
        std::env::set_var("LUNA_AUDIO_PCM", "speakers");
    }
    luna::run_with_station(Some(device::spec()))
}
