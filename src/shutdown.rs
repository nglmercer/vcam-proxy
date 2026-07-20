//! Process-wide shutdown flag fed by SIGINT/SIGTERM/SIGHUP.
//!
//! All blocking points in worker threads are bounded (channel timeouts, poll
//! timeouts), so setting this flag guarantees every thread notices within a
//! few hundred milliseconds and unwinds through its normal cleanup path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn install() -> Self {
        let flag = Self::default();
        let f = flag.clone();
        ctrlc::set_handler(move || {
            f.request();
        })
        .expect("failed to install signal handler");
        flag
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
