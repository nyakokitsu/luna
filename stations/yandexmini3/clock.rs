#[path = "../common/segment_clock.rs"]
mod segment_clock;

pub fn create_clock_backend() -> Option<Box<dyn luna::ClockBackend>> {
    segment_clock::create_clock_backend(segment_clock::Layout::Mini3)
}
