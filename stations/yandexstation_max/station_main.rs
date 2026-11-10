mod device;

fn main() -> anyhow::Result<()> {
    // Reject a different device before the shared Luna lock can replace an
    // existing instance. This is a hardware signature, not full qualification.
    device::preflight()?;
    luna::run_with_station(Some(device::spec()))
}
