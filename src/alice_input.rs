//! Narrow runtime access to KEY_ALICE. Other input events never leave Luna.
use super::*;
use std::os::unix::io::AsRawFd;
const KEY_ALICE: u16 = 582;

fn has_alice_key(file: &fs::File) -> bool {
    let mut bits = [0u8;96];
    let request: libc::c_ulong = (2 << 30) | (96 << 16) | ((b'E' as libc::c_ulong) << 8) | 0x21;
    // musl exposes an int request while glibc uses unsigned long. Preserve
    // the ioctl bit pattern rather than fallibly converting its sign bit.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, bits.as_mut_ptr()) };
    rc >= 0 && has_key(&bits, KEY_ALICE)
}
fn has_key(bits: &[u8], key: u16) -> bool {
    bits.get(key as usize / 8).is_some_and(|v| v & (1 << (key % 8)) != 0)
}
fn discover() -> Result<(String,PathBuf)> {
    let listing = fs::read_to_string("/proc/bus/input/devices")?;
    let found: Vec<_> = list_input_devices(&listing).into_iter().filter(|(_,path)| {
        fs::File::open(path).is_ok_and(|f| has_alice_key(&f))
    }).collect();
    if found.len()!=1 { bail!("expected one KEY_ALICE input device, found {}",found.len()); }
    Ok(found.into_iter().next().unwrap())
}
pub fn available() -> bool { discover().is_ok() }
pub struct AliceReader {
    input: Option<InputReader>,
    touched: Instant,
}
impl Default for AliceReader {
    fn default() -> Self { Self{input:None,touched:Instant::now()} }
}
impl AliceReader {
    fn next(&mut self) -> Result<Option<InputEvent>> {
        // A resumed caller must not receive an old press from a previous window.
        if self.touched.elapsed()>Duration::from_secs(3) { self.input=None; }
        self.touched=Instant::now();
        if self.input.is_none() { self.input=Some(InputReader::open(&[discover()?])?); }
        let deadline=Instant::now()+Duration::from_millis(800);
        loop {
            let left=deadline.saturating_duration_since(Instant::now());
            if left.is_zero() { return Ok(None); }
            let event=self.input.as_mut().unwrap().next(left.as_millis() as u64,&AtomicBool::new(false));
            match event {
                Ok(Some(event)) if event.kind==1 && event.code==KEY_ALICE => return Ok(Some(event)),
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(None),
                Err(e) => { self.input=None; return Err(e); }
            }
        }
    }
}
pub async fn next(
    State(a): State<App>, AxumPath(id): AxumPath<String>, headers: HeaderMap,
) -> Result<Json<Option<InputEventInfo>>,ApiError> {
    runtime_permission(&a,&id,&headers,"input.alice").await?;
    let token=runtime_token(&headers)?.to_owned();
    let reader={
        let mut readers=a.alice_readers.lock().await;
        let entry=readers.entry(id.clone()).or_insert_with(|| (token.clone(),Arc::new(std::sync::Mutex::new(AliceReader::default()))));
        if entry.0!=token { *entry=(token.clone(),Arc::new(std::sync::Mutex::new(AliceReader::default()))); }
        entry.1.clone()
    };
    let event=task::spawn_blocking(move || {
        reader.try_lock().map_err(|_| anyhow!("Alice input poll already in flight"))?.next()
    }).await??;
    // A stopped or restarted invocation must not receive the old reader's event.
    runtime_permission(&a,&id,&headers,"input.alice").await?;
    Ok(Json(event.map(|e|input_event_info(a.input_actions,e))))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_alice_key_bit_authorizes_device() {
        let mut bits=[0u8;96];
        assert!(!has_key(&bits,KEY_ALICE));
        bits[KEY_ALICE as usize/8]=1<<(KEY_ALICE%8);
        assert!(has_key(&bits,KEY_ALICE));
        assert!(!has_key(&bits,KEY_ALICE-1));
        assert!(!has_key(&[],KEY_ALICE));
    }
}
