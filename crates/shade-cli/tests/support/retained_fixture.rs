use std::path::Path;

/// Preserve failed real-daemon fixtures so durable operations and diagnostics
/// remain available after the daemon/service guard has stopped its process.
pub struct RetainedFixture(tempfile::TempDir);

impl RetainedFixture {
    pub fn new(prefix: &str) -> Self {
        Self(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in("/private/tmp")
                .unwrap(),
        )
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }

    pub fn preserve(&mut self) {
        self.0.disable_cleanup(true);
        eprintln!("SHADE_ACCEPTANCE_ROOT {}", self.path().display());
    }
}

impl Drop for RetainedFixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.preserve();
        }
    }
}
