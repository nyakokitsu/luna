mod device;

fn main() -> anyhow::Result<()> {
    luna::run_with_station(Some(device::spec()))
}
