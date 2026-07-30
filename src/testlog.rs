//! Test-only: capture `tracing` output so a *log line* can be asserted on.
//!
//! Some of this daemon's behaviour is only observable in its logs — the discovery retry
//! line's cause chain, the data-plane health line's counters. Those lines are contracts
//! with whoever reads the soak output, so they are tested like any other output rather
//! than eyeballed once and left to rot.
//!
//! [`tracing::subscriber::set_default`] is thread-local, so tests using this can run in
//! parallel without capturing each other's output.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// A shared byte buffer that doubles as a `tracing` writer.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Route every event at or below `level` into this buffer, for as long as the returned
    /// guard is alive.
    pub fn install(&self, level: tracing::Level) -> tracing::subscriber::DefaultGuard {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(self.clone())
            .with_ansi(false)
            .with_max_level(level)
            .finish();
        tracing::subscriber::set_default(subscriber)
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|p| p.into_inner())).to_string()
    }
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
    type Writer = LogCapture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
