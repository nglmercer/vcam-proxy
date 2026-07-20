//! Process-wide shutdown flag fed by SIGINT/SIGTERM/SIGHUP.
//!
//! All blocking points in worker threads are bounded (channel timeouts, poll
//! timeouts), so setting this flag guarantees every thread notices within a
//! few hundred milliseconds and unwinds through its normal cleanup path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Once;

static INSTALL: Once = Once::new();

#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    /// Install the signal handler. Safe to call multiple times — only the
    /// first call installs the handler; subsequent calls reuse the same flag.
    ///
    /// First Ctrl+C: sets the shutdown flag (graceful).
    /// Second Ctrl+C: exits immediately (user is insistent).
    pub fn install() -> Self {
        let flag = Self::default();
        let f = flag.clone();
        INSTALL.call_once(|| {
            let _ = ctrlc::set_handler(move || {
                static COUNT: AtomicBool = AtomicBool::new(false);
                // Second Ctrl+C → force exit immediately.
                if COUNT.swap(true, Ordering::SeqCst) {
                    std::process::exit(0);
                }
                f.request();
            });
        });
        flag
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
