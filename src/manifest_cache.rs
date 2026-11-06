//! Reuse TOML deserialization, never permission decisions. Every lookup still
//! reads the entire current descriptor and checks its SHA256. Any content change,
//! read error, UTF-8 error or malformed manifest fails/reparses as before.
use super::*;

const MAX_ENTRIES: usize = 16;
const MAX_CACHED_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub(super) struct ManifestCache {
    entries: std::sync::Mutex<HashMap<PathBuf, ([u8; 32], Manifest)>>,
}

impl ManifestCache {
    pub(super) fn read(&self, path: &Path) -> Result<Manifest> {
        // Do not trust timestamps or return a stale grant after an unreadable
        // or removed descriptor. Read and hash even on a cache hit.
        self.parse(path, fs::read_to_string(path)?)
    }

    /// Permission check for the hot scoped-API path. Cloning the full manifest
    /// (thousands of phrases) only to test one permission is wasted work.
    pub(super) fn permissions(&self, path: &Path) -> Result<Vec<String>> {
        let text = fs::read_to_string(path)?;
        self.with_manifest(path, text, |manifest| manifest.permissions.clone())
    }

    /// Parse text the caller already read. The digest still covers the full
    /// descriptor, so a changed file re-parses and malformed text still fails.
    pub(super) fn parse(&self, path: &Path, text: String) -> Result<Manifest> {
        self.with_manifest(path, text, Manifest::clone)
    }

    fn with_manifest<T>(
        &self,
        path: &Path,
        text: String,
        use_manifest: impl FnOnce(&Manifest) -> T,
    ) -> Result<T> {
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if text.len() > MAX_CACHED_BYTES {
            // Preserve validation for larger descriptors without retaining an
            // arbitrarily large parsed manifest on a memory-constrained host.
            let manifest: Manifest = toml::from_str(&text)?;
            return Ok(use_manifest(&manifest));
        }
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow!("manifest cache poisoned"))?;
        if let Some((old, manifest)) = entries.get(path) {
            if *old == digest {
                return Ok(use_manifest(manifest));
            }
        }
        // Parse the FULL Manifest, not just permissions: malformed unrelated
        // fields must continue to reject a runtime request exactly as before.
        let manifest: Manifest = toml::from_str(&text)?;
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(path) {
            entries.clear();
        }
        entries.insert(path.to_path_buf(), (digest, manifest));
        Ok(use_manifest(&entries.get(path).expect("just inserted").1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        path: PathBuf,
        text: String,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("luna-manifest-cache-{}", Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("plugin.toml");
            let text = "id='fixture'\nname='Fixture'\nversion='1.0.0'\nentry='run.sh'\npermissions=['exec']\n".to_owned();
            fs::write(&path, &text).unwrap();
            Self { dir, path, text }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn unchanged_descriptor_matches_full_parser() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        let direct: Manifest = toml::from_str(&f.text).unwrap();
        for _ in 0..3 {
            let cached = cache.read(&f.path).unwrap();
            assert_eq!(
                serde_json::to_value(&cached).unwrap(),
                serde_json::to_value(&direct).unwrap()
            );
        }
        assert_eq!(cache.entries.lock().unwrap().len(), 1);
    }

    #[test]
    fn same_length_permission_change_is_immediately_observed() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        assert_eq!(cache.read(&f.path).unwrap().permissions, vec!["exec"]);
        let changed = f.text.replace("'exec'", "'none'");
        assert_eq!(changed.len(), f.text.len());
        fs::write(&f.path, changed).unwrap();
        assert_eq!(cache.read(&f.path).unwrap().permissions, vec!["none"]);
    }

    #[test]
    fn malformed_unrelated_field_does_not_reuse_old_permissions() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        cache.read(&f.path).unwrap();
        fs::write(&f.path, format!("{}\n[launch]\nargs=42\n", f.text)).unwrap();
        assert!(cache.read(&f.path).is_err());
        fs::write(&f.path, "not valid TOML = ]").unwrap();
        assert!(cache.read(&f.path).is_err());
        fs::write(&f.path, [0xffu8]).unwrap();
        assert!(cache.read(&f.path).is_err());
    }

    #[test]
    fn permission_lookup_matches_full_manifest_and_observes_changes() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        assert_eq!(
            cache.permissions(&f.path).unwrap(),
            cache.read(&f.path).unwrap().permissions
        );
        let changed = f.text.replace("'exec'", "'none'");
        assert_eq!(changed.len(), f.text.len());
        fs::write(&f.path, changed).unwrap();
        assert_eq!(cache.permissions(&f.path).unwrap(), vec!["none"]);
        fs::write(&f.path, "not valid TOML = ]").unwrap();
        assert!(cache.permissions(&f.path).is_err());
        fs::remove_file(&f.path).unwrap();
        assert!(cache.permissions(&f.path).is_err());
    }

    #[test]
    fn already_read_text_uses_the_same_freshness_rules() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        assert_eq!(cache.parse(&f.path, f.text.clone()).unwrap().permissions, vec!["exec"]);
        // A caller that already read a different file content must not receive
        // the parsed result for the old content, even at the same path.
        let changed = f.text.replace("'exec'", "'none'");
        assert_eq!(changed.len(), f.text.len());
        assert_eq!(cache.parse(&f.path, changed).unwrap().permissions, vec!["none"]);
        // Malformed text still fails after a successful parse.
        assert!(cache.parse(&f.path, "not valid TOML = ]".to_owned()).is_err());
    }

    #[test]
    fn oversized_descriptor_is_validated_but_not_retained() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        let large = format!("{}\n#{}\n", f.text, "x".repeat(MAX_CACHED_BYTES));
        fs::write(&f.path, large).unwrap();
        assert_eq!(cache.read(&f.path).unwrap().permissions, vec!["exec"]);
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    #[test]
    fn removed_descriptor_never_returns_cached_grant() {
        let f = Fixture::new();
        let cache = ManifestCache::default();
        cache.read(&f.path).unwrap();
        fs::remove_file(&f.path).unwrap();
        assert!(cache.read(&f.path).is_err());
    }

    #[test]
    #[ignore = "explicit offline performance audit using an existing packaged descriptor"]
    fn packaged_manifest_read_cost() {
        let path = PathBuf::from(
            std::env::var_os("LUNA_AUDIT_MANIFEST").expect("set LUNA_AUDIT_MANIFEST"),
        );
        let text = fs::read_to_string(&path).unwrap();
        let cache = ManifestCache::default();
        let expected = cache.read(&path).unwrap();
        let start = Instant::now();
        for _ in 0..100 {
            let m: Manifest = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            std::hint::black_box(m);
        }
        let direct = start.elapsed();
        let start = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(cache.read(&path).unwrap());
        }
        println!(
            "MANIFEST_READ_AUDIT bytes={} phrases={} direct_100_us={} hashed_cache_100_us={}",
            text.len(),
            expected.commands.phrases.len(),
            direct.as_micros(),
            start.elapsed().as_micros()
        );
        let start = Instant::now();
        for _ in 0..100 {
            std::hint::black_box(cache.permissions(&path).unwrap());
        }
        println!(
            "PERMISSIONS_READ_AUDIT 100_us={}",
            start.elapsed().as_micros()
        );
    }
}
